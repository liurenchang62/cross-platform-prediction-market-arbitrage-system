//! 当前持仓订单薄查看：显示每个开仓的 Kalshi 和 Polymarket 双边 bid/ask 订单薄（各取前5档）。
//!
//! 用法：`portfolio-position-markets [--desk logs/desk_events.jsonl]`

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use reqwest::Client;
use serde_json::Value;

use portfolio_monitor::kalshi;
use portfolio_monitor::mapping::{load_pairs_from_desk_events, ArbPair};

#[derive(Parser)]
#[command(name = "portfolio-position-markets")]
struct Args {
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    desk: PathBuf,
    /// 每侧最多显示档位数
    #[arg(long, default_value_t = 5)]
    depth: usize,
}

// ─── KS 订单薄解析 ───────────────────────────────────────────────────────────

struct Level {
    price: f64,
    size: f64,
}

/// 从 KS orderbook JSON 提取指定侧的 bids 和 asks（均以"持有该侧"视角）。
///
/// KS 的 `yes_dollars` = YES 侧的 bid ladder（买盘）；
/// `no_dollars` = NO 侧的 bid ladder。
/// 对面侧的挂单 → ask = 1.0 - 对面_bid_price。
fn ks_ladder(book: &Value, side: &str) -> (Vec<Level>, Vec<Level>) {
    let orderbook = match book.get("orderbook_fp").or_else(|| book.get("orderbook")) {
        Some(v) => v,
        None => return (vec![], vec![]),
    };
    let (bid_key, ask_src_key) = match side {
        "YES" => ("yes_dollars", "no_dollars"),
        "NO" => ("no_dollars", "yes_dollars"),
        _ => return (vec![], vec![]),
    };

    let parse_num = |v: Option<&Value>| -> Option<f64> {
        v.and_then(|x| x.as_f64().or_else(|| x.as_str()?.parse().ok()))
    };

    let mut bids: Vec<Level> = orderbook
        .get(bid_key)
        .and_then(|a| a.as_array())
        .map(|arr| {
            let mut v: Vec<Level> = arr
                .iter()
                .filter_map(|e| {
                    let p = parse_num(e.get(0))?;
                    let s = parse_num(e.get(1))?;
                    if p > 0.0 && p < 1.0 && s > 0.0 {
                        Some(Level { price: p, size: s })
                    } else {
                        None
                    }
                })
                .collect();
            v.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap());
            v
        })
        .unwrap_or_default();

    // ask 来自对面侧 bid，ask_price = 1 - opposite_bid_price
    let mut asks: Vec<Level> = orderbook
        .get(ask_src_key)
        .and_then(|a| a.as_array())
        .map(|arr| {
            let mut v: Vec<Level> = arr
                .iter()
                .filter_map(|e| {
                    let p = parse_num(e.get(0))?;
                    let s = parse_num(e.get(1))?;
                    let ask_p = 1.0 - p;
                    if ask_p > 0.0 && ask_p < 1.0 && s > 0.0 {
                        Some(Level { price: ask_p, size: s })
                    } else {
                        None
                    }
                })
                .collect();
            v.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());
            v
        })
        .unwrap_or_default();

    bids.truncate(999);
    asks.truncate(999);
    (bids, asks)
}

// ─── PM CLOB 订单薄获取 ───────────────────────────────────────────────────────

async fn pm_clob_book(http: &Client, token_id: &str) -> Result<Value> {
    let resp = http
        .get("https://clob.polymarket.com/book")
        .query(&[("token_id", token_id)])
        .send()
        .await?;
    let text = resp.text().await?;
    serde_json::from_str(&text).map_err(Into::into)
}

fn pm_ladder(book: &Value) -> (Vec<Level>, Vec<Level>) {
    let parse = |v: Option<&Value>| -> Option<f64> {
        v.and_then(|x| x.as_f64().or_else(|| x.as_str()?.parse().ok()))
    };

    let parse_side = |key: &str| -> Vec<Level> {
        book.get(key)
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let p = parse(e.get("price"))?;
                        let s = parse(e.get("size"))?;
                        if p > 0.0 && s > 0.0 {
                            Some(Level { price: p, size: s })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut bids = parse_side("bids");
    bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap());
    let mut asks = parse_side("asks");
    asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());
    (bids, asks)
}

// ─── 打印辅助 ─────────────────────────────────────────────────────────────────

fn print_ladder(label: &str, bids: &[Level], asks: &[Level], depth: usize) {
    let best_bid = bids.first().map(|l| l.price);
    let best_ask = asks.first().map(|l| l.price);
    let spread = match (best_bid, best_ask) {
        (Some(b), Some(a)) => format!("{:.4}", a - b),
        _ => "—".to_string(),
    };
    let mid = match (best_bid, best_ask) {
        (Some(b), Some(a)) => format!("{:.4}", 0.5 * (b + a)),
        (Some(b), None) => format!("{:.4} (bid only)", b),
        (None, Some(a)) => format!("{:.4} (ask only)", a),
        _ => "—".to_string(),
    };
    println!("  {} | spread={} mid={}", label, spread, mid);

    let bid_strs: Vec<String> = bids
        .iter()
        .take(depth)
        .map(|l| format!("{:.4}×{:.0}", l.price, l.size))
        .collect();
    let ask_strs: Vec<String> = asks
        .iter()
        .take(depth)
        .map(|l| format!("{:.4}×{:.0}", l.price, l.size))
        .collect();

    if bid_strs.is_empty() {
        println!("    Bids: (空)");
    } else {
        println!("    Bids: {}", bid_strs.join("  "));
    }
    if ask_strs.is_empty() {
        println!("    Asks: (空)");
    } else {
        println!("    Asks: {}", ask_strs.join("  "));
    }
}

// ─── 主程序 ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let depth = args.depth;

    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let ks = kalshi::load_kalshi()?;

    // 1. 拉取 KS 开仓持仓
    let sign_p = "/trade-api/v2/portfolio/positions";
    let url = format!("{}/portfolio/positions?limit=1000", ks.base);
    let body = kalshi::signed_get(&http, &ks, &url, sign_p).await?;
    let v: Value = serde_json::from_str(&body)?;
    let arr = v["market_positions"].as_array().cloned().unwrap_or_default();

    let mut ks_positions: Vec<(String, String, f64)> = Vec::new(); // (ticker, side, contracts)
    for p in &arr {
        let fp = p["position_fp"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| p["position_fp"].as_f64())
            .unwrap_or(0.0);
        if fp.abs() < 0.5 {
            continue;
        }
        let ticker = p["ticker"].as_str().unwrap_or("").to_string();
        if ticker.is_empty() {
            continue;
        }
        let side = if fp > 0.0 { "YES" } else { "NO" };
        ks_positions.push((ticker.clone(), side.to_string(), fp.abs()));
    }

    if ks_positions.is_empty() {
        println!("Kalshi 当前无开仓。");
        return Ok(());
    }

    // 2. 加载 desk_events 配对（用于查 PM token_id）
    let pairs: Vec<ArbPair> = load_pairs_from_desk_events(&args.desk, false)?;
    // ticker → pairs
    let mut ticker_to_pairs: HashMap<String, Vec<&ArbPair>> = HashMap::new();
    for p in &pairs {
        ticker_to_pairs
            .entry(p.kalshi_ticker.clone())
            .or_default()
            .push(p);
    }

    println!("\n══════════════════════════════════════════════════════════════");
    println!("  当前持仓订单薄（depth={}）", depth);
    println!("══════════════════════════════════════════════════════════════");

    for (ticker, ks_side, contracts) in &ks_positions {
        println!("\n── {} ──", ticker);
        println!("  KS  {} {:.0}份", ks_side, contracts);

        // KS orderbook
        match kalshi::fetch_orderbook_json_public(&http, &ks.base, ticker).await {
            Ok(book) => {
                let (bids, asks) = ks_ladder(&book, ks_side);
                print_ladder(&format!("KS {}", ks_side), &bids, &asks, depth);
            }
            Err(e) => println!("  ⚠️ KS 订单薄获取失败: {}", e),
        }

        // PM 侧
        let matched_pairs = ticker_to_pairs.get(ticker.as_str());
        match matched_pairs {
            None => {
                println!("  PM: 无 desk_events 配对记录（孤仓）");
            }
            Some(ps) => {
                for pair in ps.iter().take(2) {
                    // 通常只有1个，取最多2个防重复
                    let pm_side = &pair.strategy.pm_buy_leg;
                    let token_short = &pair.pm_buy_token_id
                        [..pair.pm_buy_token_id.len().min(16)];
                    println!(
                        "  PM  {} token={}…",
                        pm_side, token_short
                    );
                    if let Some(title) = &pair.pm_title {
                        println!("      ({})", title);
                    }
                    match pm_clob_book(&http, &pair.pm_buy_token_id).await {
                        Ok(book) => {
                            let (bids, asks) = pm_ladder(&book);
                            print_ladder(&format!("PM {}", pm_side), &bids, &asks, depth);
                        }
                        Err(e) => println!("  ⚠️ PM 订单薄获取失败: {}", e),
                    }
                }
            }
        }
    }

    println!("\n══════════════════════════════════════════════════════════════\n");
    Ok(())
}
