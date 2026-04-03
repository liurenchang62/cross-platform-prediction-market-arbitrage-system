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
    /// 回望历史天数（不指定时交互提示）
    #[arg(long)]
    days: Option<u32>,
}

const DEFAULT_DAYS: u32 = 30;

fn prompt_days() -> Result<u32> {
    print!("回望历史天数 [默认 {}]: ", DEFAULT_DAYS);
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().read_line(&mut s).context("stdin")?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        eprintln!("（未检测到终端输入，使用默认 {} 天）", DEFAULT_DAYS);
        return Ok(DEFAULT_DAYS);
    }
    trimmed.parse::<u32>().context("须为无符号整数（天数）")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let days = match args.days {
        Some(d) => d,
        None => prompt_days()?,
    };
    portfolio_monitor::cashflow::run_cashflow(&args.config, &args.desk, &args.out, days).await
}
