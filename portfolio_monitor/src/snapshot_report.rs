//! 统一快照：Kalshi / Polymarket 同构摘要 + 映射对冲表。

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::types::Decimal;
use reqwest::Client;
use serde_json::Value;

use crate::gamma;
use crate::kalshi;
use crate::mapping::load_pairs_merged_with_desk_at;
use crate::pm;

fn http() -> Result<Client> {
    Ok(Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

fn dec_f64(d: &Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap_or(0.0)
}

async fn pm_data_positions(
    user: polymarket_client_sdk::types::Address,
) -> Result<Vec<polymarket_client_sdk::data::types::response::Position>> {
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
            .map_err(|e| anyhow::anyhow!("Data API positions: {}", e))?;
        let n = batch.len();
        if n == 0 {
            break;
        }
        all.extend(batch);
        if n < 500 {
            break;
        }
        offset += 500;
        if offset > 10_000 {
            break;
        }
    }
    Ok(all)
}

fn ks_position_side_upper(p: &Value) -> &'static str {
    match p["side"].as_str() {
        Some(s) if s.eq_ignore_ascii_case("no") => "NO",
        _ => "YES",
    }
}

pub async fn run_snapshot(config: &Path, desk_jsonl: &Path) -> Result<()> {
    let http = http()?;
    let mut pairs = load_pairs_merged_with_desk_at(config, Some(desk_jsonl), true)?;
    let ks = kalshi::load_kalshi()?;
    let pm_clob = pm::connect().await?;
    let pm_user = pm::polymarket_data_api_user_address()?;

    let ks_cash = kalshi::balance_cents(&http, &ks).await? as f64 / 100.0;
    let pm_col = pm::collateral_balance(&pm_clob).await?;
    let pm_cash = pm::clob_micro_to_usd(&pm_col);

    let sign_p = "/trade-api/v2/portfolio/positions";
    let url = format!("{}/portfolio/positions?limit=1000", ks.base);
    let body = kalshi::signed_get(&http, &ks, &url, sign_p).await?;
    let v: Value = serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("positions: {}", e))?;
    let arr = v["market_positions"].as_array().cloned().unwrap_or_default();

    let mut ks_nonzero: Vec<Value> = Vec::new();
    for p in arr {
        let fp = p["position_fp"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| p["position_fp"].as_f64())
            .unwrap_or(0.0);
        if fp.abs() >= 1e-6 {
            ks_nonzero.push(p);
        }
    }

    let mut ks_mtm = 0.0_f64;
    let mut ks_detail: Vec<(String, String, f64)> = Vec::new();
    let mut events: HashSet<String> = HashSet::new();
    for p in &ks_nonzero {
        let tick = p["ticker"].as_str().unwrap_or("(ticker)").to_string();
        let fp = p["position_fp"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| p["position_fp"].as_f64())
            .unwrap_or(0.0);
        let side_u = ks_position_side_upper(p);
        let contracts = fp.abs();
        let exp = p["market_exposure_dollars"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| p["market_exposure_dollars"].as_f64());
        let mid = kalshi::fetch_orderbook_json_public(&http, &ks.base, &tick)
            .await
            .ok()
            .as_ref()
            .and_then(|b| kalshi::orderbook_mid_for_side(b, side_u));
        let row_val = match mid {
            Some(m) => contracts * m,
            None => exp.unwrap_or(0.0),
        };
        ks_mtm += row_val;
        if let Some(ev) = p["event_ticker"].as_str() {
            if !ev.is_empty() {
                events.insert(ev.to_string());
            }
        }
        ks_detail.push((tick, side_u.to_string(), fp));
    }
    let ks_markets_n = if events.is_empty() {
        ks_nonzero
            .iter()
            .filter_map(|p| p["ticker"].as_str().map(ToString::to_string))
            .collect::<HashSet<_>>()
            .len()
    } else {
        events.len()
    };
    let ks_tokens_n = ks_nonzero.len();
    let ks_total = ks_cash + ks_mtm;

    let pm_positions = pm_data_positions(pm_user).await?;
    let mut pm_mtm = 0.0_f64;
    let mut pm_markets: HashSet<String> = HashSet::new();
    let mut pm_detail: Vec<(String, String, f64)> = Vec::new();
    for pos in &pm_positions {
        let sz = dec_f64(&pos.size);
        if sz.abs() < 1e-9 {
            continue;
        }
        pm_mtm += dec_f64(&pos.current_value);
        pm_markets.insert(format!("{:?}", pos.condition_id));
        let aid = format!("{}", pos.asset);
        pm_detail.push((aid, pos.outcome.clone(), sz));
    }
    let pm_markets_n = pm_markets.len();
    let pm_tokens_n = pm_detail.len();
    let pm_total = pm_cash + pm_mtm;

    let w = 62usize;
    println!("{}", "═".repeat(w));
    println!("{:^w$}", "持仓快照 · 统一格式", w = w);
    println!("{}  UTC {}", "═".repeat(w), Utc::now().to_rfc3339());
    println!(
        "映射 {} | desk {}",
        config.display(),
        desk_jsonl.display()
    );
    println!();

    let block = |title: &str,
                 total: f64,
                 cash: f64,
                 posv: f64,
                 mk: usize,
                 tok: usize,
                 detail: &[(String, String, f64)]| {
        println!("{}", "─".repeat(w));
        println!("{:^w$}", title, w = w);
        println!("{}", "─".repeat(w));
        println!(" 总资产           {:>18} USD", format!("{:.2}", total));
        println!(" 现金余额         {:>18} USD", format!("{:.2}", cash));
        println!(" 持仓价值         {:>18} USD", format!("{:.2}", posv));
        println!(" 持仓市场数       {:>18}", mk);
        println!(" 持仓 token 数    {:>18}", tok);
        println!(" 标识               种类          数量");
        println!(" {}", "·".repeat(50));
        if detail.is_empty() {
            println!(" （无）");
        } else {
            for (id, kind, q) in detail {
                println!(" {:<20} {:<12} {:>12.6}", id, kind, q);
            }
        }
        println!();
    };

    block(
        "Kalshi",
        ks_total,
        ks_cash,
        ks_mtm,
        ks_markets_n,
        ks_tokens_n,
        &ks_detail
            .iter()
            .map(|(a, b, c)| (a.clone(), b.clone(), *c))
            .collect::<Vec<_>>(),
    );

    block(
        "Polymarket",
        pm_total,
        pm_cash,
        pm_mtm,
        pm_markets_n,
        pm_tokens_n,
        &pm_detail,
    );

    println!("{}", "─".repeat(w));
    println!("{:^w$}", "当前持仓 · 映射对冲对", w = w);
    println!("{}", "─".repeat(w));
    println!(
        "{:<42} {:>12} {:>10} {:>12} {:>12}",
        "kalshi_ticker",
        "pm_market_id",
        "对冲份数",
        "Kalshi到期",
        "PM到期"
    );
    println!("{}", "─".repeat(92));

    for pair in &mut pairs {
        if !pm::pm_clob_token_id_valid(&pair.pm_buy_token_id) {
            continue;
        }
        let pos = kalshi::positions_for_ticker(&http, &ks, &pair.kalshi_ticker).await?;
        let ks_fp = pos
            .as_ref()
            .and_then(|p| p["position_fp"].as_str()?.parse::<f64>().ok())
            .or_else(|| pos.as_ref().and_then(|p| p["position_fp"].as_f64()))
            .unwrap_or(0.0);
        let cond = pm::conditional_shares(&pm_clob, &pair.pm_buy_token_id).await?;
        let pm_sh = pm::clob_micro_to_usd(&cond);
        let h = ks_fp.abs().min(pm_sh.abs());
        if h < 1e-9 {
            continue;
        }
        let ks_e = kalshi::market_expiry_display(&http, &ks.base, &pair.kalshi_ticker).await;
        let pm_e = gamma::pm_market_expiry_display(&http, &pair.pm_market_id).await;
        println!(
            "{:<42} {:>12} {:>10.4} {:>12} {:>12}",
            pair.kalshi_ticker,
            pair.pm_market_id,
            h,
            clip(&ks_e, 12),
            clip(&pm_e, 12)
        );
    }

    println!("{}", "═".repeat(w));
    Ok(())
}

fn clip(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    t.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}
