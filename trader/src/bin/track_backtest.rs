//! 读取 `logs/tracking_arbitrage_events.csv`，按「开始日期 UTC」与会话维度做汇总（不做模拟交易）。  
//! 用法：`cargo run --bin track_backtest -- YYYY MM DD <会话数> <每会话周期数> [arb_csv路径]`（源文件在 `desk/bin/`）
//!
//! 例：`cargo run --bin track_backtest -- 2026 3 28 3 50`

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use csv::ReaderBuilder;

#[path = "../system_params.rs"]
mod system_params;

#[derive(Debug, Clone)]
struct ArbRow {
    time: DateTime<Utc>,
    cycle_id: usize,
    cycle_phase: String,
    pair_id: String,
    pm_title: String,
    kalshi_title: String,
    net_profit: f64,
    pm_book_json: String,
    ks_book_json: String,
    total_capital_usd: f64,
    per_leg_cap_usd: f64,
    pair_cap_usd: f64,
    per_leg_cap_frac: f64,
    pair_cap_frac: f64,
    resolution_horizon_days: i64,
    polymarket_markets_n: usize,
    kalshi_markets_n: usize,
    similarity_threshold: f64,
    full_fetch_interval: usize,
    arb_track_concurrency: usize,
    min_net_profit_frac: f64,
}

fn col(idx: &HashMap<String, usize>, k: &str) -> Result<usize> {
    idx.get(k)
        .copied()
        .with_context(|| format!("CSV 缺少列 {k}"))
}

fn parse_row(rec: &csv::StringRecord, idx: &HashMap<String, usize>) -> Result<ArbRow> {
    let ti = col(idx, "time_utc_rfc3339")?;
    let ts = rec.get(ti).unwrap_or("");
    let time = DateTime::parse_from_rfc3339(ts)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| ts.parse::<DateTime<Utc>>().map_err(|e| anyhow::anyhow!(e)))
        .context("解析 time_utc_rfc3339")?;

    let g = |k: &str| -> Result<String> {
        Ok(rec.get(col(idx, k)?).unwrap_or("").to_string())
    };
    let pfn = |k: &str| -> Result<f64> {
        Ok(rec.get(col(idx, k)?).unwrap_or("0").parse().unwrap_or(0.0))
    };
    let pzu = |k: &str| -> Result<usize> {
        Ok(rec.get(col(idx, k)?).unwrap_or("0").parse().unwrap_or(0))
    };
    let pzi = |k: &str| -> Result<i64> {
        Ok(rec.get(col(idx, k)?).unwrap_or("0").parse().unwrap_or(0))
    };

    Ok(ArbRow {
        time,
        cycle_id: pzu("cycle_id")?,
        cycle_phase: g("cycle_phase")?,
        pair_id: g("pair_id")?,
        pm_title: g("pm_title")?,
        kalshi_title: g("kalshi_title")?,
        net_profit: pfn("net_profit_100")?,
        pm_book_json: g("pm_book_top5_json")?,
        ks_book_json: g("ks_book_top5_json")?,
        total_capital_usd: pfn("total_capital_usd")?,
        per_leg_cap_usd: pfn("per_leg_cap_usd")?,
        pair_cap_usd: pfn("pair_cap_usd")?,
        per_leg_cap_frac: pfn("per_leg_cap_frac")?,
        pair_cap_frac: pfn("pair_cap_frac")?,
        resolution_horizon_days: pzi("resolution_horizon_days")?,
        polymarket_markets_n: pzu("polymarket_markets_n")?,
        kalshi_markets_n: pzu("kalshi_markets_n")?,
        similarity_threshold: pfn("similarity_threshold")?,
        full_fetch_interval: pzu("full_fetch_interval_cycles")?,
        arb_track_concurrency: pzu("arb_track_concurrency")?,
        min_net_profit_frac: pfn("min_net_profit_frac_of_pair_cap")?,
    })
}

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let y: i32 = a
        .next()
        .context("缺少年")?
        .parse()
        .context("year")?;
    let mo: u32 = a.next().context("缺少月")?.parse().context("month")?;
    let d: u32 = a.next().context("缺少日")?.parse().context("day")?;
    let sessions: usize = a.next().context("缺少会话数")?.parse().context("sessions")?;
    let cycles_per_session: usize = a
        .next()
        .context("缺少每会话周期数")?
        .parse()
        .context("cycles_per_session")?;

    let csv_path = a.next().unwrap_or_else(|| system_params::TRACKING_ARB_EVENTS_CSV.to_string());
    if !Path::new(&csv_path).exists() {
        bail!("找不到套利事件 CSV: {}", csv_path);
    }

    let start = NaiveDate::from_ymd_opt(y, mo, d)
        .context("无效日期")?
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let start_utc = DateTime::<Utc>::from_naive_utc_and_offset(start, Utc);

    let mut rdr = ReaderBuilder::new()
        .has_headers(true)
        .from_path(&csv_path)
        .with_context(|| format!("打开 {}", csv_path))?;
    let headers = rdr.headers()?.clone();
    let mut idx = HashMap::new();
    for (i, h) in headers.iter().enumerate() {
        idx.insert(h.to_string(), i);
    }

    let mut rows: Vec<ArbRow> = Vec::new();
    for rec in rdr.records() {
        let rec = rec?;
        let row = parse_row(&rec, &idx)?;
        if row.time >= start_utc {
            rows.push(row);
        }
    }

    rows.sort_by(|a, b| a.time.cmp(&b.time));

    let mut cycle_order: Vec<usize> = rows.iter().map(|r| r.cycle_id).collect();
    cycle_order.sort_unstable();
    cycle_order.dedup();

    let need_cycles = sessions.saturating_mul(cycles_per_session);
    if cycle_order.len() < need_cycles {
        eprintln!(
            "⚠ 过滤后仅 {} 个不同 cycle_id，不足 {} 会话 × {} 周期 = {}；按现有周期截断会话。",
            cycle_order.len(),
            sessions,
            cycles_per_session,
            need_cycles
        );
    }
    let cycle_order: Vec<usize> = cycle_order.into_iter().take(need_cycles).collect();

    println!(
        "══ 追踪套利事件回测汇总（UTC）══\n开始日期 ≥ {:04}-{:02}-{:02}  ·  目标 {} 会话 × 每会话 {} 周期  ·  数据文件 {}\n\
说明：周期序列取自套利日志中出现过的 cycle_id 升序；未触发套利的周期不会出现在此序列中。\n",
        y, mo, d, sessions, cycles_per_session, csv_path
    );

    for s in 0..sessions {
        let lo = s * cycles_per_session;
        let hi = ((s + 1) * cycles_per_session).min(cycle_order.len());
        if lo >= cycle_order.len() {
            println!("── 会话 #{}：无周期数据（已用尽 cycle 列表）──", s + 1);
            continue;
        }
        let slice = &cycle_order[lo..hi];
        let cset: HashSet<usize> = slice.iter().copied().collect();
        let sess_rows: Vec<&ArbRow> = rows.iter().filter(|r| cset.contains(&r.cycle_id)).collect();

        let mut cycles_with_arb: HashSet<usize> = HashSet::new();
        for r in &sess_rows {
            cycles_with_arb.insert(r.cycle_id);
        }
        let total_cycles_in_sess = cset.len();
        let ratio = if total_cycles_in_sess > 0 {
            cycles_with_arb.len() as f64 / total_cycles_in_sess as f64
        } else {
            0.0
        };

        let t0 = sess_rows
            .iter()
            .map(|r| r.time)
            .min()
            .unwrap_or(start_utc);

        let mut best_by_pair: HashMap<&str, &ArbRow> = HashMap::new();
        for r in &sess_rows {
            let e = best_by_pair.entry(r.pair_id.as_str()).or_insert(r);
            if r.net_profit > e.net_profit {
                *e = r;
            }
        }
        let mut deduped: Vec<&ArbRow> = best_by_pair.into_values().collect();
        deduped.sort_by(|a, b| {
            b.net_profit
                .partial_cmp(&a.net_profit)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!(
            "会话 #{}  ·  周期集合 {:?}（共 {} 个周期）",
            s + 1,
            slice,
            slice.len()
        );
        println!("  开始时间(UTC): {}", t0.to_rfc3339());
        println!("  套利事件条数(含重复检出): {}", sess_rows.len());
        println!("  去重后套利机会数(pair_id): {}", deduped.len());
        println!(
            "  有套利机会的周期数 / 本会话总周期数: {} / {}  ({:.1}%)",
            cycles_with_arb.len(),
            total_cycles_in_sess,
            ratio * 100.0
        );

        if let Some(sample) = sess_rows.first() {
            println!(
                "  本会话参数快照(取首条事件列): total_capital=${:.2} per_leg=${:.2} pair_cap=${:.2} | frac per_leg={:.4} pair={:.4} | horizon_days={} | pm_n={} ks_n={} | sim_th={:.3} full_fetch_cycles={} arb_conc={} min_net_profit_frac={:.4}",
                sample.total_capital_usd,
                sample.per_leg_cap_usd,
                sample.pair_cap_usd,
                sample.per_leg_cap_frac,
                sample.pair_cap_frac,
                sample.resolution_horizon_days,
                sample.polymarket_markets_n,
                sample.kalshi_markets_n,
                sample.similarity_threshold,
                sample.full_fetch_interval,
                sample.arb_track_concurrency,
                sample.min_net_profit_frac
            );
        }

        println!("  ── 去重后 Top10（按模型 net_profit_100）──");
        if deduped.is_empty() {
            println!("    (无)");
        } else {
            for (i, r) in deduped.iter().take(10).enumerate() {
                println!(
                    "    #{}  net=${:.4}  cycle={} {}  {}",
                    i + 1,
                    r.net_profit,
                    r.cycle_id,
                    r.cycle_phase,
                    r.pair_id
                );
                println!("        PM:     {}", r.pm_title);
                println!("        Kalshi: {}", r.kalshi_title);
                println!("        pm_book_top5: {}", r.pm_book_json);
                println!("        ks_book_top5: {}", r.ks_book_json);
            }
        }
        println!();
    }

    Ok(())
}
