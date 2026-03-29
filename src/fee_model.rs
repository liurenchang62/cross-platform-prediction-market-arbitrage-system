//! 跨平台套利：Kalshi / Polymarket 费用与摩擦（与 `system_params` 中的 Gas+缓冲 配合）。

use std::collections::HashMap;

use crate::category_mapper::CategoryMapper;
use crate::market::Market;
use crate::system_params::{live_arbit_surplus_buffer_usd, LIVE_ARBIT_GAS_ASSUMED_USD};

/// 单次完整套利在模型中的「非交易」摩擦合计：`Gas` 固定美元 + 缓冲（`pair_cap ×` [`crate::system_params::LIVE_ARBIT_SURPLUS_BUFFER_FRAC_OF_PAIR_CAP`]）。
#[inline]
pub fn live_arb_auxiliary_drag_usd(pair_cap_usdt: f64) -> f64 {
    LIVE_ARBIT_GAS_ASSUMED_USD + live_arbit_surplus_buffer_usd(pair_cap_usdt)
}

/// 单腿（如仅 Kalshi 卖平）估算时，对摩擦按半轮计。
#[inline]
pub fn live_arb_auxiliary_drag_half_usd(pair_cap_usdt: f64) -> f64 {
    live_arb_auxiliary_drag_usd(pair_cap_usdt) * 0.5
}

/// 一轮套利估算用的费率快照（PM `fee_rate` 在第一腿成功后锁定直至第二腿结束/止损）。
#[derive(Clone, Debug)]
pub struct ArbCostParams {
    /// Kalshi taker 公式前置系数 `0.07` 的乘数（来自 `/series/fee_changes`，默认 1）。
    pub ks_taker_fee_multiplier: f64,
    /// Polymarket `GET /fee-rate` 的 `base_fee`（bps）；签单与预估同源。
    pub pm_fee_bps: u32,
    /// PM 文档公式指数 `(p(1-p))^exponent`；来自本仓库 `categories.toml` 大类映射。
    pub pm_exponent: f64,
}

impl Default for ArbCostParams {
    fn default() -> Self {
        Self {
            ks_taker_fee_multiplier: 1.0,
            pm_fee_bps: 0,
            pm_exponent: PM_EXPONENT_UNMATCHED,
        }
    }
}

/// PM 文档（2026-03-30 起扩展类目）中 exponent；未命中类目时用折中值 **1**。
pub const PM_EXPONENT_UNMATCHED: f64 = 1.0;

fn pm_exponent_table() -> HashMap<&'static str, f64> {
    // 与 config/categories.toml 的 `name` 对齐；science 与 technology 同为 Tech(exp=1)。
    [
        ("crypto", 1.0),
        ("sports_basketball", 1.0),
        ("sports_football", 1.0),
        ("sports_soccer", 1.0),
        ("sports_golf", 1.0),
        ("sports_teams", 1.0),
        ("sports_college", 1.0),
        ("sports_other", 1.0),
        ("business", 1.0),
        ("politics_us", 1.0),
        ("politics_uk", 1.0),
        ("technology", 1.0),
        ("science", 1.0),
        ("weather", 0.5),
        ("entertainment_movies", 1.0),
        ("entertainment_music", 1.0),
        ("gaming", 2.0),
        ("religion", 2.0),
    ]
    .into_iter()
    .collect()
}

/// 由本仓库大类（`CategoryMapper::classify`）得到 PM 指数；多标签取 **较大** exponent（中段价上略偏保守）。
pub fn pm_exponent_for_pm_market(mapper: &CategoryMapper, pm: &Market) -> f64 {
    let names = mapper.classify(&pm.title);
    pm_exponent_for_category_names(&names)
}

pub fn pm_exponent_for_category_names(names: &[String]) -> f64 {
    if names.is_empty() {
        return PM_EXPONENT_UNMATCHED;
    }
    let table = pm_exponent_table();
    let mut m = PM_EXPONENT_UNMATCHED;
    for n in names {
        let e = table
            .get(n.as_str())
            .copied()
            .unwrap_or(PM_EXPONENT_UNMATCHED);
        m = m.max(e);
    }
    m
}

/// Kalshi taker（二次型）：`ceil_cent(0.07 * mult * C * P * (1-P))`，P 为美元概率价。
pub fn kalshi_taker_fee_usd_round_up(contracts: f64, price_dollars: f64, fee_mult: f64) -> f64 {
    if contracts <= 0.0 || !contracts.is_finite() {
        return 0.0;
    }
    let p = price_dollars.clamp(1e-9, 1.0 - 1e-9);
    let m = if fee_mult.is_finite() && fee_mult > 0.0 {
        fee_mult
    } else {
        1.0
    };
    let raw = 0.07_f64 * m * contracts * p * (1.0 - p);
    (raw * 100.0).ceil() / 100.0
}

/// Polymarket taker 费（USDC）：`C * p * (bps/10000) * (p(1-p))^exponent`，再按文档舍入。
pub fn polymarket_taker_fee_usdc_round(
    shares: f64,
    price_dollars: f64,
    fee_bps: u32,
    exponent: f64,
) -> f64 {
    if fee_bps == 0 || shares <= 0.0 || !shares.is_finite() {
        return 0.0;
    }
    let p = price_dollars.clamp(1e-12, 1.0 - 1e-12);
    let fee_rate = (fee_bps as f64) / 10_000.0;
    let pq = p * (1.0 - p);
    let expo = if exponent.is_finite() { exponent.max(0.0) } else { 1.0 };
    let v = shares * p * fee_rate * pq.powf(expo);
    if !v.is_finite() || v <= 0.0 {
        return 0.0;
    }
    let rounded = (v * 10_000.0).round() / 10_000.0;
    if rounded < 0.0001 {
        0.0
    } else {
        rounded
    }
}

/// Kalshi `market_id`（ticker）前缀作为 series（与行情 API 中 event 前缀一致）。
pub fn kalshi_series_from_ticker(ticker: &str) -> String {
    ticker
        .split('-')
        .next()
        .unwrap_or(ticker)
        .trim()
        .to_string()
}
