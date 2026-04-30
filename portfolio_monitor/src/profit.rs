//! 全历史利润相关汇总（Kalshi：结算已实现、fills 手续费；Polymarket：已关仓已实现、开仓未实现 cashPnl、CLOB 映射内成交费）。

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use polymarket_client_sdk_v2::data::types::request::{ClosedPositionsRequest, PositionsRequest};
use polymarket_client_sdk_v2::data::types::{ClosedPositionSortBy, SortDirection};
use polymarket_client_sdk_v2::data::Client as DataClient;
use polymarket_client_sdk_v2::types::Decimal;
use reqwest::Client;
use serde_json::Value;

use crate::kalshi::{self, KalshiAuth};
use crate::mapping::load_pairs_from_desk_events;
use crate::pm;

fn http() -> Result<Client> {
    Ok(Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()?)
}

fn dec(d: &Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap_or(0.0)
}

async fn pm_open_positions(
    user: polymarket_client_sdk_v2::types::Address,
) -> Result<Vec<polymarket_client_sdk_v2::data::types::response::Position>> {
    let client = DataClient::default();
    let mut all = Vec::new();
    let mut offset: i32 = 0;
    let zero = Decimal::from(0);
    loop {
        let req = PositionsRequest::builder()
            .user(user)
            .limit(500)
            .context("positions limit")?
            .offset(offset)
            .context("positions offset")?
            .size_threshold(zero)
            .build();
        let batch = client
            .positions(&req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let n = batch.len();
        if n == 0 {
            break;
        }
        all.extend(batch);
        if n < 500 {
            break;
        }
        offset += 500;
        if offset > 100_000 {
            break;
        }
    }
    Ok(all)
}

async fn pm_closed_positions(
    user: polymarket_client_sdk_v2::types::Address,
) -> Result<Vec<polymarket_client_sdk_v2::data::types::response::ClosedPosition>> {
    let client = DataClient::default();
    let mut all = Vec::new();
    let mut offset: i32 = 0;
    loop {
        let req = ClosedPositionsRequest::builder()
            .user(user)
            .limit(50)
            .context("closed limit")?
            .offset(offset)
            .context("closed offset")?
            .sort_by(ClosedPositionSortBy::Timestamp)
            .sort_direction(SortDirection::Asc)
            .build();
        let batch = client
            .closed_positions(&req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let n = batch.len();
        if n == 0 {
            break;
        }
        all.extend(batch);
        if n < 50 {
            break;
        }
        offset += 50;
        if offset > 100_000 {
            break;
        }
    }
    Ok(all)
}

async fn ks_tickers_merged(
    http: &Client,
    ks: &KalshiAuth,
    desk: &Path,
) -> Result<Vec<String>> {
    let pairs = load_pairs_from_desk_events(desk, false)?;
    let mut set: HashSet<String> = pairs.iter().map(|p| p.kalshi_ticker.clone()).collect();
    let sign_p = "/trade-api/v2/portfolio/positions";
    let url = format!("{}/portfolio/positions?limit=1000", ks.base);
    let body = kalshi::signed_get(http, ks, &url, sign_p).await?;
    let v: Value = serde_json::from_str(&body)?;
    for p in v["market_positions"].as_array().cloned().unwrap_or_default() {
        if let Some(t) = p["ticker"].as_str() {
            if !t.is_empty() {
                set.insert(t.to_string());
            }
        }
    }
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    Ok(v)
}

pub async fn run_profit(desk: &Path) -> Result<()> {
    let w = 56;
    println!("{}", "═".repeat(w));
    println!("{:^w$}", "利润统计（全历史）", w = w);
    println!("{}  {}", "═".repeat(w), Utc::now().to_rfc3339());
    println!();

    let http = http()?;
    let ks = kalshi::load_kalshi()?;
    let pm_clob = pm::connect().await?;
    let pm_user = pm::polymarket_data_api_user_address()?;

    let tickers = ks_tickers_merged(&http, &ks, desk).await?;
    let mut ks_settle_real = 0.0_f64;
    let mut ks_fees_fills = 0.0_f64;
    for t in &tickers {
        for s in kalshi::all_settlements_for_ticker(&http, &ks, t).await? {
            let rev = s["revenue"].as_i64().unwrap_or(0) as f64 / 100.0;
            let fee = s["fee_cost"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
            ks_settle_real += rev - fee;
        }
        for f in kalshi::all_fills_for_ticker(&http, &ks, t).await? {
            ks_fees_fills += f["fee_cost"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
        }
    }

    let pairs = load_pairs_from_desk_events(desk, false)?;
    let mut pm_fees = 0.0_f64;
    for p in &pairs {
        if !pm::pm_clob_token_id_valid(&p.pm_buy_token_id) {
            continue;
        }
        let trades = pm::all_trades_for_token(&pm_clob, &p.pm_buy_token_id).await?;
        let (_, f, _, _) = pm::trades_net_ledger(&trades);
        pm_fees += f;
    }

    let open = pm_open_positions(pm_user).await?;
    let mut pm_unreal = 0.0_f64;
    let mut pm_realized_open_leg = 0.0_f64;
    for pos in &open {
        if dec(&pos.size).abs() < 1e-9 {
            continue;
        }
        pm_unreal += dec(&pos.cash_pnl);
        pm_realized_open_leg += dec(&pos.realized_pnl);
    }

    let closed = pm_closed_positions(pm_user).await?;
    let mut pm_closed_real = 0.0_f64;
    for c in &closed {
        pm_closed_real += dec(&c.realized_pnl);
    }

    println!("Kalshi");
    println!("  已实现利润（官方 settlements 累加） {:>12} USD", format!("{:.2}", ks_settle_real));
    println!("  总手续费（fills 官方 fee_cost 累加） {:>12} USD", format!("{:.2}", ks_fees_fills));
    println!();
    println!("Polymarket");
    println!("  未实现利润（Data API 开仓 cashPnl 合计） {:>12} USD", format!("{:.2}", pm_unreal));
    println!("  已实现（已关仓 closed-positions 合计） {:>12} USD", format!("{:.2}", pm_closed_real));
    println!("  已实现（开仓头寸内 realized_pnl 合计） {:>12} USD", format!("{:.2}", pm_realized_open_leg));
    println!("  总手续费（映射内 CLOB trades 估算）     {:>12} USD", format!("{:.2}", pm_fees));
    println!();
    println!("{}", "═".repeat(w));
    Ok(())
}
