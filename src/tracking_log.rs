//! 仅跟踪/监控：`logs/tracking_pairs.csv`（每周期追踪对快照）与 `logs/tracking_arbitrage_events.csv`（每次检出套利 + 簿前 5 档）。  
//! 与 [`crate::desk_events::DeskEventsLog`]（实盘成交 JSONL）完全分离。

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use csv::WriterBuilder;
use serde_json::json;

use crate::arbitrage_detector::ArbitrageOpportunity;
use crate::system_params::{TRACKING_ARB_EVENTS_CSV, TRACKING_PAIRS_CSV};
use crate::tracking::TrackedArbitrage;

/// 写入套利事件 CSV 时附带的一屏参数（与 `system_params` / 当周期池大小一致）。
#[derive(Clone, Debug)]
pub struct TrackingParamsSnapshot {
    pub total_capital_usd: f64,
    pub per_leg_cap_usd: f64,
    pub pair_cap_usd: f64,
    pub per_leg_cap_frac: f64,
    pub pair_cap_frac: f64,
    pub resolution_horizon_days: i64,
    pub polymarket_markets_n: usize,
    pub kalshi_markets_n: usize,
    pub similarity_threshold: f64,
    pub full_fetch_interval_cycles: usize,
    pub arb_track_concurrency: usize,
    pub min_net_profit_frac_of_pair_cap: f64,
}

const PAIRS_HEADER: &[&str] = &[
    "time_utc_rfc3339",
    "cycle_id",
    "cycle_phase",
    "pair_id",
    "pm_market_id",
    "kalshi_market_id",
    "pm_title",
    "kalshi_title",
    "pm_side",
    "kalshi_side",
    "similarity",
    "needs_inversion",
    "active",
    "trading_session_utc_date",
    "global_tick",
];

const ARB_HEADER: &[&str] = &[
    "time_utc_rfc3339",
    "cycle_id",
    "cycle_phase",
    "pair_id",
    "pm_market_id",
    "kalshi_market_id",
    "pm_title",
    "kalshi_title",
    "pm_side",
    "kalshi_side",
    "similarity",
    "needs_inversion",
    "net_profit_100",
    "roi_100_percent",
    "contracts",
    "pm_optimal",
    "kalshi_optimal",
    "pm_avg_slipped",
    "kalshi_avg_slipped",
    "capital_used",
    "pm_book_top5_json",
    "ks_book_top5_json",
    "total_capital_usd",
    "per_leg_cap_usd",
    "pair_cap_usd",
    "per_leg_cap_frac",
    "pair_cap_frac",
    "resolution_horizon_days",
    "polymarket_markets_n",
    "kalshi_markets_n",
    "similarity_threshold",
    "full_fetch_interval_cycles",
    "arb_track_concurrency",
    "min_net_profit_frac_of_pair_cap",
];

fn ensure_csv(path: &str, header: &[&str]) -> Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        fs::create_dir_all(dir).ok();
    }
    if !Path::new(path).exists() {
        let mut w = WriterBuilder::new()
            .has_headers(false)
            .from_path(path)
            .with_context(|| format!("create {path}"))?;
        w.write_record(header)?;
        w.flush()?;
    }
    Ok(())
}

fn book_top5_json(v: &[(f64, f64)]) -> String {
    json!(v.iter().take(5).map(|(p, s)| json!([p, s])).collect::<Vec<_>>()).to_string()
}

#[derive(Clone)]
pub struct TrackingPairsLog {
    inner: Arc<Mutex<()>>,
}

impl TrackingPairsLog {
    pub fn new() -> Result<Self> {
        ensure_csv(TRACKING_PAIRS_CSV, PAIRS_HEADER)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(())),
        })
    }

    pub fn append_cycle_snapshot(
        &self,
        session_cycle_id: usize,
        cycle_phase: &str,
        pairs: &[TrackedArbitrage],
        trading_session_utc_date: NaiveDate,
        global_tick: usize,
    ) -> Result<()> {
        let _g = self.inner.lock().expect("tracking_pairs lock");
        let t = Utc::now().to_rfc3339();
        let mut w = WriterBuilder::new()
            .has_headers(false)
            .from_writer(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(TRACKING_PAIRS_CSV)
                    .context("append tracking_pairs.csv")?,
            );
        for p in pairs {
            if !p.active {
                continue;
            }
            w.write_record(&[
                t.clone(),
                session_cycle_id.to_string(),
                cycle_phase.to_string(),
                p.pair_id.clone(),
                p.pm_market.market_id.clone(),
                p.kalshi_market.market_id.clone(),
                p.pm_market.title.clone(),
                p.kalshi_market.title.clone(),
                p.pm_side.clone(),
                p.kalshi_side.clone(),
                format!("{:.6}", p.similarity),
                if p.needs_inversion { "1" } else { "0" }.to_string(),
                "1".to_string(),
                trading_session_utc_date.format("%Y-%m-%d").to_string(),
                global_tick.to_string(),
            ])?;
        }
        w.flush()?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct TrackingArbEventsLog {
    inner: Arc<Mutex<()>>,
}

impl TrackingArbEventsLog {
    pub fn new() -> Result<Self> {
        ensure_csv(TRACKING_ARB_EVENTS_CSV, ARB_HEADER)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(())),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_arbitrage_found(
        &self,
        session_cycle_id: usize,
        cycle_phase: &str,
        pair_id: &str,
        pm_market_id: &str,
        kalshi_market_id: &str,
        pm_title: &str,
        kalshi_title: &str,
        pm_side: &str,
        kalshi_side: &str,
        similarity: f64,
        needs_inversion: bool,
        opp: &ArbitrageOpportunity,
        params: &TrackingParamsSnapshot,
        trading_session_utc_date: NaiveDate,
        global_tick: usize,
    ) -> Result<()> {
        let _g = self.inner.lock().expect("tracking_arb lock");
        let t = Utc::now().to_rfc3339();
        let mut w = WriterBuilder::new()
            .has_headers(false)
            .from_writer(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(TRACKING_ARB_EVENTS_CSV)
                    .context("append tracking_arbitrage_events.csv")?,
            );
        let pmj = book_top5_json(&opp.orderbook_pm_top5);
        let ksj = book_top5_json(&opp.orderbook_kalshi_top5);
        w.write_record(&[
            t,
            session_cycle_id.to_string(),
            cycle_phase.to_string(),
            pair_id.to_string(),
            pm_market_id.to_string(),
            kalshi_market_id.to_string(),
            pm_title.to_string(),
            kalshi_title.to_string(),
            pm_side.to_string(),
            kalshi_side.to_string(),
            format!("{:.6}", similarity),
            if needs_inversion { "1" } else { "0" }.to_string(),
            format!("{:.6}", opp.net_profit_100),
            format!("{:.6}", opp.roi_100_percent),
            format!("{:.6}", opp.contracts),
            format!("{:.6}", opp.pm_optimal),
            format!("{:.6}", opp.kalshi_optimal),
            format!("{:.6}", opp.pm_avg_slipped),
            format!("{:.6}", opp.kalshi_avg_slipped),
            format!("{:.6}", opp.capital_used),
            pmj,
            ksj,
            format!("{:.6}", params.total_capital_usd),
            format!("{:.6}", params.per_leg_cap_usd),
            format!("{:.6}", params.pair_cap_usd),
            format!("{:.6}", params.per_leg_cap_frac),
            format!("{:.6}", params.pair_cap_frac),
            params.resolution_horizon_days.to_string(),
            params.polymarket_markets_n.to_string(),
            params.kalshi_markets_n.to_string(),
            format!("{:.6}", params.similarity_threshold),
            params.full_fetch_interval_cycles.to_string(),
            params.arb_track_concurrency.to_string(),
            format!("{:.6}", params.min_net_profit_frac_of_pair_cap),
            trading_session_utc_date.format("%Y-%m-%d").to_string(),
            global_tick.to_string(),
        ])?;
        w.flush()?;
        Ok(())
    }
}

/// 终端：仅跟踪模式下每次检出套利时打印簿前 5 档（实盘路径仍由 `validate_arbitrage_pair_post_success` 详打）。
pub fn print_arbitrage_detection_books(
    opp: &ArbitrageOpportunity,
    pm_market_title: &str,
    kalshi_market_title: &str,
    similarity: f64,
    needs_inversion: bool,
    pm_side: &str,
    kalshi_side: &str,
    pm_expiry: Option<DateTime<Utc>>,
    ks_expiry: Option<DateTime<Utc>>,
) {
    let inv = if needs_inversion { " (Y/N颠倒)" } else { "" };
    println!("\n  📌 检出套利 (similarity {:.3}){}", similarity, inv);
    println!("     PM:     {}", pm_market_title);
    println!("     Kalshi: {}", kalshi_market_title);
    if let Some(d) = pm_expiry {
        println!("     PM 到期: {} UTC", d.format("%Y-%m-%d %H:%M"));
    }
    if let Some(d) = ks_expiry {
        println!("     KS 到期: {} UTC", d.format("%Y-%m-%d %H:%M"));
    }
    println!("     📗 PM 买{} 簿 Top5:", pm_side);
    for (j, (p, s)) in opp.orderbook_pm_top5.iter().take(5).enumerate() {
        println!("        #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_pm_top5.is_empty() {
        println!("        (空)");
    }
    println!("     📗 Kalshi 买{} 簿 Top5:", kalshi_side);
    for (j, (p, s)) in opp.orderbook_kalshi_top5.iter().take(5).enumerate() {
        println!("        #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_kalshi_top5.is_empty() {
        println!("        (空)");
    }
    println!(
        "     💰 模型净利 ${:.2}  ROI {:.1}%  n={:.4}",
        opp.net_profit_100, opp.roi_100_percent, opp.contracts
    );
}
