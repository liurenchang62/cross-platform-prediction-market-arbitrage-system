//! 定向平仓：给定 Kalshi ticker，从 desk_events 找对应 PM token，两腿同时平。
//!
//! 用法：
//!   cargo run -p portfolio-monitor --bin close-position -- \
//!       --ks-ticker KXNETFLIXRANKSHOW-26APR20-BIG
//!
//! 默认 dry-run（只打印订单薄和仓位，不实际下单）。
//! 实际执行需加 `--execute`。

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::Client;

use portfolio_monitor::kalshi;
use portfolio_monitor::mapping::load_pairs_from_desk_events;
use portfolio_monitor::pm;

#[derive(Parser)]
#[command(name = "close-position")]
struct Args {
    /// Kalshi 市场 ticker，例如 KXNETFLIXRANKSHOW-26APR20-BIG
    #[arg(long)]
    ks_ticker: String,

    /// desk_events 文件路径
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    desk: PathBuf,

    /// 实际执行平仓（不加此参数为 dry-run，仅显示当前持仓和订单薄）
    #[arg(long)]
    execute: bool,

    /// KS 卖出最低价（分，默认 1 = 接受任意成交价）
    #[arg(long, default_value_t = 1)]
    ks_min_cents: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let ks = kalshi::load_kalshi()?;
    let pm_clob = pm::connect().await?;

    // ── 1. 查 KS 当前持仓 ────────────────────────────────────────────────────
    let pos_json = kalshi::positions_for_ticker(&http, &ks, &args.ks_ticker)
        .await?
        .with_context(|| format!("Kalshi 无此市场持仓: {}", args.ks_ticker))?;

    let fp = pos_json["position_fp"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| pos_json["position_fp"].as_f64())
        .unwrap_or(0.0);
    let ks_side = if fp > 0.0 { "YES" } else { "NO" };
    let ks_contracts = fp.abs().round() as i64;

    if ks_contracts == 0 {
        println!("KS {} 当前无持仓，退出。", args.ks_ticker);
        return Ok(());
    }
    println!(
        "KS 持仓: {} {} {} 份",
        args.ks_ticker, ks_side, ks_contracts
    );

    // KS 订单薄（显示最优 bid）
    if let Ok(book) = kalshi::fetch_orderbook_json_public(&http, &ks.base, &args.ks_ticker).await {
        if let Some(mid) = kalshi::orderbook_mid_for_side(&book, ks_side) {
            println!("  KS {} mid={:.4}", ks_side, mid);
        }
        // 提取 best bid
        let bid_key = if ks_side == "YES" { "yes_dollars" } else { "no_dollars" };
        let ob = book.get("orderbook_fp").or_else(|| book.get("orderbook"));
        if let Some(ob) = ob {
            let best_bid = ob[bid_key]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|e| {
                    let p = e.get(0)?.as_f64().or_else(|| e.get(0)?.as_str()?.parse().ok())?;
                    let s = e.get(1)?.as_f64().unwrap_or(0.0);
                    if s > 0.0 && p > 0.0 { Some(p) } else { None }
                })
                .fold(None::<f64>, |a, p| Some(a.map_or(p, |x: f64| x.max(p))));
            println!(
                "  KS {} best_bid={:.4}  预计回收≈${:.2}",
                ks_side,
                best_bid.unwrap_or(0.0),
                best_bid.unwrap_or(0.0) * ks_contracts as f64
            );
        }
    }

    // ── 2. 从 desk_events 找 PM token ────────────────────────────────────────
    let pairs = load_pairs_from_desk_events(&args.desk, false)?;
    let matched: Vec<_> = pairs
        .iter()
        .filter(|p| p.kalshi_ticker == args.ks_ticker)
        .collect();

    if matched.is_empty() {
        println!("desk_events 无对应 PM 配对，仅平 KS 侧。");
    }

    let mut pm_closes: Vec<(String, String, i64)> = Vec::new(); // (token_id, pm_side, shares)
    for pair in &matched {
        let shares_dec = pm::conditional_shares(&pm_clob, &pair.pm_buy_token_id)
            .await
            .unwrap_or_default();
        let shares_f: f64 = shares_dec.to_string().parse().unwrap_or(0.0);
        let shares_micro = shares_f as i64;
        // PM balance 以 micro 为单位（1 份 = 1_000_000）
        let shares_int = shares_micro / 1_000_000;
        let pm_side = &pair.strategy.pm_buy_leg;

        if shares_int == 0 {
            println!(
                "PM token={} ({}) 余额为0，跳过",
                &pair.pm_buy_token_id[..pair.pm_buy_token_id.len().min(16)],
                pm_side
            );
            continue;
        }
        println!(
            "PM 持仓: token={}… ({}) {} 份",
            &pair.pm_buy_token_id[..pair.pm_buy_token_id.len().min(16)],
            pm_side,
            shares_int
        );

        // PM 订单薄 best bid
        let pm_book: Result<serde_json::Value, _> = async {
            let resp = http
                .get("https://clob.polymarket.com/book")
                .query(&[("token_id", &pair.pm_buy_token_id)])
                .send()
                .await?;
            let t = resp.text().await?;
            serde_json::from_str(&t).map_err(anyhow::Error::from)
        }
        .await;

        if let Ok(book) = pm_book {
            let best_bid = book["bids"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|b| {
                    b["price"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .or_else(|| b["price"].as_f64())
                })
                .fold(None::<f64>, |a, p| Some(a.map_or(p, |x: f64| x.max(p))));
            println!(
                "  PM {} best_bid={:.4}  预计回收≈${:.2}",
                pm_side,
                best_bid.unwrap_or(0.0),
                best_bid.unwrap_or(0.0) * shares_int as f64
            );
            if best_bid.unwrap_or(0.0) < 0.001 {
                println!("  ⚠️ PM bid≈0，市场可能已结算或无流动性，不执行 PM 平仓");
                continue;
            }
        }

        pm_closes.push((pair.pm_buy_token_id.clone(), pm_side.clone(), shares_int));
    }

    // ── 3. 执行 ──────────────────────────────────────────────────────────────
    if !args.execute {
        println!("\n[dry-run] 加 --execute 后将实际执行以下操作：");
        println!("  KS SELL {} {} {} 份 @ min {}¢ IOC", args.ks_ticker, ks_side, ks_contracts, args.ks_min_cents);
        for (token_id, pm_side, shares) in &pm_closes {
            println!(
                "  PM SELL {} {} {} 份 FAK",
                &token_id[..token_id.len().min(16)],
                pm_side,
                shares
            );
        }
        return Ok(());
    }

    println!("\n▶ 执行平仓...");

    // KS 平仓
    match kalshi::sell_ioc(&http, &ks, &args.ks_ticker, ks_side, ks_contracts, args.ks_min_cents).await {
        Ok(v) => {
            let order = &v["order"];
            let fill = order["fill_count_fp"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| order["fill_count"].as_f64())
                .unwrap_or(0.0);
            println!(
                "  ✅ KS IOC order_id={} fill={}/{} ks_side={}",
                order["order_id"].as_str().unwrap_or("?"),
                fill, ks_contracts, ks_side
            );
        }
        Err(e) => eprintln!("  ❌ KS 卖出失败: {}", e),
    }

    // PM 平仓
    for (token_id, pm_side, shares) in &pm_closes {
        match pm::sell_token_fak(&pm_clob, token_id, *shares).await {
            Ok(r) => println!(
                "  ✅ PM FAK success={} making={} taking={} side={}",
                r.success,
                r.making_amount,
                r.taking_amount,
                pm_side
            ),
            Err(e) => eprintln!("  ❌ PM 卖出失败 ({}): {}", pm_side, e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    println!("\n平仓完成。");
    Ok(())
}
