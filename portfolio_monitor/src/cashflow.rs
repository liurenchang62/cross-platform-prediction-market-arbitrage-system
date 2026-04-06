//! 回望 N 天双平台现金流 → `out_dir/cashflow.csv`（每次覆盖）。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use csv::Writer;
use polymarket_client_sdk::data::types::request::ActivityRequest;
use polymarket_client_sdk::data::types::response::Activity;
use polymarket_client_sdk::data::types::{ActivitySortBy, ActivityType, Side as PmDataSide, SortDirection};
use polymarket_client_sdk::data::Client as DataClient;
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

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|x| x.with_timezone(&Utc))
}

/// Returns (sorted tickers, map of ticker→title for those already stored in desk_events).
async fn ks_merged_tickers(
    http: &Client,
    ks: &KalshiAuth,
    desk: &Path,
) -> Result<(Vec<String>, HashMap<String, String>)> {
    let pairs = load_pairs_from_desk_events(desk, false)?;

    // Collect titles already stored in the documenting files — no API call needed for these.
    let known_titles: HashMap<String, String> = pairs
        .iter()
        .filter_map(|p| {
            p.kalshi_title
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| (p.kalshi_ticker.clone(), t.to_string()))
        })
        .collect();

    let mut set: HashSet<String> = pairs.iter().map(|p| p.kalshi_ticker.clone()).collect();
    let sign_p = "/trade-api/v2/portfolio/positions";
    let url = format!("{}/portfolio/positions?limit=1000", ks.base);
    let body = kalshi::signed_get(http, &ks, &url, sign_p).await?;
    let v: Value = serde_json::from_str(&body)?;
    for p in v["market_positions"].as_array().cloned().unwrap_or_default() {
        if let Some(t) = p["ticker"].as_str() {
            if !t.is_empty() {
                set.insert(t.to_string());
            }
        }
    }
    let mut tickers: Vec<String> = set.into_iter().collect();
    tickers.sort();
    Ok((tickers, known_titles))
}

async fn fetch_pm_activity(user: polymarket_client_sdk::types::Address) -> Result<Vec<Activity>> {
    let client = DataClient::default();
    let mut all = Vec::new();
    let mut offset: i32 = 0;
    loop {
        let req = ActivityRequest::builder()
            .user(user)
            .limit(500)
            .context("activity limit")?
            .offset(offset)
            .context("activity offset")?
            .sort_by(ActivitySortBy::Timestamp)
            .sort_direction(SortDirection::Asc)
            .build();
        let batch = client
            .activity(&req)
            .await
            .map_err(|e| anyhow::anyhow!("Data API activity: {}", e))?;
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

#[derive(Clone)]
struct Row {
    t: DateTime<Utc>,
    platform: &'static str,
    behavior: String,
    flow: String,
    amount: f64,
    note: String,
}

fn pm_activity_row(act: &Activity) -> Option<Row> {
    let ts = Utc.timestamp_opt(act.timestamp, 0).single()?;
    let raw = act.usdc_size.to_string().parse::<f64>().unwrap_or(0.0);
    let (behavior, flow, amount) = match act.activity_type {
        ActivityType::Trade => match act.side.as_ref() {
            Some(PmDataSide::Buy) => ("买入", "流出", raw.abs()),
            Some(PmDataSide::Sell) => ("卖出", "流入", raw.abs()),
            _ => return None,
        },
        ActivityType::Redeem => {
            if raw >= 0.0 {
                ("赎回", "流入", raw.abs())
            } else {
                ("赎回", "流出", (-raw).abs())
            }
        }
        ActivityType::Merge => ("合并", "流入", raw.abs()),
        ActivityType::Split => ("拆分", "流出", raw.abs()),
        ActivityType::Reward | ActivityType::MakerRebate => ("返点", "流入", raw.abs()),
        ActivityType::Yield => ("收益", "流入", raw.abs()),
        ActivityType::Conversion => ("转换", "流入", raw.abs()),
        ActivityType::Unknown(_) => return None,
        _ => ("其他", "流入", raw.abs()),
    };
    Some(Row {
        t: ts,
        platform: "Polymarket",
        behavior: behavior.to_string(),
        flow: flow.to_string(),
        amount,
        note: act.title.as_deref().unwrap_or("").to_string(),
    })
}

/// 生成现金流前尝试触发 PM 赎回；若有赎回成功，等待 Data API 索引。
/// 任何错误都打印警告后静默跳过，不阻塞现金流生成。
async fn try_trigger_pm_redeem(http: &Client) {
    let rpc = crate::pm_relayer_automation::polygon_rpc_for_automation();
    let relay = crate::pm_relayer_automation::relayer_url_for_automation();
    println!("检查 PM 可赎回头寸…");
    match crate::pm_relayer_automation::desk_hydrate_pm_proxy(http, &rpc, &relay, false).await {
        Ok(rep) => {
            if rep.redeem_ok > 0 {
                println!(
                    "✓ PM {} 笔赎回已上链，等待 Data API 索引（5s）…",
                    rep.redeem_ok
                );
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            } else if rep.redeem_fail > 0 {
                eprintln!("⚠ PM {} 笔赎回失败，继续生成现金流", rep.redeem_fail);
            } else if rep.redeem_skipped > 0 {
                println!("ℹ {} 笔 PM 头寸尚未结算，跳过赎回", rep.redeem_skipped);
            } else {
                println!("ℹ 无 PM 头寸可赎回");
            }
        }
        Err(e) => {
            eprintln!("⚠ PM 赎回触发失败（{}），继续生成现金流", e);
        }
    }
}

pub async fn run_cashflow(desk: &Path, out_dir: &Path, lookback_days: u32) -> Result<()> {
    if lookback_days == 0 {
        anyhow::bail!("回望天数须 ≥ 1");
    }
    let cutoff = Utc::now() - chrono::Duration::days(lookback_days as i64);
    std::fs::create_dir_all(out_dir).ok();

    let http = http()?;

    // Step 1: 触发 PM 赎回；若有新赎回成功则稍等 API 更新后再拉取活动记录
    try_trigger_pm_redeem(&http).await;

    let ks = kalshi::load_kalshi()?;
    let pm_user = pm::polymarket_data_api_user_address()?;
    let (tickers, mut ticker_title) = ks_merged_tickers(&http, &ks, desk).await?;

    // For tickers without a stored title, try the Kalshi API; fall back to raw ticker.
    for ticker in &tickers {
        if !ticker_title.contains_key(ticker) {
            let title = kalshi::market_title(&http, &ks, ticker.as_str())
                .await
                .unwrap_or_else(|_| ticker.clone());
            ticker_title.insert(ticker.clone(), title);
        }
    }

    let mut rows: Vec<Row> = Vec::new();

    for ticker in &tickers {
        let note = ticker_title.get(ticker).cloned().unwrap_or_else(|| ticker.clone());
        let fills = kalshi::all_fills_for_ticker(&http, &ks, ticker.as_str()).await?;
        for f in fills {
            let Some(ts) = parse_rfc3339(f["created_time"].as_str().unwrap_or("")) else {
                continue;
            };
            if ts < cutoff {
                continue;
            }
            let behavior = match f["action"].as_str() {
                Some("buy") => "买入",
                Some("sell") => "卖出",
                _ => continue,
            };
            let count = f["count_fp"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
            let side = f["side"].as_str().unwrap_or("yes");
            let yes_p = f["yes_price_dollars"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
            let no_p = f["no_price_dollars"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
            let unit = if side == "yes" { yes_p } else { no_p };
            let fee = f["fee_cost"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
            let leg = count * unit;
            let (flow, amount) = match f["action"].as_str() {
                Some("buy") => ("流出".to_string(), leg + fee),
                Some("sell") => ("流入".to_string(), (leg - fee).max(0.0)),
                _ => continue,
            };
            rows.push(Row {
                t: ts,
                platform: "Kalshi",
                behavior: behavior.to_string(),
                flow,
                amount,
                note: note.clone(),
            });
        }

        for s in kalshi::all_settlements_for_ticker(&http, &ks, ticker.as_str()).await? {
            let Some(ts) = parse_rfc3339(s["settled_time"].as_str().unwrap_or("")) else {
                continue;
            };
            if ts < cutoff {
                continue;
            }
            let rev = s["revenue"].as_i64().unwrap_or(0) as f64 / 100.0;
            let fee = s["fee_cost"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
            let net = rev - fee;
            rows.push(Row {
                t: ts,
                platform: "Kalshi",
                behavior: "结算流回".to_string(),
                flow: (if net >= 0.0 { "流入" } else { "流出" }).to_string(),
                amount: net.abs(),
                note: note.clone(),
            });
        }
    }

    for act in fetch_pm_activity(pm_user).await? {
        let Some(ts) = Utc.timestamp_opt(act.timestamp, 0).single() else {
            continue;
        };
        if ts < cutoff {
            continue;
        }
        if let Some(r) = pm_activity_row(&act) {
            rows.push(r);
        }
    }

    rows.sort_by_key(|r| r.t);

    let out_csv = out_dir.join("cashflow.csv");
    let mut w = Writer::from_path(&out_csv)?;
    w.write_record([
        "time_utc",
        "platform",
        "behavior",
        "flow",
        "amount_usd",
        "note",
    ])?;
    for r in rows {
        w.write_record([
            &r.t.to_rfc3339(),
            r.platform,
            &r.behavior,
            &r.flow,
            &format!("{:.6}", r.amount),
            &r.note,
        ])?;
    }
    w.flush()?;
    println!("已写入 {:?}（回望 {} 天）", out_csv, lookback_days);
    Ok(())
}
