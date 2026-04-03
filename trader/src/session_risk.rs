//! 会话级风控：启动快照 \(T_0\)、现金预警、再平衡提示、暂停实盘直至恢复。

use std::sync::{Arc, Mutex};

use anyhow::Result;
use reqwest::Client as HttpClient;

use crate::clients::PolymarketClient;
use crate::feishu_push;
use crate::kalshi_trade::{self, KalshiTradeConfig};
use crate::polymarket_trade::{clob_collateral_micro_usdc_to_usd, PolymarketClobTrader};
use crate::system_params::{
    arb_probe_caps, set_session_reference_capital_usd, DESK_EVENTS_JSONL,
    SESSION_LARGE_CAPITAL_THRESHOLD_USD, SESSION_LARGE_WARN_CASH_FRAC_OF_T0,
    SESSION_REBALANCE_PEER_CASH_FRAC_OF_COMBINED_T0, SESSION_SMALL_WARN_PER_LEG_CAP_MULT,
};

pub type SessionRiskHandle = Arc<Mutex<SessionRiskController>>;

#[derive(Clone, Debug)]
pub struct VenueBaseline {
    pub t0_usd: f64,
    pub large_tier: bool,
    pub cash_warn_threshold_usd: f64,
}

#[derive(Clone, Debug)]
pub struct SessionBaselines {
    /// \(\min(KS\ T0,\ PM\ T0)\)，与探针比例相乘得到单腿/每对名义上限。
    pub reference_capital_usd: f64,
    pub ks: VenueBaseline,
    pub pm: VenueBaseline,
    pub combined_t0_usd: f64,
}

#[derive(Clone, Debug)]
pub struct LiveSnapshot {
    pub ks_cash_usd: f64,
    /// Kalshi 条件/事件合约：`position_fp`×该侧订单簿 **中间价**（美元概率）之和，与 PM 侧 mid 口径一致。
    pub ks_position_mtm_usd: f64,
    /// Kalshi：**市场** = `event_positions` 有敞口的 event 去重数（无则等于 token 数）。
    pub ks_markets_n: usize,
    /// Kalshi：**token** = `market_positions` 有敞口的 ticker 去重数（单合约腿）。
    pub ks_tokens_n: usize,
    /// Kalshi 有敞口合约腿中，成功用簿算出 mid 并计入 `ks_position_mtm_usd` 的条数。
    pub ks_position_mtm_priced_n: usize,
    pub pm_collateral_usd: f64,
    pub pm_cond_mtm_usd: f64,
    /// Polymarket 条件仓：**市场** = Gamma `id` 去重（`clob_token_ids` 反查失败时退化为 token 数）。
    pub pm_conditional_markets_n: usize,
    /// Polymarket 条件仓：**token** = Conditional `balance>0` 的 outcome 条数。
    pub pm_conditional_tokens_n: usize,
    /// 上述 token 中，成功用订单簿 mid 估出 MTM 的条数。
    pub pm_cond_mtm_tokens_n: usize,
}

impl LiveSnapshot {
    pub fn ks_total_usd(&self) -> f64 {
        self.ks_cash_usd + self.ks_position_mtm_usd
    }

    pub fn pm_total_usd(&self) -> f64 {
        self.pm_collateral_usd + self.pm_cond_mtm_usd
    }
}

pub struct SessionRiskController {
    baseline: SessionBaselines,
    last: LiveSnapshot,
    ks_warn: bool,
    pm_warn: bool,
    paused: bool,
    last_rebalance_candidate: bool,
}

pub async fn fetch_live_snapshot(
    http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    polymarket: &PolymarketClient,
) -> Result<LiveSnapshot> {
    let ks_cents = kalshi_trade::get_balance_cents(http, kalshi_cfg).await?;
    let ks_cash_usd = ks_cents as f64 / 100.0;
    let (ks_position_mtm_usd, ks_markets_n, ks_tokens_n, ks_position_mtm_priced_n) =
        kalshi_trade::aggregate_kalshi_market_positions_mtm_usd_and_count(http, kalshi_cfg).await?;
    let pm_col_dec = pm_trader
        .fetch_clob_collateral_balance_for_guard()
        .await?;
    let pm_collateral_usd = clob_collateral_micro_usdc_to_usd(&pm_col_dec);
    let (pm_cond_mtm_usd, pm_conditional_markets_n, pm_conditional_tokens_n, pm_cond_mtm_tokens_n) = pm_trader
        .estimate_conditional_mtm_usd_from_trades_and_books(polymarket)
        .await?;
    Ok(LiveSnapshot {
        ks_cash_usd,
        ks_position_mtm_usd,
        ks_markets_n,
        ks_tokens_n,
        ks_position_mtm_priced_n,
        pm_collateral_usd,
        pm_cond_mtm_usd,
        pm_conditional_markets_n,
        pm_conditional_tokens_n,
        pm_cond_mtm_tokens_n,
    })
}

impl SessionRiskController {
    /// 用启动时刻快照固定 \(T_0\) 与 \(\min(T0)\) 参考总资金，并写入 [`set_session_reference_capital_usd`]。
    pub fn from_startup_snapshot(snap: LiveSnapshot) -> Self {
        let ks_t0 = snap.ks_total_usd();
        let pm_t0 = snap.pm_total_usd();
        let reference_capital_usd = ks_t0.min(pm_t0);
        set_session_reference_capital_usd(reference_capital_usd);

        let (per_leg, _) = arb_probe_caps();
        let combined_t0_usd = ks_t0 + pm_t0;
        let ks_large = ks_t0 + 1e-9 >= SESSION_LARGE_CAPITAL_THRESHOLD_USD;
        let pm_large = pm_t0 + 1e-9 >= SESSION_LARGE_CAPITAL_THRESHOLD_USD;
        let ks_cash_warn_threshold_usd = if ks_large {
            SESSION_LARGE_WARN_CASH_FRAC_OF_T0 * ks_t0
        } else {
            per_leg * SESSION_SMALL_WARN_PER_LEG_CAP_MULT
        };
        let pm_cash_warn_threshold_usd = if pm_large {
            SESSION_LARGE_WARN_CASH_FRAC_OF_T0 * pm_t0
        } else {
            per_leg * SESSION_SMALL_WARN_PER_LEG_CAP_MULT
        };
        let baseline = SessionBaselines {
            reference_capital_usd,
            ks: VenueBaseline {
                t0_usd: ks_t0,
                large_tier: ks_large,
                cash_warn_threshold_usd: ks_cash_warn_threshold_usd,
            },
            pm: VenueBaseline {
                t0_usd: pm_t0,
                large_tier: pm_large,
                cash_warn_threshold_usd: pm_cash_warn_threshold_usd,
            },
            combined_t0_usd,
        };
        let ks_warn = snap.ks_cash_usd + 1e-9 < ks_cash_warn_threshold_usd;
        let pm_warn = snap.pm_collateral_usd + 1e-9 < pm_cash_warn_threshold_usd;
        let peer_thr = SESSION_REBALANCE_PEER_CASH_FRAC_OF_COMBINED_T0 * combined_t0_usd;
        let last_rebalance_candidate = (ks_warn && snap.pm_collateral_usd + 1e-9 >= peer_thr)
            || (pm_warn && snap.ks_cash_usd + 1e-9 >= peer_thr);
        Self {
            baseline,
            last: snap,
            ks_warn,
            pm_warn,
            paused: ks_warn || pm_warn,
            last_rebalance_candidate,
        }
    }

    pub fn baselines(&self) -> &SessionBaselines {
        &self.baseline
    }

    pub fn last_snapshot(&self) -> &LiveSnapshot {
        &self.last
    }

    pub fn trading_paused(&self) -> bool {
        self.paused
    }

    /// 拉取最新快照并据**上升沿 / 解除**推飞书（不误报重复预警）。
    pub fn apply_snapshot_with_notifications(&mut self, snap: &LiveSnapshot) {
        let prev_ks = self.ks_warn;
        let prev_pm = self.pm_warn;
        let prev_paused = self.paused;
        let prev_rebal = self.last_rebalance_candidate;

        let ks_warn =
            snap.ks_cash_usd + 1e-9 < self.baseline.ks.cash_warn_threshold_usd;
        let pm_warn =
            snap.pm_collateral_usd + 1e-9 < self.baseline.pm.cash_warn_threshold_usd;
        let paused = ks_warn || pm_warn;

        let peer_thr =
            SESSION_REBALANCE_PEER_CASH_FRAC_OF_COMBINED_T0 * self.baseline.combined_t0_usd;
        let rebalance_candidate = (ks_warn && snap.pm_collateral_usd + 1e-9 >= peer_thr)
            || (pm_warn && snap.ks_cash_usd + 1e-9 >= peer_thr);

        if ks_warn && !prev_ks {
            let msg = format_session_warn(
                "Kalshi",
                snap.ks_cash_usd,
                self.baseline.ks.cash_warn_threshold_usd,
                self.baseline.ks.t0_usd,
                self.baseline.ks.large_tier,
            );
            let _ = feishu_push::push_trade_summary_to_feishu(&msg);
        }
        if pm_warn && !prev_pm {
            let msg = format_session_warn(
                "Polymarket",
                snap.pm_collateral_usd,
                self.baseline.pm.cash_warn_threshold_usd,
                self.baseline.pm.t0_usd,
                self.baseline.pm.large_tier,
            );
            let _ = feishu_push::push_trade_summary_to_feishu(&msg);
        }
        if rebalance_candidate && !prev_rebal {
            let msg = format_session_rebalance(
                peer_thr,
                snap.ks_cash_usd,
                snap.pm_collateral_usd,
                ks_warn,
                pm_warn,
            );
            let _ = feishu_push::push_trade_summary_to_feishu(&msg);
        }
        if !paused && prev_paused {
            let msg = format_session_recovery(snap);
            let _ = feishu_push::push_trade_summary_to_feishu(&msg);
        }

        self.last = snap.clone();
        self.ks_warn = ks_warn;
        self.pm_warn = pm_warn;
        self.paused = paused;
        self.last_rebalance_candidate = rebalance_candidate;
    }
}

pub async fn refresh_and_notify(
    handle: &SessionRiskHandle,
    http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    polymarket: &PolymarketClient,
) -> Result<()> {
    let snap = fetch_live_snapshot(http, kalshi_cfg, pm_trader, polymarket).await?;
    let mut g = handle.lock().unwrap();
    g.apply_snapshot_with_notifications(&snap);
    Ok(())
}

pub fn format_session_startup(snap: &LiveSnapshot, baseline: &SessionBaselines) -> String {
    let ks = &baseline.ks;
    let pm = &baseline.pm;
    let tier_ks = if ks.large_tier { "大额" } else { "小额" };
    let tier_pm = if pm.large_tier { "大额" } else { "小额" };
    let (per_leg, pair_cap) = arb_probe_caps();
    format!(
        "【启动体检】SESSION START HEALTH CHECK\n\
交易周期开始 · 账户（美元 USD）\n\
— — —\n\
Kalshi:\n\
  · 现金余额: ${:.2}（GET /portfolio/balance，美分折算）\n\
  · 持仓 MTM: ${:.2}（position_fp × 订单簿中间价；已标价 {}/{} 条合约腿）\n\
  · 持仓：市场 {} 个 · token {} 个（市场=event 去重，token=ticker 合约腿）\n\
  · 总资产 T0: ${:.2}（现金+持仓MTM） | 档别:{tier_ks}\n\
Polymarket:\n\
  · 现金余额(Collateral USDC): ${:.2}（CLOB balance-allowance，micro÷1e6）\n\
  · 条件仓 MTM: ${:.2}（mid×份额，balance micro÷1e6）\n\
  · 持仓：市场 {} 个 · token {} 个（Gamma id 去重 / outcome 条数，已估 mid {} 条 token）\n\
  · 总资产 T0: ${:.2}（现金+MTM） | 档别:{tier_pm}\n\
— — —\n\
策略参考总资金 R: ${:.2} = min(Kalshi_T0, PM_T0)（比例不变）\n\
  · 单腿探针上限: ${:.4} = R×per_leg_frac\n\
  · 每对名义上限: ${:.4} = R×pair_frac\n\
— — —\n\
现金预警线: KS ${:.2} | PM ${:.2}\n\
两侧 T0 合计 ${:.2}；再平衡门槛(对侧现金): ≥${:.2}（合计T0×{:.0}%）\n\
— — —\n\
成交日志: {}",
        snap.ks_cash_usd,
        snap.ks_position_mtm_usd,
        snap.ks_position_mtm_priced_n,
        snap.ks_tokens_n,
        snap.ks_markets_n,
        snap.ks_tokens_n,
        ks.t0_usd,
        snap.pm_collateral_usd,
        snap.pm_cond_mtm_usd,
        snap.pm_conditional_markets_n,
        snap.pm_conditional_tokens_n,
        snap.pm_cond_mtm_tokens_n,
        pm.t0_usd,
        baseline.reference_capital_usd,
        per_leg,
        pair_cap,
        ks.cash_warn_threshold_usd,
        pm.cash_warn_threshold_usd,
        baseline.combined_t0_usd,
        SESSION_REBALANCE_PEER_CASH_FRAC_OF_COMBINED_T0 * baseline.combined_t0_usd,
        SESSION_REBALANCE_PEER_CASH_FRAC_OF_COMBINED_T0 * 100.0,
        DESK_EVENTS_JSONL,
    )
}

fn format_session_warn(
    venue: &str,
    cash_usd: f64,
    threshold_usd: f64,
    t0_usd: f64,
    large: bool,
) -> String {
    let tier = if large { "大额" } else { "小额" };
    format!(
        "【风控预警】CASH RISK — {venue}\n\
档别: {tier}\n\
当前现金(USD): ${:.2}\n\
预警线: ${:.2}\n\
该平台启动T0: ${:.2}\n\
已暂停实盘下单，解除后自动恢复并推送【风控恢复】。",
        cash_usd, threshold_usd, t0_usd
    )
}

fn format_session_rebalance(
    peer_min_cash_usd: f64,
    ks_cash: f64,
    pm_cash: f64,
    ks_in_warn: bool,
    pm_in_warn: bool,
) -> String {
    format!(
        "【再平衡提示】REBALANCE HINT\n\
一侧已触发现金预警，且另一侧现金/Collateral ≥ ${:.2}（启动时两侧T0合计×{:.0}%）。\n\
Kalshi 现金 ${:.2} | Polymarket Collateral ${:.2}\n\
预警: Kalshi={} Polymarket={}\n\
（仅提示，不自动划转。）",
        peer_min_cash_usd,
        SESSION_REBALANCE_PEER_CASH_FRAC_OF_COMBINED_T0 * 100.0,
        ks_cash,
        pm_cash,
        if ks_in_warn { "是" } else { "否" },
        if pm_in_warn { "是" } else { "否" },
    )
}

fn format_session_recovery(snap: &LiveSnapshot) -> String {
    format!(
        "【风控恢复】RISK CLEARED\n\
Kalshi 现金 ${:.2} | Polymarket Collateral ${:.2}\n\
已恢复实盘下单。",
        snap.ks_cash_usd, snap.pm_collateral_usd
    )
}
