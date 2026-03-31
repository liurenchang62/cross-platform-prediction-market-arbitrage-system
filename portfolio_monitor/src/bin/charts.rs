//! `cargo run -p portfolio-monitor --bin portfolio-charts`

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "portfolio-charts")]
struct Args {
    #[arg(long, default_value = "portfolio_monitor/config/arb_pairs.toml")]
    config: PathBuf,
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    events: PathBuf,
    #[arg(long, default_value = "portfolio_monitor/out")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    portfolio_monitor::tasks::run_charts(&args.config, &args.events, &args.out).await
}
