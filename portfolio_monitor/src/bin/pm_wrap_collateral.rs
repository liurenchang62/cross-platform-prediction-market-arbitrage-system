//! 将 Proxy 钱包中全部 USDC.e 通过 Polymarket CollateralOnramp 换成 pUSD。
//!
//! Polymarket V2（2026-04-28）已将交易抵押品从 USDC.e 迁移至 pUSD，运行前需执行此操作。
//!
//! 环境变量要求（与主程序相同）：
//!   POLYMARKET_SIGNATURE_TYPE=1   （必须为 Proxy 钱包）
//!   POLYMARKET_PRIVATE_KEY_PATH   或 POLYMARKET_PRIVATE_KEY
//!   RELAYER_API_KEY               （或 POLYMARKET_API_KEY）
//!   RELAYER_API_KEY_ADDRESS       （或 POLYMARKET_ADDRESS）
//!
//! 用法：
//!   cargo run -p portfolio-monitor --bin pm-wrap-collateral --release
//!   cargo run -p portfolio-monitor --bin pm-wrap-collateral --release -- --rpc-url https://polygon-bor-rpc.publicnode.com

use anyhow::Result;
use clap::Parser;
use portfolio_monitor::pm_relayer_automation;

#[derive(Parser)]
#[command(name = "pm-wrap-collateral", about = "将 Proxy 钱包 USDC.e → pUSD（Polymarket V2 迁移）")]
struct Args {
    #[arg(long, default_value = "https://polygon-bor-rpc.publicnode.com", env = "POLYGON_RPC_URL")]
    rpc_url: String,

    #[arg(long, default_value = "https://relayer-v2.polymarket.com", env = "POLYMARKET_RELAYER_URL")]
    relayer_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    println!("🔄 PM CollateralOnramp：USDC.e → pUSD（Polymarket V2 抵押品迁移）");
    println!("   RPC  : {}", args.rpc_url);
    println!("   Relayer: {}", args.relayer_url);

    pm_relayer_automation::wrap_usdce_to_pusd(&args.rpc_url, &args.relayer_url).await?;

    Ok(())
}
