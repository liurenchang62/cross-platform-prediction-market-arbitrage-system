//! 三时点套利对比工具：识别时 → 执行后 → 结算后。
//!
//! 数据源：`desk_events.jsonl`（原始执行字段）+ Kalshi settlements API + PM closed-positions API。
//! 所有计算在此完成，不影响交易流程。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::DateTime;
use polymarket_client_sdk_v2::data::types::request::ClosedPositionsRequest;
use polymarket_client_sdk_v2::data::types::{ClosedPositionSortBy, SortDirection};
use polymarket_client_sdk_v2::data::Client as DataClient;
use polymarket_client_sdk_v2::types::Decimal;
use reqwest::Client;
use serde::Deserialize;

use crate::kalshi::{self, KalshiAuth};
use crate::pm;

// ─── desk_events 行反序列化 ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DeskRow {
    time_utc_rfc3339: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    kalshi_market_id: String,
    #[serde(default)]
    pm_market_id: String,
    #[serde(default)]
    pm_buy_token_id: String,
    #[serde(default)]
    kalshi_side: String,
    #[serde(default)]
    pm_side: String,
    #[serde(default)]
    kalshi_title: String,
    #[serde(default)]
    pm_title: String,
    // 识别时
    #[serde(default)]
    roi_model: f64,
    // 执行原始数据
    #[serde(default)]
    ks_fill_count: i64,
    #[serde(default)]
    ks_fill_price_cents: i64,
    #[serde(default)]
    pm_fill_tokens: f64,
    #[serde(default)]
    pm_cost_usdc: f64,
    #[serde(default)]
    pm_fee_bps: u32,
    #[serde(default)]
    ks_fee_multiplier: f64,
}

// ─── 执行后预计（不含结算）────────────────────────────────────────────────────

struct ExecEstimate {
    hedged_n: f64,
    ks_cost: f64,
    pm_net_cost: f64,
    pm_fee: f64,
    ks_fee: f64,
    gross_return: f64, // hedged_n - ks_cost - pm_net_cost（套利如期兑现的毛利）
    net_return: f64,   // gross_return - ks_fee - pm_fee
    roi_pct: f64,
}

fn compute_exec_estimate(row: &DeskRow) -> Option<ExecEstimate> {
    let ks_fill = row.ks_fill_count;
    let pm_fill = row.pm_fill_tokens;
    if ks_fill < 1 || pm_fill < 0.5 {
        return None;
    }
    let ks_price = row.ks_fill_price_cents as f64 / 100.0;
    let ks_cost = ks_fill as f64 * ks_price;
    // pm_cost_usdc = making_amount（BUY 订单支付的 USDC，含手续费）
    // 反推净成本：pm_net_cost = cost × 10000 / (10000 + bps)
    let bps = row.pm_fee_bps as f64;
    let pm_net_cost = if bps > 0.0 {
        row.pm_cost_usdc * 10_000.0 / (10_000.0 + bps)
    } else {
        row.pm_cost_usdc
    };
    let pm_fee = row.pm_cost_usdc - pm_net_cost;
    let ks_fee = ks_cost * row.ks_fee_multiplier;
    let hedged_n = (ks_fill as f64).min(pm_fill);
    let pm_avg_net = if pm_fill > 1e-9 { pm_net_cost / pm_fill } else { 0.0 };
    // 预计毛回报：每份对冲最终兑付 $1，减去入场成本（不含费）
    let gross_return = hedged_n * (1.0 - ks_price - pm_avg_net);
    let net_return = gross_return - ks_fee - pm_fee;
    let capital = ks_cost + pm_net_cost;
    let roi_pct = if capital > 1e-9 { net_return / capital * 100.0 } else { 0.0 };
    Some(ExecEstimate { hedged_n, ks_cost, pm_net_cost, pm_fee, ks_fee, gross_return, net_return, roi_pct })
}

// ─── 结算后实际（API 查询）───────────────────────────────────────────────────

struct SettledResult {
    ks_payout_usd: f64,  // Kalshi settlement revenue（美元，未扣手续费）
    ks_fee_usd: f64,     // Kalshi settlement fee_cost（美元）
    pm_realized_pnl: f64,// PM closed_position realized_pnl（API 直接返回）
    total_net: f64,       // ks净利 + pm净利
}

async fn fetch_ks_settled(
    http: &Client,
    ks: &KalshiAuth,
    ticker: &str,
    ks_cost: f64,
) -> Result<Option<(f64, f64)>> {
    // returns (payout, fee)
    let settlements = kalshi::all_settlements_for_ticker(http, ks, ticker).await?;
    if settlements.is_empty() {
        return Ok(None);
    }
    let mut revenue = 0.0_f64;
    let mut fee = 0.0_f64;
    for s in &settlements {
        revenue += s["revenue"].as_i64().unwrap_or(0) as f64 / 100.0;
        fee += s["fee_cost"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
    }
    let _ = ks_cost; // kept for potential future use
    Ok(Some((revenue, fee)))
}

fn dec_f64(d: &Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap_or(0.0)
}

async fn fetch_pm_realized(token_id: &str, user: polymarket_client_sdk_v2::types::Address) -> Result<Option<f64>> {
    let client = DataClient::default();
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
        for pos in &batch {
            // asset (U256) matches pm_buy_token_id (decimal string)
            if pos.asset.to_string() == token_id {
                return Ok(Some(dec_f64(&pos.realized_pnl)));
            }
        }
        if n < 50 { break; }
        offset += 50;
        if offset > 100_000 { break; }
    }
    Ok(None)
}

// ─── 主入口 ──────────────────────────────────────────────────────────────────

pub async fn run_reconcile(desk: &Path, settled: bool) -> Result<()> {
    let raw = std::fs::read_to_string(desk)
        .with_context(|| format!("读取 {}", desk.display()))?;

    let mut rows: Vec<DeskRow> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        match serde_json::from_str::<DeskRow>(line) {
            Ok(r) => rows.push(r),
            Err(_) => continue,
        }
    }

    if rows.is_empty() {
        println!("desk_events.jsonl 无有效记录。");
        return Ok(());
    }

    // 可选：预取结算数据
    let (http_opt, ks_opt, pm_user_opt) = if settled {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        let ks = kalshi::load_kalshi()?;
        let user = pm::polymarket_data_api_user_address()?;
        (Some(http), Some(ks), Some(user))
    } else {
        (None, None, None)
    };

    // 缓存：ticker → settled (payout, fee)
    let mut ks_settle_cache: HashMap<String, Option<(f64, f64)>> = HashMap::new();
    // 缓存：pm_token_id → realized_pnl
    let mut pm_realize_cache: HashMap<String, Option<f64>> = HashMap::new();

    let w_ticker = 26;
    let w_title = 32;

    println!();
    println!("╔═══ 套利三时点回报对比 ═══════════════════════════════════════════════════════════════════════════════════╗");
    println!("║ 字段说明：执行后预计基于实际成交价/数量计算（ks限价为上界）；结算后实际从API取；ROI = 净回报/入场净成本 ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    for row in &rows {
        // 1. Ticker | Market ID
        let ticker_mid = format!("{}|{}", row.kalshi_market_id, row.pm_market_id);
        // 2. 标题
        let ks_title_short = if row.kalshi_title.len() > w_title {
            format!("{}…", &row.kalshi_title[..w_title])
        } else {
            row.kalshi_title.clone()
        };
        let pm_title_short = if row.pm_title.len() > w_title {
            format!("{}…", &row.pm_title[..w_title])
        } else {
            row.pm_title.clone()
        };
        // 3. 时间
        let ts = DateTime::parse_from_rfc3339(&row.time_utc_rfc3339)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|_| row.time_utc_rfc3339.clone());

        println!("──────────────────────────────────────────────────────────────────────────────────────────");
        println!(" 时间        {}", ts);
        println!(" KS Ticker   {}", &row.kalshi_market_id);
        println!(" PM Market   {}", &row.pm_market_id);
        println!(" KS 标题     {} ({})", ks_title_short, row.kalshi_side);
        println!(" PM 标题     {} ({})", pm_title_short, row.pm_side);
        println!(" 状态        {}", row.status);
        println!();

        // 4. 真实下单笔数
        if row.ks_fill_count > 0 || row.pm_fill_tokens > 0.0 {
            println!(" ┌─ 执行数据");
            println!(" │  KS 成交   {} 份 @ ${:.4}/份（限价上界）",
                row.ks_fill_count, row.ks_fill_price_cents as f64 / 100.0);
            println!(" │  PM 成交   {:.4} token，花费 ${:.4} USDC（含手续费）",
                row.pm_fill_tokens,
                row.pm_cost_usdc);
            let pm_avg_gross = if row.pm_fill_tokens > 1e-9 {
                row.pm_cost_usdc / row.pm_fill_tokens
            } else { 0.0 };
            println!(" │  PM 均价   ${:.4}/token（gross，含费）", pm_avg_gross);
        } else {
            println!(" │  KS/PM 均无成交数据（旧格式或未执行）");
        }
        println!();

        // 5. 识别时 ROI
        println!(" ┌─ 时点①：识别时（ladder 估算）");
        println!(" │  ROI 估计  {:.2}%", row.roi_model);
        println!();

        // 6. 执行后预计
        println!(" ┌─ 时点②：执行后预计（按实际成交价重算，假设套利如期结算）");
        match compute_exec_estimate(row) {
            Some(e) => {
                println!(" │  对冲份数  {:.3} 份（min(KS, PM)）", e.hedged_n);
                println!(" │  KS 入场成本  ${:.4}  PM 净入场成本  ${:.4}", e.ks_cost, e.pm_net_cost);
                println!(" │  KS 手续费    ${:.4}  PM 手续费       ${:.4}", e.ks_fee, e.pm_fee);
                println!(" │  预计毛回报  ${:.4}", e.gross_return);
                println!(" │  预计净回报  ${:.4}", e.net_return);
                println!(" │  预计 ROI    {:.2}%", e.roi_pct);
                println!(" │  vs 识别时   {:+.2}% ({:+.4}$净回报)",
                    e.roi_pct - row.roi_model, e.net_return);
            }
            None => {
                println!(" │  执行数据不足（旧格式记录或首腿未成交）");
            }
        }
        println!();

        // 7. 结算后实际
        println!(" ┌─ 时点③：结算后实际");
        if !settled {
            println!(" │  （未请求结算数据，使用 --settled 参数启用）");
        } else if let (Some(http), Some(ks), Some(user)) = (&http_opt, &ks_opt, &pm_user_opt) {
            // KS settlement
            let ticker = &row.kalshi_market_id;
            let exec_est = compute_exec_estimate(row);
            let ks_cost_for_cache = exec_est.as_ref().map(|e| e.ks_cost).unwrap_or(0.0);

            let ks_settled = if let Some(cached) = ks_settle_cache.get(ticker) {
                cached.clone()
            } else {
                let v = fetch_ks_settled(http, ks, ticker, ks_cost_for_cache).await
                    .unwrap_or(None);
                ks_settle_cache.insert(ticker.clone(), v.clone());
                v
            };

            // PM realized
            let pm_token = &row.pm_buy_token_id;
            let pm_realized = if pm_token.is_empty() {
                None
            } else if let Some(cached) = pm_realize_cache.get(pm_token) {
                cached.clone()
            } else {
                let v = fetch_pm_realized(pm_token, *user).await.unwrap_or(None);
                pm_realize_cache.insert(pm_token.clone(), v);
                pm_realize_cache.get(pm_token).and_then(|x| *x)
            };

            match (ks_settled, pm_realized) {
                (Some((ks_payout, ks_fee_actual)), Some(pm_pnl)) => {
                    let exec_est = compute_exec_estimate(row);
                    let ks_cost = exec_est.as_ref().map(|e| e.ks_cost).unwrap_or(0.0);
                    let pm_net_cost = exec_est.as_ref().map(|e| e.pm_net_cost).unwrap_or(0.0);
                    let ks_net = ks_payout - ks_cost - ks_fee_actual;
                    let total_net = ks_net + pm_pnl;
                    let capital = ks_cost + pm_net_cost;
                    let roi_actual = if capital > 1e-9 { total_net / capital * 100.0 } else { 0.0 };
                    println!(" │  KS 结算 payout ${:.4}  fee ${:.4}  KS净 ${:.4}",
                        ks_payout, ks_fee_actual, ks_net);
                    println!(" │  PM realized_pnl ${:.4}（PM Data API 直接返回）", pm_pnl);
                    println!(" │  实际净回报  ${:.4}", total_net);
                    println!(" │  实际 ROI    {:.2}%", roi_actual);
                    if let Some(e) = &compute_exec_estimate(row) {
                        println!(" │  vs 执行后预计   {:+.4}$ 净回报  {:+.2}% ROI",
                            total_net - e.net_return, roi_actual - e.roi_pct);
                    }
                }
                (Some((ks_payout, ks_fee_actual)), None) => {
                    println!(" │  KS 结算 payout ${:.4}  fee ${:.4}", ks_payout, ks_fee_actual);
                    println!(" │  PM realized_pnl：未找到（市场可能尚未结算）");
                }
                (None, Some(pm_pnl)) => {
                    println!(" │  KS：无结算记录（尚未结算或 ticker 不匹配）");
                    println!(" │  PM realized_pnl ${:.4}", pm_pnl);
                }
                (None, None) => {
                    println!(" │  KS 和 PM 均无结算记录（市场可能尚未结算）");
                }
            }
        }
        println!();
    }

    let _ = (w_ticker, w_title); // suppress unused warning
    Ok(())
}
