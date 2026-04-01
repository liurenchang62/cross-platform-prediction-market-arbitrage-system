//! 统一快照：Kalshi / PM 同构账户块 + 对冲对照表。

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "portfolio-snapshot")]
struct Args {
    #[arg(long, default_value = "portfolio_monitor/config/arb_pairs.toml")]
    config: PathBuf,
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    desk: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    portfolio_monitor::snapshot_report::run_snapshot(&args.config, &args.desk).await
}
