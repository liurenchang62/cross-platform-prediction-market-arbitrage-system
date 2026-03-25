//! 模拟交易与持仓跟踪：同一「市场对 + PM 侧 + Kalshi 侧」未平仓不加仓；平仓后该腿冷却若干周期。
//! 相反腿（如 PM YES+KS NO 与 PM NO+KS YES）键不同，可同时持仓。
//!
//! 开关见 `system_params`：`PAPER_TRADING_ENABLED`、`PAPER_WRITE_TRADE_LOG`；
//! 每个监控周期**开始**对全部持仓拉双边订单簿，在 bid 上按持仓份数真实吃单，计平仓手续费与 gas，并写一条 `NO_CLOSE` 或 `CLOSE` 记录。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;

use crate::arbitrage_detector::{
    orderbook_best_ask_price, ArbitrageDetector, ArbitrageOpportunity, Fees, GAS_FEE_PER_TX,
    PairOrderbookLadders, proceeds_for_exact_contracts_sell,
};
use crate::system_params::{
    paper_settlement_fee_estimate, PAPER_COOLDOWN_CYCLES, PAPER_INITIAL_CASH, PAPER_MAX_OPEN_POSITIONS,
    PAPER_MIN_EDGE_EARLY_USD, PAPER_RUN_LABEL_ENV, PAPER_SESSION_COUNTER_FILE, PAPER_TRADES_CSV,
    PAPER_TRADING_ENABLED, PAPER_WRITE_TRADE_LOG,
};

#[derive(Debug, Clone)]
pub struct PaperPosition {
    pub trade_id: String,
    pub pair_label: String,
    pub opened_cycle: usize,
    /// 模拟开仓时刻（UTC，RFC3339）
    pub simulated_open_time_utc: String,
    pub pm_market_id: String,
    pub kalshi_market_id: String,
    pub pm_token_id: String,
    pub n: f64,
    pub pm_side: String,
    pub kalshi_side: String,
    pub entry_capital_used: f64,
    pub pm_entry_avg: f64,
    pub ks_entry_avg: f64,
    pub fees_open: f64,
    pub gas_open: f64,
}

pub struct PaperEngine {
    pub cash: f64,
    pub initial_cash: f64,
    /// 每次进程启动递增，用于区分中断重启后的测试轮次
    pub session_id: u64,
    /// 是否写入单一交易 CSV（见 `PAPER_WRITE_TRADE_LOG`）
    pub write_trade_log: bool,
    /// 本会话内已见最大监控周期号（用于 `SESSION_END`）
    max_cycle_seen: usize,
    /// 避免重复写 `SESSION_END`（含 Drop）
    session_end_logged: bool,
    /// 进程启动墙钟（UTC），写入 `SESSION_START`
    session_wall_started: DateTime<Utc>,
    positions: HashMap<String, PaperPosition>,
    cooldown_remaining: HashMap<String, u32>,
    fees: Fees,
}

impl PaperEngine {
    /// 由 `system_params::PAPER_TRADING_ENABLED` 控制是否构造引擎（`true` 为开启）。
    pub fn try_new() -> Result<Option<Self>> {
        if !PAPER_TRADING_ENABLED {
            return Ok(None);
        }

        std::fs::create_dir_all("logs").ok();

        let session_id = next_session_id()?;
        let write_trade_log = PAPER_WRITE_TRADE_LOG;

        let session_wall_started = Utc::now();
        let mut engine = Self {
            cash: PAPER_INITIAL_CASH,
            initial_cash: PAPER_INITIAL_CASH,
            session_id,
            write_trade_log,
            max_cycle_seen: 0,
            session_end_logged: false,
            session_wall_started,
            positions: HashMap::new(),
            cooldown_remaining: HashMap::new(),
            fees: Fees::default(),
        };

        if write_trade_log {
            engine.ensure_csv_header()?;
            engine.append_session_start()?;
        }

        println!(
            "   📒 [Paper] 模拟盘已启用 | session_id={} | 初始资金 ${:.2} | 冷却 {} 周期 | 写交易文件={}",
            session_id,
            PAPER_INITIAL_CASH,
            PAPER_COOLDOWN_CYCLES,
            write_trade_log
        );

        Ok(Some(engine))
    }

    fn ensure_csv_header(&self) -> Result<()> {
        let path = Path::new(PAPER_TRADES_CSV);
        if path.exists() {
            return Ok(());
        }
        let mut w = csv::Writer::from_path(PAPER_TRADES_CSV).context("create paper_trades.csv")?;
        w.write_record(PaperCsv::HEADER)?;
        w.flush()?;
        Ok(())
    }

    /// 每个监控周期开始时调用：冷却倒计时。
    pub fn tick_cooldowns(&mut self) {
        let keys: Vec<String> = self.cooldown_remaining.keys().cloned().collect();
        for k in keys {
            if let Some(v) = self.cooldown_remaining.get_mut(&k) {
                *v = v.saturating_sub(1);
            }
            if self.cooldown_remaining.get(&k).copied().unwrap_or(0) == 0 {
                self.cooldown_remaining.remove(&k);
            }
        }
    }

    pub fn open_count(&self) -> usize {
        self.positions.len()
    }

    pub fn has_open(&self, pair_label: &str) -> bool {
        self.positions.contains_key(pair_label)
    }

    pub fn in_cooldown(&self, pair_label: &str) -> bool {
        self.cooldown_remaining
            .get(pair_label)
            .copied()
            .unwrap_or(0)
            > 0
    }

    /// 当前所有持仓快照（用于周期开始批量拉订单簿）。
    pub fn snapshot_open_positions(&self) -> Vec<PaperPosition> {
        self.positions.values().cloned().collect()
    }

    /// 周期开始：在已拉取的双边订单簿上评估提前平仓；写入 `CLOSE` 或 `NO_CLOSE`。
    pub fn check_early_close_at_cycle(
        &mut self,
        pair_label: &str,
        ladders: &PairOrderbookLadders,
        check_cycle: usize,
        check_time: DateTime<Utc>,
    ) -> Result<()> {
        let pos = match self.positions.get(pair_label) {
            Some(p) => p.clone(),
            None => return Ok(()),
        };

        let gas_close = GAS_FEE_PER_TX * 2.0;
        let total_open_outlay = pos.entry_capital_used + pos.fees_open + pos.gas_open;

        let notes_base = std::env::var(PAPER_RUN_LABEL_ENV).unwrap_or_default();

        let pm_fill = proceeds_for_exact_contracts_sell(&ladders.pm_bids_desc, pos.n);
        let ks_fill = proceeds_for_exact_contracts_sell(&ladders.ks_bids_desc, pos.n);

        let (no_close_reason, close_payload) = match (pm_fill, ks_fill) {
            (None, _) => ("insufficient_liquidity_pm_bids", None),
            (_, None) => ("insufficient_liquidity_kalshi_bids", None),
            (Some((proceeds_pm, pm_avg_sell)), Some((proceeds_ks, ks_avg_sell))) => {
                let fees_close =
                    proceeds_pm * self.fees.polymarket + proceeds_ks * self.fees.kalshi;
                let u_early_net = proceeds_pm + proceeds_ks - fees_close - gas_close;
                let fee_settle =
                    paper_settlement_fee_estimate(pos.n, self.fees.polymarket, self.fees.kalshi);
                let u_hold = pos.n - fee_settle;

                if u_early_net <= u_hold + PAPER_MIN_EDGE_EARLY_USD {
                    let diag = format!(
                        "u_early_net={:.4} u_hold={:.4} min_edge_usd={:.2}",
                        u_early_net, u_hold, PAPER_MIN_EDGE_EARLY_USD
                    );
                    let notes = if notes_base.is_empty() {
                        diag
                    } else {
                        format!("{} | {}", notes_base, diag)
                    };
                    self.append_no_close(
                        &pos,
                        check_cycle,
                        check_time,
                        "edge_below_threshold",
                        &notes,
                    )?;
                    return Ok(());
                }

                let pnl_realized = u_early_net - total_open_outlay;
                (
                    "",
                    Some((
                        proceeds_pm,
                        proceeds_ks,
                        pm_avg_sell,
                        ks_avg_sell,
                        fees_close,
                        gas_close,
                        u_early_net,
                        pnl_realized,
                    )),
                )
            }
        };

        if let Some((
            proceeds_pm,
            proceeds_ks,
            pm_avg_sell,
            ks_avg_sell,
            fees_close,
            gas_close_v,
            u_early_net,
            pnl_realized,
        )) = close_payload
        {
            self.cash += u_early_net;
            self.positions.remove(pair_label);
            self.cooldown_remaining
                .insert(pair_label.to_string(), PAPER_COOLDOWN_CYCLES);

            self.append_close(
                &pos,
                check_cycle,
                check_time,
                pm_avg_sell,
                ks_avg_sell,
                proceeds_pm + proceeds_ks,
                fees_close,
                gas_close_v,
                pnl_realized,
                &notes_base,
            )?;

            println!(
                "   📒 [Paper] 平仓 early_bid | {} | trade_id={} | pnl=${:.2} | cash=${:.2}",
                pair_label, pos.trade_id, pnl_realized, self.cash
            );
            return Ok(());
        }

        let notes = if notes_base.is_empty() {
            no_close_reason.to_string()
        } else {
            format!("{} | {}", notes_base, no_close_reason)
        };
        self.append_no_close(&pos, check_cycle, check_time, no_close_reason, &notes)?;
        Ok(())
    }

    /// 拉取订单簿失败时仍记一条 `NO_CLOSE`（本周期无法评估 bid）。
    pub fn log_no_close_book_error(
        &mut self,
        pos: &PaperPosition,
        check_cycle: usize,
        check_time: DateTime<Utc>,
        reason: &str,
    ) -> Result<()> {
        let notes = std::env::var(PAPER_RUN_LABEL_ENV).unwrap_or_default();
        let notes = if notes.is_empty() {
            reason.to_string()
        } else {
            format!("{} | {}", notes, reason)
        };
        self.append_no_close(pos, check_cycle, check_time, reason, &notes)
    }

    fn append_no_close(
        &mut self,
        pos: &PaperPosition,
        check_cycle: usize,
        check_time: DateTime<Utc>,
        no_close_reason: &str,
        notes: &str,
    ) -> Result<()> {
        if !self.write_trade_log {
            return Ok(());
        }
        self.bump_cycle(check_cycle);
        PaperCsv {
            event: "NO_CLOSE",
            session_id: self.session_id,
            trade_id: &pos.trade_id,
            pair_label: &pos.pair_label,
            cycle: check_cycle,
            simulated_open_time_utc: &pos.simulated_open_time_utc,
            check_time_utc: Some(check_time),
            pm_market_id: &pos.pm_market_id,
            kalshi_market_id: &pos.kalshi_market_id,
            pm_token_id: &pos.pm_token_id,
            pm_side: &pos.pm_side,
            kalshi_side: &pos.kalshi_side,
            n: pos.n,
            entry_capital: pos.entry_capital_used,
            pm_entry_avg: pos.pm_entry_avg,
            ks_entry_avg: pos.ks_entry_avg,
            fees_open: pos.fees_open,
            gas_open: pos.gas_open,
            exit_type: "",
            pm_exit_avg: None,
            ks_exit_avg: None,
            proceeds_gross: None,
            fees_close: None,
            gas_close: None,
            pnl_realized: None,
            cash_after: self.cash,
            no_close_reason,
            notes,
        }
        .append()?;
        Ok(())
    }

    fn append_close(
        &mut self,
        pos: &PaperPosition,
        check_cycle: usize,
        check_time: DateTime<Utc>,
        pm_exit_avg: f64,
        ks_exit_avg: f64,
        proceeds_gross: f64,
        fees_close: f64,
        gas_close: f64,
        pnl_realized: f64,
        notes: &str,
    ) -> Result<()> {
        if !self.write_trade_log {
            return Ok(());
        }
        self.bump_cycle(check_cycle);
        PaperCsv {
            event: "CLOSE",
            session_id: self.session_id,
            trade_id: &pos.trade_id,
            pair_label: &pos.pair_label,
            cycle: check_cycle,
            simulated_open_time_utc: &pos.simulated_open_time_utc,
            check_time_utc: Some(check_time),
            pm_market_id: &pos.pm_market_id,
            kalshi_market_id: &pos.kalshi_market_id,
            pm_token_id: &pos.pm_token_id,
            pm_side: &pos.pm_side,
            kalshi_side: &pos.kalshi_side,
            n: pos.n,
            entry_capital: pos.entry_capital_used,
            pm_entry_avg: pos.pm_entry_avg,
            ks_entry_avg: pos.ks_entry_avg,
            fees_open: pos.fees_open,
            gas_open: pos.gas_open,
            exit_type: "early_bid",
            pm_exit_avg: Some(pm_exit_avg),
            ks_exit_avg: Some(ks_exit_avg),
            proceeds_gross: Some(proceeds_gross),
            fees_close: Some(fees_close),
            gas_close: Some(gas_close),
            pnl_realized: Some(pnl_realized),
            cash_after: self.cash,
            no_close_reason: "",
            notes,
        }
        .append()?;
        Ok(())
    }

    pub fn try_open(
        &mut self,
        pair_label: &str,
        opp: &ArbitrageOpportunity,
        pm_side: &str,
        kalshi_side: &str,
        cycle: usize,
        pm_market_id: &str,
        kalshi_market_id: &str,
        pm_token_id: &str,
        opened_at: DateTime<Utc>,
    ) -> Result<bool> {
        if self.has_open(pair_label) || self.in_cooldown(pair_label) {
            return Ok(false);
        }
        if self.open_count() >= PAPER_MAX_OPEN_POSITIONS {
            return Ok(false);
        }

        let total_open_outlay = opp.capital_used + opp.fees_amount + opp.gas_amount;
        if self.cash < total_open_outlay {
            println!(
                "   📒 [Paper] 资金不足，跳过开仓 {} | 需要 ${:.2}（含费与 gas）剩余 ${:.2}",
                pair_label, total_open_outlay, self.cash
            );
            return Ok(false);
        }

        let trade_id = uuid::Uuid::new_v4().to_string();
        let open_time = opened_at.to_rfc3339();
        let pos = PaperPosition {
            trade_id: trade_id.clone(),
            pair_label: pair_label.to_string(),
            opened_cycle: cycle,
            simulated_open_time_utc: open_time.clone(),
            pm_market_id: pm_market_id.to_string(),
            kalshi_market_id: kalshi_market_id.to_string(),
            pm_token_id: pm_token_id.to_string(),
            n: opp.contracts,
            pm_side: pm_side.to_string(),
            kalshi_side: kalshi_side.to_string(),
            entry_capital_used: opp.capital_used,
            pm_entry_avg: opp.pm_avg_slipped,
            ks_entry_avg: opp.kalshi_avg_slipped,
            fees_open: opp.fees_amount,
            gas_open: opp.gas_amount,
        };

        self.cash -= total_open_outlay;
        self.positions.insert(pair_label.to_string(), pos.clone());

        let notes = format!(
            "open_session={} {}",
            self.session_id,
            std::env::var(PAPER_RUN_LABEL_ENV).unwrap_or_default()
        )
        .trim()
        .to_string();

        if self.write_trade_log {
            self.bump_cycle(cycle);
            PaperCsv {
                event: "OPEN",
                session_id: self.session_id,
                trade_id: &trade_id,
                pair_label,
                cycle,
                simulated_open_time_utc: &open_time,
                check_time_utc: None,
                pm_market_id,
                kalshi_market_id,
                pm_token_id,
                pm_side,
                kalshi_side,
                n: pos.n,
                entry_capital: pos.entry_capital_used,
                pm_entry_avg: pos.pm_entry_avg,
                ks_entry_avg: pos.ks_entry_avg,
                fees_open: pos.fees_open,
                gas_open: pos.gas_open,
                exit_type: "",
                pm_exit_avg: None,
                ks_exit_avg: None,
                proceeds_gross: None,
                fees_close: None,
                gas_close: None,
                pnl_realized: None,
                cash_after: self.cash,
                no_close_reason: "",
                notes: &notes,
            }
            .append()?;
        }

        println!(
            "   📒 [Paper] 开仓 OPEN | {} | trade_id={} | n={:.4} | 成本(含费/gas)${:.2} | cash=${:.2}",
            pair_label, trade_id, opp.contracts, total_open_outlay, self.cash
        );

        Ok(true)
    }

    fn append_session_start(&mut self) -> Result<()> {
        let notes = format!(
            "marker=session_start wall_utc={} initial_cash={:.2}",
            self.session_wall_started.to_rfc3339(),
            self.initial_cash
        );
        self.write_session_marker_row(
            "SESSION_START",
            0,
            self.session_wall_started,
            Some(self.session_wall_started),
            "",
            "-",
            &notes,
        )
    }

    /// 写入 `SESSION_END`（正常退出可显式调用；进程退出时 `Drop` 会再尽力写一次）。
    pub fn write_session_end(&mut self, reason: &str) {
        if self.session_end_logged || !self.write_trade_log {
            return;
        }
        let wall = Utc::now();
        let notes = format!(
            "marker=session_end wall_utc_end={} last_cycle={} reason={}",
            wall.to_rfc3339(),
            self.max_cycle_seen,
            reason
        );
        match self.write_session_marker_row(
            "SESSION_END",
            self.max_cycle_seen,
            self.session_wall_started,
            Some(wall),
            reason,
            "-",
            &notes,
        ) {
            Ok(()) => self.session_end_logged = true,
            Err(e) => eprintln!("📒 [Paper] SESSION_END 写入失败: {}", e),
        }
    }

    fn write_session_marker_row(
        &mut self,
        event: &str,
        cycle: usize,
        simulated_wall: DateTime<Utc>,
        check_wall: Option<DateTime<Utc>>,
        exit_type: &str,
        trade_id: &str,
        notes: &str,
    ) -> Result<()> {
        if !self.write_trade_log {
            return Ok(());
        }
        let sim_str = simulated_wall.to_rfc3339();
        let notes = notes.to_string();
        PaperCsv {
            event,
            session_id: self.session_id,
            trade_id,
            pair_label: "-",
            cycle,
            simulated_open_time_utc: &sim_str,
            check_time_utc: check_wall,
            pm_market_id: "",
            kalshi_market_id: "",
            pm_token_id: "",
            pm_side: "",
            kalshi_side: "",
            n: 0.0,
            entry_capital: 0.0,
            pm_entry_avg: 0.0,
            ks_entry_avg: 0.0,
            fees_open: 0.0,
            gas_open: 0.0,
            exit_type,
            pm_exit_avg: None,
            ks_exit_avg: None,
            proceeds_gross: None,
            fees_close: None,
            gas_close: None,
            pnl_realized: None,
            cash_after: self.cash,
            no_close_reason: "",
            notes: &notes,
        }
        .append()
    }

    fn bump_cycle(&mut self, cycle: usize) {
        self.max_cycle_seen = self.max_cycle_seen.max(cycle);
    }
}

impl Drop for PaperEngine {
    fn drop(&mut self) {
        self.write_session_end("drop");
    }
}

struct PaperCsv<'a> {
    event: &'a str,
    session_id: u64,
    trade_id: &'a str,
    pair_label: &'a str,
    cycle: usize,
    simulated_open_time_utc: &'a str,
    check_time_utc: Option<DateTime<Utc>>,
    pm_market_id: &'a str,
    kalshi_market_id: &'a str,
    pm_token_id: &'a str,
    pm_side: &'a str,
    kalshi_side: &'a str,
    n: f64,
    entry_capital: f64,
    pm_entry_avg: f64,
    ks_entry_avg: f64,
    fees_open: f64,
    gas_open: f64,
    exit_type: &'a str,
    pm_exit_avg: Option<f64>,
    ks_exit_avg: Option<f64>,
    proceeds_gross: Option<f64>,
    fees_close: Option<f64>,
    gas_close: Option<f64>,
    pnl_realized: Option<f64>,
    cash_after: f64,
    no_close_reason: &'a str,
    notes: &'a str,
}

impl PaperCsv<'_> {
    const HEADER: &'static [&'static str; 28] = &[
        "event",
        "session_id",
        "trade_id",
        "pair_label",
        "cycle",
        "simulated_open_time_utc",
        "check_time_utc",
        "pm_market_id",
        "kalshi_market_id",
        "pm_token_id",
        "pm_side",
        "kalshi_side",
        "n",
        "entry_capital",
        "pm_entry_avg",
        "ks_entry_avg",
        "fees_open",
        "gas_open",
        "exit_type",
        "pm_exit_avg",
        "ks_exit_avg",
        "proceeds_gross",
        "fees_close",
        "gas_close",
        "pnl_realized",
        "cash_after",
        "no_close_reason",
        "notes",
    ];

    fn append(&self) -> Result<()> {
        let mut w = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(PAPER_TRADES_CSV)
                    .context("append paper_trades.csv")?,
            );

        w.write_record([
            self.event,
            &self.session_id.to_string(),
            self.trade_id,
            self.pair_label,
            &self.cycle.to_string(),
            self.simulated_open_time_utc,
            &self
                .check_time_utc
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            self.pm_market_id,
            self.kalshi_market_id,
            self.pm_token_id,
            self.pm_side,
            self.kalshi_side,
            &format!("{:.6}", self.n),
            &format!("{:.4}", self.entry_capital),
            &format!("{:.6}", self.pm_entry_avg),
            &format!("{:.6}", self.ks_entry_avg),
            &format!("{:.4}", self.fees_open),
            &format!("{:.4}", self.gas_open),
            self.exit_type,
            &self.pm_exit_avg.map(|x| format!("{:.6}", x)).unwrap_or_default(),
            &self.ks_exit_avg.map(|x| format!("{:.6}", x)).unwrap_or_default(),
            &self
                .proceeds_gross
                .map(|x| format!("{:.4}", x))
                .unwrap_or_default(),
            &self
                .fees_close
                .map(|x| format!("{:.4}", x))
                .unwrap_or_default(),
            &self.gas_close.map(|x| format!("{:.4}", x)).unwrap_or_default(),
            &self
                .pnl_realized
                .map(|x| format!("{:.4}", x))
                .unwrap_or_default(),
            &format!("{:.4}", self.cash_after),
            self.no_close_reason,
            self.notes,
        ])?;
        w.flush()?;
        Ok(())
    }
}

fn next_session_id() -> Result<u64> {
    let path = Path::new(PAPER_SESSION_COUNTER_FILE);
    let mut n: u64 = 0;
    if path.exists() {
        let mut s = String::new();
        std::fs::File::open(path)
            .and_then(|mut f| f.read_to_string(&mut s))
            .ok();
        n = s.trim().parse().unwrap_or(0);
    }
    n = n.saturating_add(1);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, format!("{}\n", n)).context("write paper_session_counter")?;
    Ok(n)
}

/// 在已构建的 `PairOrderbookLadders` 上完成套利计算（本周期不重复解析/请求）。
pub fn validate_opportunity_from_ladders(
    arb_detector: &ArbitrageDetector,
    ladders: &PairOrderbookLadders,
    pm_side: &str,
    kalshi_side: &str,
    needs_inversion: bool,
    capital_usdt: f64,
) -> Option<ArbitrageOpportunity> {
    let pm_optimal = orderbook_best_ask_price(&ladders.pm_asks)?;
    let kalshi_optimal = orderbook_best_ask_price(&ladders.ks_asks)?;

    arb_detector.calculate_arbitrage_100usdt(
        pm_optimal,
        kalshi_optimal,
        Some(ladders.pm_asks.as_slice()),
        Some(ladders.ks_asks.as_slice()),
        pm_side,
        kalshi_side,
        needs_inversion,
        capital_usdt,
    )
}
