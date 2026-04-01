//! 回望天数 + desk_events OK 统计与曲线图（覆盖写入 out）。

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(name = "portfolio-turnover")]
struct Args {
    #[arg(long, default_value = "logs/desk_events.jsonl")]
    events: PathBuf,
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

fn main() -> Result<()> {
    let args = Args::parse();
    let days = prompt_days()?;
    portfolio_monitor::turnover::run_turnover(&args.events, &args.out, days)
}
