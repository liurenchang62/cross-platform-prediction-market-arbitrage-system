//! 解析日窗口由 `system_params::RESOLUTION_HORIZON_DAYS` 控制：只读 `Market.resolution_date`，**字段来源与终端「到期」、匹配器日期加权完全一致**
//!（`clients::parse_polymarket_market_resolution_date` / `parse_kalshi_market_resolution_date`，Kalshi 优先 `expected_expiration_time`）。
//! 剔除「有明确时间且严格晚于 now+horizon」的远期市场；无日期的保留。

use chrono::{DateTime, Duration, Utc};

use crate::market::Market;
use crate::system_params::RESOLUTION_HORIZON_DAYS;

/// 若市场有 `resolution_date`（同上解析标准）且该时间 **严格晚于** `now + horizon_days`，则剔除。
pub fn filter_markets_by_resolution_horizon(
    markets: Vec<Market>,
    now: DateTime<Utc>,
) -> Vec<Market> {
    let horizon = Duration::days(RESOLUTION_HORIZON_DAYS);
    let cutoff = now + horizon;
    markets
        .into_iter()
        .filter(|m| match m.resolution_date {
            None => true,
            Some(dt) => dt <= cutoff,
        })
        .collect()
}

/// 任一侧 `resolution_date`（与展示同源）晚于 cutoff，则该追踪对应剔除。
pub fn tracked_pair_exceeds_horizon(
    pm: &Market,
    ks: &Market,
    now: DateTime<Utc>,
) -> bool {
    let horizon = Duration::days(RESOLUTION_HORIZON_DAYS);
    let cutoff = now + horizon;
    let pm_far = pm
        .resolution_date
        .map(|dt| dt > cutoff)
        .unwrap_or(false);
    let ks_far = ks
        .resolution_date
        .map(|dt| dt > cutoff)
        .unwrap_or(false);
    pm_far || ks_far
}

/// 两侧均有到期时刻且 **严格宽于** `max_days`（整日等价：`abs(Δ秒) > max_days×86400`）时返回 `true`。`max_days ≤ 0` 视为不启用。
pub fn pair_settlement_dates_skew_exceeds(pm: &Market, ks: &Market, max_days: i64) -> bool {
    pair_settlement_dates_skew_exceeds_opt(pm.resolution_date, ks.resolution_date, max_days)
}

/// 同上，直接使用可选到期字段（便于探针逻辑复用）。
pub fn pair_settlement_dates_skew_exceeds_opt(
    pm: Option<DateTime<Utc>>,
    ks: Option<DateTime<Utc>>,
    max_days: i64,
) -> bool {
    if max_days <= 0 {
        return false;
    }
    let (Some(d_pm), Some(d_ks)) = (pm, ks) else {
        return false;
    };
    let limit_secs = max_days.saturating_mul(86_400).max(0) as u64;
    (d_pm - d_ks).num_seconds().abs() as u64 > limit_secs
}
