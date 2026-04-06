//! 全历史利润相关字段汇总（终端）。

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "portfolio-profit")]
struct Args {
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    desk: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    portfolio_monitor::profit::run_profit(&args.desk).await
}
