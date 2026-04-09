//! 套利三时点回报对比工具（识别时 → 执行后 → 结算后）。
//!
//! 用法：
//!   arb_reconcile                        # 只输出执行数据（不查 API）
//!   arb_reconcile --settled              # 同时从 Kalshi + PM API 拉取结算数据
//!   arb_reconcile --desk path/to/events.jsonl --settled

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "arb-reconcile", about = "套利三时点回报对比：识别时 → 执行后 → 结算后")]
struct Args {
    /// desk_events.jsonl 路径
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    desk: PathBuf,

    /// 同时从 Kalshi + PM API 查询结算后真实收益（需配置相应环境变量）
    #[arg(long, default_value_t = false)]
    settled: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    portfolio_monitor::arb_reconcile::run_reconcile(&args.desk, args.settled).await
}
