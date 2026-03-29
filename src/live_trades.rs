//! 唯一实盘输出：`logs/live_trades.csv`（追加，带表头）。

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use csv::WriterBuilder;

use crate::system_params::LIVE_TRADES_CSV;

const HEADER: &[&str] = &[
    "time_utc_rfc3339",
    "cycle_id",
    "cycle_phase",
    "pair_label",
    "pm_market_id",
    "kalshi_market_id",
    "pm_side",
    "kalshi_side",
    "contracts_n",
    "net_profit_model",
    "capital_used_model",
    "kalshi_order_id",
    "kalshi_client_order_id",
    "polymarket_response_json",
    "pm_error",
    "ks_error",
    "status",
];

pub struct LiveTradesLog {
    lock: Mutex<()>,
}

impl LiveTradesLog {
    pub fn new() -> Result<Self> {
        if let Some(dir) = Path::new(LIVE_TRADES_CSV).parent() {
            fs::create_dir_all(dir).ok();
        }
        if !Path::new(LIVE_TRADES_CSV).exists() {
            let mut w = WriterBuilder::new()
                .has_headers(false)
                .from_path(LIVE_TRADES_CSV)
                .context("创建 live_trades.csv")?;
            w.write_record(HEADER)?;
            w.flush()?;
        }
        Ok(Self {
            lock: Mutex::new(()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &self,
        cycle_id: usize,
        cycle_phase: &str,
        pair_label: &str,
        pm_market_id: &str,
        kalshi_market_id: &str,
        pm_side: &str,
        kalshi_side: &str,
        contracts_n: f64,
        net_profit_model: f64,
        capital_used_model: f64,
        kalshi_order_id: &str,
        kalshi_client_order_id: &str,
        polymarket_response_json: &str,
        pm_error: &str,
        ks_error: &str,
        status: &str,
    ) -> Result<()> {
        let _g = self.lock.lock().expect("live_trades lock");
        let t = Utc::now().to_rfc3339();
        let mut w = WriterBuilder::new()
            .has_headers(false)
            .from_writer(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(LIVE_TRADES_CSV)
                    .context("追加 live_trades.csv")?,
            );
        let row: Vec<String> = vec![
            t,
            cycle_id.to_string(),
            cycle_phase.to_string(),
            pair_label.to_string(),
            pm_market_id.to_string(),
            kalshi_market_id.to_string(),
            pm_side.to_string(),
            kalshi_side.to_string(),
            format!("{:.6}", contracts_n),
            format!("{:.6}", net_profit_model),
            format!("{:.6}", capital_used_model),
            kalshi_order_id.to_string(),
            kalshi_client_order_id.to_string(),
            polymarket_response_json.to_string(),
            pm_error.to_string(),
            ks_error.to_string(),
            status.to_string(),
        ];
        w.write_record(&row)?;
        w.flush()?;
        Ok(())
    }
}
