//! 独立执行：遍历 Data API 持仓，经 Relayer 对 **当前所有可赎回** 的 PM 条件头寸逐条提交 Proxy `redeem`，并刷新 CLOB Collateral。
//!
//! 环境与主程序相同：`POLYMARKET_SIGNATURE_TYPE=1`、Proxy 私钥、`RELAYER_API_KEY`（或 `POLYMARKET_API_KEY`）及地址变量等。
//!
//! ```text
//! cargo run -p trader --bin pm_redeem --release
//!
//! # 若因 allowance 不足失败，可加一次批量授权：
//! cargo run -p trader --bin pm_redeem --release -- --approve-batch
//! ```

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use reqwest::Client;

#[derive(Parser)]
#[command(name = "pm_redeem")]
struct Args {
    #[arg(long, env = "POLYGON_RPC_URL")]
    rpc_url: Option<String>,
    #[arg(long, env = "POLYMARKET_RELAYER_URL")]
    relayer_url: Option<String>,
    /// 提交前先做 Relayer USDC/CTF 批量授权
    #[arg(long)]
    approve_batch: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let http = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;
    let rpc = args.rpc_url.unwrap_or_else(
        portfolio_monitor::pm_relayer_automation::polygon_rpc_for_automation,
    );
    let relay = args
        .relayer_url
        .unwrap_or_else(portfolio_monitor::pm_relayer_automation::relayer_url_for_automation);

    println!("PM 独立赎回 · RPC {} · Relayer {}", rpc, relay);
    if args.approve_batch {
        println!("（将先执行 Relayer 批量授权）");
    }

    let rep = portfolio_monitor::pm_relayer_automation::desk_hydrate_pm_proxy(
        &http,
        &rpc,
        &relay,
        args.approve_batch,
    )
    .await?;

    if let Some(note) = &rep.note {
        println!("{}", note);
    }
    println!(
        "赎回提交成功 {} 笔 · 失败 {} · 跳过（未就绪或无仓）{} · CLOB 刷新 {}",
        rep.redeem_ok,
        rep.redeem_fail,
        rep.redeem_skipped,
        if rep.clob_refresh_ok { "ok" } else { "否" }
    );
    Ok(())
}
