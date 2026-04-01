//! 回望天数 + 双平台现金流 CSV（覆盖写入 out/cashflow.csv）。

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(name = "portfolio-cashflow")]
struct Args {
    #[arg(long, default_value = "portfolio_monitor/config/arb_pairs.toml")]
    config: PathBuf,
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    desk: PathBuf,
    #[arg(long, default_value = "portfolio_monitor/out")]
    out: PathBuf,
}

fn prompt_days() -> Result<u32> {
    print!("回望历史天数: ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().read_line(&mut s).context("stdin")?;
    s.trim()
        .parse::<u32>()
        .context("须为无符号整数（天数）")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let days = prompt_days()?;
    portfolio_monitor::cashflow::run_cashflow(&args.config, &args.desk, &args.out, days).await
}
