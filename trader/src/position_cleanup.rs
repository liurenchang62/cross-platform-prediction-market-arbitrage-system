//! 仓位清理模块：在程序启动（拉取市场前）以及每次全量重建前运行，处理三类场景：
//!
//! - **A 孤仓**：Kalshi 有持仓但无 desk_events 配对记录，或 desk_events 对中一侧已清零另一侧仍有余额 → 直接平仓。
//! - **B 一侧已结算**：Kalshi 或 Polymarket 一腿已结算时，关闭另一侧（锁定盈利或回收残值）。
//! - **C 高置信止损**：双侧均开仓，但我方持有一侧当前 bid ≤ 0.10（市场 90%+ 置信反向）→ 平仓回收残值。
//!
//! 所有平仓操作均直接执行，无需人工确认。

use std::collections::HashMap;

use reqwest::Client as HttpClient;
use serde_json::Value;
use tokio::time::{sleep, Duration};

use crate::kalshi_trade::{
    fetch_kalshi_market_settlement_result, fetch_kalshi_open_positions_list,
    fetch_market_orderbook_json, place_sell_ioc, KalshiOpenPos, KalshiTradeConfig,
};
use crate::polymarket_trade::{clob_conditional_micro_to_shares, PolymarketClobTrader};
use crate::system_params::{DESK_EVENTS_JSONL, STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS};

/// 我方持有侧 bid 低于此阈值时视为「高置信亏损侧」（0.90 置信 = 对立面 ≥ 0.90 → 我方 ≤ 0.10）。
const C_LOSING_BID_THRESHOLD: f64 = 0.10;
/// 最小有意义回收金额（美元），低于此值不发单。
const MIN_RECOVERY_USD: f64 = 0.10;

// ─── 内部数据结构 ────────────────────────────────────────────────────────────

struct KnownPair {
    ks_ticker: String,
    pm_token_id: String,
    pm_side: String,   // "YES" 或 "NO"（我方买入方向）
}

/// 从 `desk_events.jsonl` 加载所有有效套利对（按 (ks_ticker, pm_token_id) 去重，status 非 KS_FAIL/INIT）。
fn load_known_pairs(path: &str) -> Vec<KnownPair> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut seen: HashMap<(String, String), KnownPair> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let status = v["status"].as_str().unwrap_or("");
        if status == "KS_FAIL" || status == "INIT" {
            continue;
        }
        let ks_ticker = v["kalshi_market_id"].as_str().unwrap_or("").to_string();
        let pm_token_id = v["pm_buy_token_id"].as_str().unwrap_or("").to_string();
        if ks_ticker.is_empty() || pm_token_id.is_empty() {
            continue;
        }
        let pm_side = v["pm_side"].as_str().unwrap_or("YES").to_string();
        seen.entry((ks_ticker.clone(), pm_token_id.clone()))
            .or_insert_with(|| KnownPair { ks_ticker, pm_token_id, pm_side });
    }
    seen.into_values().collect()
}

// ─── 辅助 API 调用 ──────────────────────────────────────────────────────────

/// Gamma `GET /markets?clob_token_ids=...`：包含已关闭/已结算的市场（区别于主路径的 `parse_polymarket_gamma_market_row`）。
async fn fetch_gamma_raw_for_token(http: &HttpClient, token_id: &str) -> Option<Value> {
    let resp = http
        .get("https://gamma-api.polymarket.com/markets")
        .query(&[("clob_token_ids", token_id), ("limit", "1")])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let arr: Vec<Value> = resp.json().await.ok()?;
    arr.into_iter().next()
}

/// 解析 PM Gamma market JSON（含已结算）→ `Some("yes")` 或 `Some("no")` 表示 YES/NO 胜出；未结算返回 `None`。
fn pm_settlement_result(market_json: &Value) -> Option<String> {
    let closed = market_json["closed"].as_bool().unwrap_or(false);
    let resolved = market_json["umaResolutionStatus"].as_str() == Some("resolved");
    if !closed && !resolved {
        return None;
    }
    let prices_str = market_json["outcomePrices"].as_str()?;
    let prices: Vec<String> = serde_json::from_str(prices_str).ok()?;
    let yes_price: f64 = prices.first()?.parse().ok()?;
    if yes_price >= 0.9 {
        Some("yes".to_string())
    } else if yes_price <= 0.1 {
        Some("no".to_string())
    } else {
        None // 模糊结果，不处理
    }
}

/// PM CLOB `GET /book?token_id=...`：返回该 token 当前最优买价（美元，0–1）。
async fn pm_best_bid(http: &HttpClient, token_id: &str) -> Option<f64> {
    let resp = http
        .get("https://clob.polymarket.com/book")
        .query(&[("token_id", token_id)])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let book: Value = resp.json().await.ok()?;
    book["bids"]
        .as_array()?
        .iter()
        .filter_map(|b| {
            b["price"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| b["price"].as_f64())
        })
        .fold(None::<f64>, |acc, p| Some(acc.map_or(p, |a: f64| a.max(p))))
}

/// Kalshi 订单簿中我方持有侧的当前最优买价（美元，0–1）。
fn kalshi_best_bid_for_side(book: &Value, side: &str) -> Option<f64> {
    let orderbook = book.get("orderbook_fp").or_else(|| book.get("orderbook"))?;
    let key = match side {
        "YES" => "yes_dollars",
        "NO" => "no_dollars",
        _ => return None,
    };
    let arr = orderbook.get(key)?.as_array()?;
    let mut best: Option<f64> = None;
    for entry in arr {
        let p = entry
            .get(0)
            .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))?;
        let s = entry
            .get(1)
            .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0.0);
        if s > 0.0 && p > 0.0 {
            best = Some(best.map_or(p, |b: f64| b.max(p)));
        }
    }
    best
}

// ─── 辅助执行 ───────────────────────────────────────────────────────────────

async fn close_pm_side(pm_trader: &PolymarketClobTrader, token_id: &str, shares: f64, label: &str) {
    let n = shares.floor() as i64;
    if n < 1 {
        return;
    }
    match pm_trader.sell_outcome_token_fak(token_id, n).await {
        Ok(r) => println!(
            "      ✅ {} PM FAK 卖出 n={} | success={} making={} taking={}",
            label, n, r.success, r.making_amount, r.taking_amount
        ),
        Err(e) => eprintln!("      ⚠️ {} PM 卖出失败: {}", label, e),
    }
    sleep(Duration::from_millis(300)).await;
}

async fn close_ks_side(
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    ticker: &str,
    side: &str,
    contracts: f64,
    label: &str,
) {
    let count = contracts.round() as i64;
    if count < 1 {
        return;
    }
    let client_id = format!("cleanup_{}_{}", label.replace(' ', "_"), &ticker[..ticker.len().min(20)]);
    match place_sell_ioc(
        ks_http,
        kalshi_cfg,
        ticker,
        side,
        count,
        STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS,
        &client_id,
    )
    .await
    {
        Ok(r) => println!(
            "      ✅ {} KS IOC 卖出 fill={} order_id={}",
            label, r.fill_count, r.order_id
        ),
        Err(e) => eprintln!("      ⚠️ {} KS 卖出失败: {}", label, e),
    }
    sleep(Duration::from_millis(300)).await;
}

// ─── 主函数 ─────────────────────────────────────────────────────────────────

/// 仓位清理：在**拉取市场前**调用（程序启动时 & 每次 90 分钟全量重建前）。
///
/// 三个场景均直接执行，不请求人工确认。
pub async fn run_position_cleanup(
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
) {
    println!("\n🔎 ── 仓位清理扫描 ──");

    // 1. 加载历史套利对
    let known_pairs = load_known_pairs(DESK_EVENTS_JSONL);
    println!("   desk_events 套利对（去重）: {} 个", known_pairs.len());

    // 2. 拉取 KS 全量持仓
    let ks_pos_map: HashMap<String, KalshiOpenPos> =
        match fetch_kalshi_open_positions_list(ks_http, kalshi_cfg).await {
            Ok(list) => list.into_iter().map(|p| (p.ticker.clone(), p)).collect(),
            Err(e) => {
                eprintln!("   ⚠️ 拉取 Kalshi 持仓失败，跳过清理: {}", e);
                return;
            }
        };
    println!("   Kalshi 有持仓 ticker: {}", ks_pos_map.len());

    // 3. Scenario A – Kalshi 孤仓（ticker 不在任何 desk_events 对中）
    let known_tickers: std::collections::HashSet<&str> =
        known_pairs.iter().map(|p| p.ks_ticker.as_str()).collect();
    for (ticker, pos) in &ks_pos_map {
        if !known_tickers.contains(ticker.as_str()) {
            println!(
                "   🔴 [A-KS孤仓] {} {} {:.2}份 — 无 desk_events 配对记录 → 平仓",
                ticker, pos.side, pos.contracts
            );
            close_ks_side(ks_http, kalshi_cfg, ticker, &pos.side, pos.contracts, "A-KS孤仓").await;
        }
    }

    // 4. 遍历每个已知套利对
    for pair in &known_pairs {
        // 拉取 PM 余额
        let pm_balance = match pm_trader
            .fetch_conditional_balance_shares(&pair.pm_token_id)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "   ⚠️ 拉取 PM 余额 token={}… 失败: {}",
                    &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                    e
                );
                continue;
            }
        };
        let pm_shares = clob_conditional_micro_to_shares(&pm_balance);
        let ks_opt = ks_pos_map.get(&pair.ks_ticker);
        let has_ks = ks_opt.map(|p| p.contracts >= 0.5).unwrap_or(false);
        let has_pm = pm_shares >= 0.5;

        if !has_ks && !has_pm {
            continue; // 双侧均已清零，无需处理
        }

        // ── Scenario A: 单侧孤仓 ──────────────────────────────────────────
        if has_pm && !has_ks {
            // 先查 bid：无买单时（已结算为0或极度流动性缺失）不盲目下单
            let pm_bid = pm_best_bid(ks_http, &pair.pm_token_id).await;
            let expected = pm_bid.unwrap_or(0.0) * pm_shares;
            if pm_bid.unwrap_or(0.0) < 0.001 {
                println!(
                    "   🟡 [A-PM孤仓] token={}… {} {:.2}份 | KS {} 无持仓 | bid≈0 无流动性（可能已结算为0），跳过",
                    &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                    pair.pm_side,
                    pm_shares,
                    pair.ks_ticker
                );
            } else {
                println!(
                    "   🟡 [A-PM孤仓] token={}… {} {:.2}份 | KS {} 无持仓 | bid={:.3} 预期≈${:.3} → 平仓",
                    &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                    pair.pm_side,
                    pm_shares,
                    pair.ks_ticker,
                    pm_bid.unwrap_or(0.0),
                    expected
                );
                close_pm_side(pm_trader, &pair.pm_token_id, pm_shares, "A-PM孤仓").await;
            }
            continue;
        }

        if has_ks && !has_pm {
            let pos = ks_opt.unwrap();
            println!(
                "   🟡 [A-KS孤仓] {} {} {:.2}份 | PM token={}… 无余额 → 平仓",
                pair.ks_ticker,
                pos.side,
                pos.contracts,
                &pair.pm_token_id[..pair.pm_token_id.len().min(16)]
            );
            close_ks_side(ks_http, kalshi_cfg, &pair.ks_ticker, &pos.side, pos.contracts, "A-KS孤仓").await;
            continue;
        }

        // 双侧均有持仓 → Scenario B & C
        let pos = ks_opt.unwrap();

        // ── Scenario B: Kalshi 已结算 ─────────────────────────────────────
        // 无论 KS 哪侧胜出，都尝试卖 PM：
        // - KS "yes"（我方 KS NO 输）→ PM YES 应 ≈ $1，立即锁定盈利
        // - KS "no"（我方 KS NO 赢）→ PM YES ≈ $0，回收残值
        match fetch_kalshi_market_settlement_result(ks_http, kalshi_cfg, &pair.ks_ticker).await {
            Ok(Some(ks_result)) => {
                let pm_bid = pm_best_bid(ks_http, &pair.pm_token_id).await;
                let expected = pm_bid.unwrap_or(0.0) * pm_shares;
                println!(
                    "   📌 [B-KS结算] {} result={} | PM token={}… {:.2}份 bid={:.3} 预期回收≈${:.3}",
                    pair.ks_ticker,
                    ks_result,
                    &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                    pm_shares,
                    pm_bid.unwrap_or(0.0),
                    expected
                );
                if expected >= MIN_RECOVERY_USD || ks_result == "yes" {
                    // ks_result="yes" 时 PM YES ≈ $1，即使 expected 暂时低估也应卖出
                    close_pm_side(pm_trader, &pair.pm_token_id, pm_shares, "B-KS结算").await;
                }
                continue; // 已处理，不再检查 B-PM 和 C
            }
            Ok(None) => {} // KS 未结算，继续检查 PM 侧
            Err(e) => eprintln!("   ⚠️ 查 KS 结算状态 {} 失败: {}", pair.ks_ticker, e),
        }
        sleep(Duration::from_millis(200)).await;

        // ── Scenario B: Polymarket 已结算 ────────────────────────────────
        // PM "yes"（PM YES 赢，我方 KS NO 输）→ 卖 KS NO 回收残值
        // PM "no"（PM NO 赢，PM YES 输）→ KS NO 将自动结算 $1，无需操作
        if let Some(market_json) = fetch_gamma_raw_for_token(ks_http, &pair.pm_token_id).await {
            if let Some(pm_result) = pm_settlement_result(&market_json) {
                match pm_result.as_str() {
                    "yes" => {
                        // PM YES 赢 → KS NO 应为亏损侧，回收残值
                        let ks_bid = fetch_market_orderbook_json(ks_http, kalshi_cfg, &pair.ks_ticker)
                            .await
                            .ok()
                            .flatten()
                            .as_ref()
                            .and_then(|b| kalshi_best_bid_for_side(b, &pos.side));
                        let expected = ks_bid.unwrap_or(0.0) * pos.contracts;
                        println!(
                            "   📌 [B-PM结算yes] token={}… | KS {} {} {:.2}份 bid={:.3} 预期回收≈${:.3}",
                            &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                            pair.ks_ticker,
                            pos.side,
                            pos.contracts,
                            ks_bid.unwrap_or(0.0),
                            expected
                        );
                        if expected >= MIN_RECOVERY_USD {
                            close_ks_side(
                                ks_http,
                                kalshi_cfg,
                                &pair.ks_ticker,
                                &pos.side,
                                pos.contracts,
                                "B-PM结算yes",
                            )
                            .await;
                        }
                    }
                    "no" => {
                        // PM NO 赢 → PM YES 为亏损侧；KS NO 将自动结算 $1，无需干预
                        // 尝试卖 PM YES 回收微量残值
                        let pm_bid = pm_best_bid(ks_http, &pair.pm_token_id).await;
                        let expected = pm_bid.unwrap_or(0.0) * pm_shares;
                        println!(
                            "   📌 [B-PM结算no] token={}… {:.2}份 bid={:.3} 预期≈${:.3} | KS NO 将自动结算",
                            &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                            pm_shares,
                            pm_bid.unwrap_or(0.0),
                            expected
                        );
                        if expected >= MIN_RECOVERY_USD {
                            close_pm_side(pm_trader, &pair.pm_token_id, pm_shares, "B-PM结算no").await;
                        }
                    }
                    _ => {}
                }
                continue; // 已处理 B
            }
        }
        sleep(Duration::from_millis(200)).await;

        // ── Scenario C: 双侧均开仓，高置信止损 ────────────────────────────
        // 当我方持有侧 bid ≤ C_LOSING_BID_THRESHOLD（0.10），市场 90%+ 置信我方处于亏损侧。

        // C-PM：检查我方 PM token bid
        if let Some(pm_bid) = pm_best_bid(ks_http, &pair.pm_token_id).await {
            if pm_bid <= C_LOSING_BID_THRESHOLD {
                let expected = pm_bid * pm_shares;
                if expected >= MIN_RECOVERY_USD {
                    println!(
                        "   🟠 [C-PM止损] token={}… {} bid={:.3} qty={:.2} 预期≈${:.3} → 平仓",
                        &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                        pair.pm_side,
                        pm_bid,
                        pm_shares,
                        expected
                    );
                    close_pm_side(pm_trader, &pair.pm_token_id, pm_shares, "C-PM止损").await;
                } else {
                    println!(
                        "   🟠 [C-PM止损] token={}… {} bid={:.3} qty={:.2} 预期≈${:.3} → 跳过（回收不足 ${}）",
                        &pair.pm_token_id[..pair.pm_token_id.len().min(16)],
                        pair.pm_side,
                        pm_bid,
                        pm_shares,
                        expected,
                        MIN_RECOVERY_USD
                    );
                }
            }
        }
        sleep(Duration::from_millis(200)).await;

        // C-KS：检查我方 KS 持仓 bid
        match fetch_market_orderbook_json(ks_http, kalshi_cfg, &pair.ks_ticker).await {
            Ok(Some(book)) => {
                if let Some(ks_bid) = kalshi_best_bid_for_side(&book, &pos.side) {
                    if ks_bid <= C_LOSING_BID_THRESHOLD {
                        let expected = ks_bid * pos.contracts;
                        if expected >= MIN_RECOVERY_USD {
                            println!(
                                "   🟠 [C-KS止损] {} {} bid={:.3} qty={:.2} 预期≈${:.3} → 平仓",
                                pair.ks_ticker, pos.side, ks_bid, pos.contracts, expected
                            );
                            close_ks_side(
                                ks_http,
                                kalshi_cfg,
                                &pair.ks_ticker,
                                &pos.side,
                                pos.contracts,
                                "C-KS止损",
                            )
                            .await;
                        } else {
                            println!(
                                "   🟠 [C-KS止损] {} {} bid={:.3} qty={:.2} 预期≈${:.3} → 跳过（回收不足 ${}）",
                                pair.ks_ticker, pos.side, ks_bid, pos.contracts, expected, MIN_RECOVERY_USD
                            );
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("   ⚠️ 拉 KS 订单簿 {} 失败: {}", pair.ks_ticker, e),
        }
        sleep(Duration::from_millis(200)).await;
    }

    println!("🔎 ── 仓位清理完毕 ──\n");
}
