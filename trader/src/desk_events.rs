//! 实盘成交一行一条 JSON（`logs/desk_events.jsonl`），供 `portfolio-turnover` 等组合工具消费。
//! 固定字段含 `time_utc_rfc3339`、`trading_session_utc_date`（YYYY-MM-DD）、`session_cycle_id`、`global_tick`、`status`、`pair_label` 等。
//! `orders_*`：本回合**真正发出的委托**笔数（Kalshi 首腿买、窗末卖平、PM 第二腿 FOK 每次 post 各计 1）。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};

use crate::system_params::DESK_EVENTS_JSONL;

/// 执行层原始数据，用于 arb_reconcile 三时点对比（所有计算在 reconcile 工具中完成，不影响交易流程）。
#[derive(Debug, Clone, Default)]
pub struct ExecRaw {
    /// Kalshi IOC 实际成交份数（整数）
    pub ks_fill_count: i64,
    /// Kalshi IOC 成交限价（美分），如 45 = $0.45/份；实际成交价 ≤ 此值（上界估算）
    pub ks_fill_price_cents: i64,
    /// PM FAK 实际收到的 token 数（= taking_amount，可为小数；buying YES/NO 时 taking 是 token）
    pub pm_fill_tokens: f64,
    /// PM FAK 实际支付的 USDC（= making_amount；buying YES/NO 时 making 是 USDC）
    pub pm_cost_usdc: f64,
    /// 识别阶段估算 ROI%（来自 ArbitrageOpportunity.roi_100_percent）
    pub roi_model: f64,
    /// PM 手续费率（bps），识别阶段拉取，用于 reconcile 计算费用构成
    pub pm_fee_bps: u32,
    /// Kalshi 手续费乘数，识别阶段查费率表，用于 reconcile 计算费用构成
    pub ks_fee_multiplier: f64,
}

pub struct DeskEventsLog {
    lock: Mutex<()>,
}

impl DeskEventsLog {
    pub fn new() -> Result<Self> {
        if let Some(dir) = Path::new(DESK_EVENTS_JSONL).parent() {
            fs::create_dir_all(dir).ok();
        }
        if !Path::new(DESK_EVENTS_JSONL).exists() {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .open(DESK_EVENTS_JSONL)
                .context("创建 desk_events.jsonl")?;
            f.flush()?;
        }
        Ok(Self {
            lock: Mutex::new(()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &self,
        trading_session_utc_date: NaiveDate,
        session_cycle_id: usize,
        global_tick: usize,
        cycle_phase: &str,
        pair_label: &str,
        pm_market_id: &str,
        kalshi_market_id: &str,
        pm_buy_token_id: &str,
        pm_side: &str,
        kalshi_side: &str,
        kalshi_title: &str,
        pm_title: &str,
        contracts_n: f64,
        net_profit_model: f64,
        capital_used_model: f64,
        kalshi_order_id: &str,
        kalshi_client_order_id: &str,
        polymarket_response_json: &str,
        pm_error: &str,
        ks_error: &str,
        kalshi_unwind_order_ids: &[String],
        status: &str,
        polymarket_order_count: usize,
        exec: &ExecRaw,
    ) -> Result<()> {
        let _g = self.lock.lock().expect("desk_events lock");
        let orders_kalshi_buy = u64::from(!kalshi_order_id.is_empty());
        let orders_kalshi_unwind_sell = kalshi_unwind_order_ids.len() as u64;
        let orders_polymarket = polymarket_order_count as u64;
        let orders_kalshi_total = orders_kalshi_buy + orders_kalshi_unwind_sell;
        let row = serde_json::json!({
            "time_utc_rfc3339": Utc::now().to_rfc3339(),
            "trading_session_utc_date": trading_session_utc_date.format("%Y-%m-%d").to_string(),
            "session_cycle_id": session_cycle_id,
            "cycle_id": session_cycle_id,
            "global_tick": global_tick,
            "cycle_phase": cycle_phase,
            "pair_label": pair_label,
            "pm_market_id": pm_market_id,
            "kalshi_market_id": kalshi_market_id,
            "pm_buy_token_id": pm_buy_token_id,
            "pm_side": pm_side,
            "kalshi_side": kalshi_side,
            "kalshi_title": kalshi_title,
            "pm_title": pm_title,
            "contracts_n": contracts_n,
            "net_profit_model": net_profit_model,
            "capital_used_model": capital_used_model,
            "kalshi_order_id": kalshi_order_id,
            "kalshi_client_order_id": kalshi_client_order_id,
            "polymarket_response_json": polymarket_response_json,
            "pm_error": pm_error,
            "ks_error": ks_error,
            "kalshi_unwind_order_ids": kalshi_unwind_order_ids,
            "status": status,
            "orders_kalshi_buy": orders_kalshi_buy,
            "orders_kalshi_unwind_sell": orders_kalshi_unwind_sell,
            "orders_kalshi_total": orders_kalshi_total,
            "orders_polymarket": orders_polymarket,
            // ── 执行层原始数据（arb_reconcile 三时点对比用） ──
            "ks_fill_count": exec.ks_fill_count,
            "ks_fill_price_cents": exec.ks_fill_price_cents,
            "pm_fill_tokens": exec.pm_fill_tokens,
            "pm_cost_usdc": exec.pm_cost_usdc,
            "roi_model": exec.roi_model,
            "pm_fee_bps": exec.pm_fee_bps,
            "ks_fee_multiplier": exec.ks_fee_multiplier,
        });
        let line = serde_json::to_string(&row).context("序列化 desk 事件")?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(DESK_EVENTS_JSONL)
            .context("追加 desk_events.jsonl")?;
        writeln!(f, "{}", line).context("写入 desk_events.jsonl")?;
        f.flush()?;
        Ok(())
    }
}
