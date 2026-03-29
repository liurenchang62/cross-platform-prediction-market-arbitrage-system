//! Kalshi `/series/fee_changes`：仅在「跟踪/全量匹配前」拉取，按 series 缓存乘数。

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde_json::Value;

use crate::fee_model::kalshi_series_from_ticker;
use crate::kalshi_trade::{fetch_series_fee_changes_json, KalshiTradeConfig};

#[derive(Clone, Debug, Default)]
pub struct KalshiFeeScheduleState {
    /// `series_ticker` -> 当前生效的 taker 侧乘数（作用于公式中 `0.07`）。
    pub quadratic_taker_multiplier: HashMap<String, f64>,
}

impl KalshiFeeScheduleState {
    pub fn multiplier_for_ticker(&self, market_ticker: &str) -> f64 {
        let series = kalshi_series_from_ticker(market_ticker);
        self.multiplier_for_series(&series)
    }

    pub fn multiplier_for_series(&self, series_ticker: &str) -> f64 {
        self.quadratic_taker_multiplier
            .get(series_ticker)
            .copied()
            .filter(|m| m.is_finite() && *m > 0.0)
            .unwrap_or(1.0)
    }
}

/// 在跟踪前调用：仅当 `series_filter` 非空时请求 API；否则返回默认乘数 1。
pub async fn fetch_kalshi_fee_schedule(
    http: &Client,
    cfg: &KalshiTradeConfig,
    series_filter: &HashSet<String>,
) -> Result<KalshiFeeScheduleState> {
    if series_filter.is_empty() {
        return Ok(KalshiFeeScheduleState::default());
    }

    let v = fetch_series_fee_changes_json(http, cfg).await?;
    Ok(parse_fee_changes(&v, series_filter, Utc::now()))
}

fn parse_fee_changes(v: &Value, series_filter: &HashSet<String>, now: DateTime<Utc>) -> KalshiFeeScheduleState {
    let mut best: HashMap<String, (DateTime<Utc>, f64)> = HashMap::new();
    let arr = v
        .get("series_fee_change_arr")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    for row in arr {
        let Some(st) = row.get("series_ticker").and_then(|x| x.as_str()) else {
            continue;
        };
        if !series_filter.contains(st) {
            continue;
        }
        let Some(Ok(ts)) = row
            .get("scheduled_ts")
            .and_then(|x| x.as_str())
            .map(|s| DateTime::parse_from_rfc3339(s))
        else {
            continue;
        };
        let ts = ts.with_timezone(&Utc);
        if ts > now {
            continue;
        }
        let fee_type = row
            .get("fee_type")
            .and_then(|x| x.as_str())
            .unwrap_or("quadratic");
        let mult = row
            .get("fee_multiplier")
            .and_then(|x| x.as_f64())
            .unwrap_or(1.0);

        let use_mult = match fee_type {
            "quadratic" | "quadratic_with_maker_fees" => mult,
            "flat" | _ => 1.0,
        };

        best.entry(st.to_string())
            .and_modify(|e| {
                if ts >= e.0 {
                    *e = (ts, use_mult);
                }
            })
            .or_insert((ts, use_mult));
    }

    KalshiFeeScheduleState {
        quadratic_taker_multiplier: best.into_iter().map(|(s, (_t, m))| (s, m)).collect(),
    }
}
