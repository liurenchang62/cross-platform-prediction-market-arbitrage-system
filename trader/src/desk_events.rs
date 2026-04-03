//! 实盘成交一行一条 JSON（`logs/desk_events.jsonl`），供 `portfolio-turnover` 等组合工具消费。
//! 固定字段含 `time_utc_rfc3339`、`trading_session_utc_date`（YYYY-MM-DD）、`session_cycle_id`、`global_tick`、`status`、`pair_label` 等。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};

use crate::system_params::DESK_EVENTS_JSONL;

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
    ) -> Result<()> {
        let _g = self.lock.lock().expect("desk_events lock");
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
