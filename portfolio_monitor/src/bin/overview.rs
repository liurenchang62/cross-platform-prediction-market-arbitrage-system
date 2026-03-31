//! `cargo run -p portfolio-monitor --bin portfolio-overview`
//!
//! 默认映射：`portfolio_monitor/config/arb_pairs.toml`

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "portfolio-overview")]
struct Args {
    #[arg(long, default_value = "portfolio_monitor/config/arb_pairs.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    portfolio_monitor::tasks::run_overview(&args.config).await
}
