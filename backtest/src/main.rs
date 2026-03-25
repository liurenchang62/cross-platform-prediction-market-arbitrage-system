//! 读取模拟交易 CSV，按 **UTC 日历日** 选择「会话启动日」，列出当日**开始**的会话，并对选定会话做绩效汇总。
//!
//! 盈亏：总利润 = 已平仓实现盈亏之和 + 未平仓按持有到期模型的锁定估计；与权益减初始在账上一致。

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use dialoguer::{theme::ColorfulTheme, Select};
use std::collections::{BTreeSet, HashMap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use std::env;
use std::fs::File;
use std::path::Path;

const DEFAULT_CSV: &str = "logs/paper_trades.csv";
const ENV_CSV: &str = "PAPER_TRADES_CSV";

const FEE_PM: f64 = 0.01;
const FEE_KS: f64 = 0.01;

fn settlement_fee_estimate(n: f64) -> f64 {
    if n <= 0.0 || !n.is_finite() {
        return 0.0;
    }
    n * (FEE_PM + FEE_KS)
}

fn u_hold_notional(n: f64) -> f64 {
    n - settlement_fee_estimate(n)
}

fn open_total_outlay(entry_capital: f64, fees_open: f64, gas_open: f64) -> f64 {
    entry_capital + fees_open + gas_open
}

fn locked_pnl_at_open(n: f64, entry_capital: f64, fees_open: f64, gas_open: f64) -> f64 {
    u_hold_notional(n) - open_total_outlay(entry_capital, fees_open, gas_open)
}

#[derive(Debug, Clone)]
struct Row {
    line: usize,
    event: String,
    session_id: u64,
    pair_label: String,
    cycle: usize,
    simulated_open_time_utc: String,
    check_time_utc: String,
    n: f64,
    entry_capital: f64,
    fees_open: f64,
    gas_open: f64,
    pnl_realized: f64,
    cash_after: f64,
    notes: String,
}

fn parse_f64(s: &str) -> f64 {
    s.trim().parse().unwrap_or(0.0)
}

fn parse_u64(s: &str) -> u64 {
    s.trim().parse().unwrap_or(0)
}

fn parse_usize(s: &str) -> usize {
    s.trim().parse().unwrap_or(0)
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn effective_time_utc(r: &Row) -> Option<DateTime<Utc>> {
    match r.event.as_str() {
        "SESSION_START" | "SESSION_END" => {
            parse_dt(&r.check_time_utc).or_else(|| parse_dt(&r.simulated_open_time_utc))
        }
        "OPEN" => parse_dt(&r.simulated_open_time_utc),
        "NO_CLOSE" | "CLOSE" => parse_dt(&r.check_time_utc).or_else(|| parse_dt(&r.simulated_open_time_utc)),
        _ => parse_dt(&r.check_time_utc).or_else(|| parse_dt(&r.simulated_open_time_utc)),
    }
}

/// 会话启动时刻：优先会话开始行，否则取该会话最早可解析时间。
fn session_start_utc(rows: &[Row], session_id: u64) -> Option<DateTime<Utc>> {
    let from_marker = rows
        .iter()
        .find(|r| r.session_id == session_id && r.event == "SESSION_START")
        .and_then(effective_time_utc);
    if from_marker.is_some() {
        return from_marker;
    }
    rows.iter()
        .filter(|r| r.session_id == session_id)
        .filter_map(effective_time_utc)
        .min()
}

fn session_start_calendar_date(rows: &[Row], session_id: u64) -> Option<NaiveDate> {
    session_start_utc(rows, session_id).map(|t| t.date_naive())
}

fn load_csv(path: &Path) -> Result<Vec<Row>> {
    let f = File::open(path).with_context(|| format!("打开 {}", path.display()))?;
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(f);
    let headers = rdr.headers()?.clone();
    let idx: HashMap<String, usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| (h.to_string(), i))
        .collect();

    fn get(record: &csv::StringRecord, idx: &HashMap<String, usize>, name: &str) -> String {
        idx.get(name)
            .and_then(|&i| record.get(i))
            .unwrap_or("")
            .to_string()
    }

    let mut out = Vec::new();
    for (line, result) in rdr.records().enumerate() {
        let record = result.context("解析 CSV 行")?;
        let event = get(&record, &idx, "event");
        if event.is_empty() {
            continue;
        }
        out.push(Row {
            line: line + 2,
            event,
            session_id: parse_u64(&get(&record, &idx, "session_id")),
            pair_label: get(&record, &idx, "pair_label"),
            cycle: parse_usize(&get(&record, &idx, "cycle")),
            simulated_open_time_utc: get(&record, &idx, "simulated_open_time_utc"),
            check_time_utc: get(&record, &idx, "check_time_utc"),
            n: parse_f64(&get(&record, &idx, "n")),
            entry_capital: parse_f64(&get(&record, &idx, "entry_capital")),
            fees_open: parse_f64(&get(&record, &idx, "fees_open")),
            gas_open: parse_f64(&get(&record, &idx, "gas_open")),
            pnl_realized: parse_f64(&get(&record, &idx, "pnl_realized")),
            cash_after: parse_f64(&get(&record, &idx, "cash_after")),
            notes: get(&record, &idx, "notes"),
        });
    }
    Ok(out)
}

/// 可选日期：至少有一个会话在该日**启动**（按启动时刻 UTC 日历日）。
fn collect_session_anchor_dates(rows: &[Row]) -> BTreeSet<NaiveDate> {
    let ids: BTreeSet<u64> = rows.iter().map(|r| r.session_id).collect();
    ids.into_iter()
        .filter_map(|sid| session_start_calendar_date(rows, sid))
        .collect()
}

fn sessions_started_on_date(rows: &[Row], d: NaiveDate) -> Vec<u64> {
    let mut v: Vec<u64> = rows
        .iter()
        .map(|r| r.session_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|sid| session_start_calendar_date(rows, *sid) == Some(d))
        .collect();
    v.sort_unstable();
    v
}

#[derive(Debug, Clone)]
struct OpenPosition {
    n: f64,
    entry_capital: f64,
    fees_open: f64,
    gas_open: f64,
}

#[derive(Debug, Clone)]
struct SessionSpanInfo {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    cycle_min: Option<usize>,
    cycle_max: Option<usize>,
}

fn compute_session_span(session_rows: &[&Row]) -> SessionSpanInfo {
    let start = session_rows
        .iter()
        .find(|r| r.event == "SESSION_START")
        .and_then(|r| effective_time_utc(r))
        .or_else(|| {
            session_rows
                .iter()
                .filter_map(|r| effective_time_utc(r))
                .min()
        });

    let end_from_marker = session_rows
        .iter()
        .find(|r| r.event == "SESSION_END")
        .and_then(|r| effective_time_utc(r));
    let end_last_row = session_rows.iter().filter_map(|r| effective_time_utc(r)).max();
    let end = end_from_marker.or(end_last_row);

    let mut cmin: Option<usize> = None;
    let mut cmax: Option<usize> = None;
    for r in session_rows.iter().copied() {
        cmin = Some(cmin.map_or(r.cycle, |m| m.min(r.cycle)));
        cmax = Some(cmax.map_or(r.cycle, |m| m.max(r.cycle)));
    }

    SessionSpanInfo {
        start,
        end,
        cycle_min: cmin,
        cycle_max: cmax,
    }
}

fn fmt_dt_hm(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn fmt_wall_span(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let d = end.signed_duration_since(start);
    if d < Duration::zero() {
        return "—".to_string();
    }
    let secs = d.num_seconds().max(0) as u64;
    let days = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{} 天 {} 小时 {} 分", days, h, m)
    } else if h > 0 {
        format!("{} 小时 {} 分", h, m)
    } else if m > 0 {
        format!("{} 分", m)
    } else {
        format!("{} 秒", s.max(1))
    }
}

fn analyze_session(rows: &[Row], session_id: u64) {
    let mut session_rows: Vec<&Row> = rows.iter().filter(|r| r.session_id == session_id).collect();
    session_rows.sort_by(|a, b| {
        let ta = effective_time_utc(a);
        let tb = effective_time_utc(b);
        match (ta, tb) {
            (Some(aa), Some(bb)) => aa
                .cmp(&bb)
                .then_with(|| a.cycle.cmp(&b.cycle))
                .then_with(|| a.line.cmp(&b.line)),
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, None) => a.line.cmp(&b.line),
        }
    });

    let span = compute_session_span(&session_rows);

    let mut close_count = 0usize;
    let mut open_event_count = 0usize;
    let mut close_realized_sum = 0.0;
    let mut opens: HashMap<String, OpenPosition> = HashMap::new();
    let mut initial_cash: Option<f64> = None;
    let mut last_cash: Option<f64> = None;

    for r in &session_rows {
        last_cash = Some(r.cash_after);
        match r.event.as_str() {
            "SESSION_START" => {
                if let Some(v) = parse_initial_cash_from_notes(&r.notes) {
                    initial_cash = Some(v);
                }
            }
            "OPEN" if r.pair_label != "-" && !r.pair_label.is_empty() => {
                open_event_count += 1;
                opens.insert(
                    r.pair_label.clone(),
                    OpenPosition {
                        n: r.n,
                        entry_capital: r.entry_capital,
                        fees_open: r.fees_open,
                        gas_open: r.gas_open,
                    },
                );
            }
            "CLOSE" if r.pair_label != "-" && !r.pair_label.is_empty() => {
                close_count += 1;
                close_realized_sum += r.pnl_realized;
                opens.remove(&r.pair_label);
            }
            _ => {}
        }
    }

    let locked_pnl_still_open: f64 = opens
        .values()
        .map(|p| locked_pnl_at_open(p.n, p.entry_capital, p.fees_open, p.gas_open))
        .sum();
    let u_hold_open_sum: f64 = opens.values().map(|p| u_hold_notional(p.n)).sum();

    print_performance_report(
        session_id,
        session_start_calendar_date(rows, session_id),
        &span,
        initial_cash,
        last_cash,
        u_hold_open_sum,
        open_event_count,
        close_count,
        opens.len(),
        close_realized_sum,
        locked_pnl_still_open,
    );
}

/// 左栏（标签）显示宽度（终端里中文一般占 2 列）。
const LABEL_DISP_W: usize = 18;
/// 右栏（数值）显示宽度。
const VALUE_DISP_W: usize = 44;
const COL_GAP: &str = "  ";
const COL_GAP_DISP_W: usize = 2;
/// 两竖线之间正文总显示宽度。
const INNER_DISP_W: usize = LABEL_DISP_W + COL_GAP_DISP_W + VALUE_DISP_W;

fn pad_fill_display(target: usize, used: usize) -> String {
    " ".repeat(target.saturating_sub(used))
}

fn fit_display_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
        if used + cw > max {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push_str(&pad_fill_display(max, used));
    out
}

fn pad_label(label: &str) -> String {
    let w = label.width();
    if w <= LABEL_DISP_W {
        format!("{}{}", label, pad_fill_display(LABEL_DISP_W, w))
    } else {
        fit_display_width(label, LABEL_DISP_W)
    }
}

fn pad_value(value: &str) -> String {
    let w = value.width();
    if w <= VALUE_DISP_W {
        format!("{}{}", pad_fill_display(VALUE_DISP_W, w), value)
    } else {
        fit_display_width(value, VALUE_DISP_W)
    }
}

fn print_outer_top() {
    println!("  ┌{}┐", "─".repeat(INNER_DISP_W));
}

fn print_outer_bottom() {
    println!("  └{}┘", "─".repeat(INNER_DISP_W));
}

fn print_inner_sep() {
    println!("  ├{}┤", "─".repeat(INNER_DISP_W));
}

fn row_line(label: &str, value: &str) {
    let body = format!("{}{}{}", pad_label(label), COL_GAP, pad_value(value));
    println!("  │{}│", body);
}

fn fmt_money(v: f64) -> String {
    if !v.is_finite() {
        return "—".to_string();
    }
    let cents_i = (v * 100.0).round() as i64;
    let neg = cents_i < 0;
    let abs_cents = cents_i.unsigned_abs();
    let dollars_u = abs_cents / 100;
    let frac = (abs_cents % 100) as u32;
    let mut s = dollars_u.to_string();
    let mut with_commas = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            with_commas.push(',');
        }
        with_commas.push(c);
    }
    s = with_commas.chars().rev().collect();
    let sign = if neg { "-" } else { "" };
    format!("{}{}.{:02} USD", sign, s, frac)
}

fn section_title(title: &str) {
    let w = title.width();
    let line = if w <= INNER_DISP_W {
        format!("{}{}", title, pad_fill_display(INNER_DISP_W, w))
    } else {
        fit_display_width(title, INNER_DISP_W)
    };
    println!("  │{}│", line);
}

fn print_performance_report(
    session_id: u64,
    anchor_date: Option<NaiveDate>,
    span: &SessionSpanInfo,
    initial_cash: Option<f64>,
    last_cash: Option<f64>,
    u_hold_open_sum: f64,
    open_event_count: usize,
    close_count: usize,
    open_pairs_end: usize,
    close_realized_sum: f64,
    locked_pnl_still_open: f64,
) {
    let total_profit = close_realized_sum + locked_pnl_still_open;
    let equity = last_cash.map(|c| c + u_hold_open_sum);
    let delta_from_funds = initial_cash.zip(equity).map(|(ic, e)| e - ic);
    let reconcile_eps = 0.05_f64;
    let reconcile_ok = delta_from_funds
        .map(|d| (d - total_profit).abs() <= reconcile_eps)
        .unwrap_or(true);

    println!();
    print_outer_top();
    row_line("会话编号", &session_id.to_string());
    if let Some(d) = anchor_date {
        row_line("归属 UTC 日", &d.to_string());
    } else {
        row_line("归属 UTC 日", "—");
    }
    print_inner_sep();

    section_title("时间跨度");
    match (span.start, span.end) {
        (Some(s), Some(e)) => {
            row_line("启动", &fmt_dt_hm(s));
            row_line("结束", &fmt_dt_hm(e));
            row_line("墙钟跨度", &fmt_wall_span(s, e));
        }
        (Some(s), None) => {
            row_line("启动", &fmt_dt_hm(s));
            row_line("结束", "—");
            row_line("墙钟跨度", "—");
        }
        _ => {
            row_line("启动", "—");
            row_line("结束", "—");
            row_line("墙钟跨度", "—");
        }
    }
    match (span.cycle_min, span.cycle_max) {
        (Some(lo), Some(hi)) => {
            let n = hi.saturating_sub(lo).saturating_add(1);
            row_line("监控周期数", &format!("{} 个", n));
        }
        _ => row_line("监控周期数", "—"),
    }
    print_inner_sep();

    section_title("资金");
    row_line(
        "初始资金",
        &initial_cash.map(fmt_money).unwrap_or_else(|| "—".to_string()),
    );
    row_line(
        "期末现金",
        &last_cash.map(fmt_money).unwrap_or_else(|| "—".to_string()),
    );
    row_line(
        "到期估算权益",
        &equity.map(fmt_money).unwrap_or_else(|| "—".to_string()),
    );
    match (initial_cash, delta_from_funds) {
        (Some(_), Some(d)) => row_line("较初始变动", &fmt_money(d)),
        _ => row_line("较初始变动", "—"),
    }
    print_inner_sep();

    section_title("仓位");
    row_line("开仓笔数", &open_event_count.to_string());
    row_line("平仓笔数", &close_count.to_string());
    row_line("期末未平", &open_pairs_end.to_string());
    print_inner_sep();

    section_title("利润");
    row_line("总利润", &fmt_money(total_profit));
    row_line("未平仓锁定", &fmt_money(locked_pnl_still_open));
    row_line("平仓利润", &fmt_money(close_realized_sum));

    print_outer_bottom();

    println!();
    println!("* 选日规则：仅列出「会话启动时刻」落在该 UTC 日的会话；跨自然日不拆分会话，整段绩效仍按会话汇总。");
    println!("* 平仓利润为已平仓实现盈亏加总；未平仓锁定按持有到期回款估计减开仓成本；总利润为二者之和，与较初始变动一致。");
    println!("* 到期权益为期末现金加未平仓估算回款；结算侧费率按双边各百分之一估算。");
    println!("* 初始资金来自会话启动记录；期末现金取时间序上最后一笔后的现金余额。");
    if !reconcile_ok {
        if let Some(d) = delta_from_funds {
            println!(
                "* 分项利润与权益差存在 {:.4} 美元量级的偏差，多为舍入或记录异常，建议核对源文件。",
                (d - total_profit).abs()
            );
        }
    }
}

fn parse_initial_cash_from_notes(notes: &str) -> Option<f64> {
    for part in notes.split_whitespace() {
        if let Some(rest) = part.strip_prefix("initial_cash=") {
            return rest.parse().ok();
        }
    }
    None
}

fn pick_date(dates: &BTreeSet<NaiveDate>) -> Result<Option<NaiveDate>> {
    if dates.is_empty() {
        println!("没有可解析的「会话启动日」（UTC）。请确认文件中包含会话开始记录。");
        return Ok(None);
    }

    let years: Vec<i32> = dates.iter().map(|d| d.year()).collect::<BTreeSet<_>>().into_iter().collect();
    let y_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("选择 UTC 年份（会话启动日）")
        .items(
            &years
                .iter()
                .map(|y| y.to_string())
                .collect::<Vec<_>>(),
        )
        .default(0)
        .interact()?;
    let year = years[y_idx];

    let months: Vec<u32> = dates
        .iter()
        .filter(|d| d.year() == year)
        .map(|d| d.month())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let m_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("选择 UTC 月份")
        .items(
            &months
                .iter()
                .map(|m| format!("{:02}", m))
                .collect::<Vec<_>>(),
        )
        .default(0)
        .interact()?;
    let month = months[m_idx];

    let days: Vec<u32> = dates
        .iter()
        .filter(|d| d.year() == year && d.month() == month)
        .map(|d| d.day())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let d_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("选择 UTC 日")
        .items(
            &days
                .iter()
                .map(|d| format!("{:02}", d))
                .collect::<Vec<_>>(),
        )
        .default(0)
        .interact()?;
    let day = days[d_idx];

    Ok(Some(
        NaiveDate::from_ymd_opt(year, month, day).context("非法日期")?,
    ))
}

fn main() -> Result<()> {
    let path = env::var(ENV_CSV).unwrap_or_else(|_| DEFAULT_CSV.to_string());
    let path = Path::new(&path);
    println!("读取 {}", path.display());
    let rows = load_csv(path)?;

    let dates = collect_session_anchor_dates(&rows);
    let Some(target) = pick_date(&dates)? else {
        return Ok(());
    };
    println!("\n选中 UTC 日（会话启动日）：{}", target);

    let sessions = sessions_started_on_date(&rows, target);
    if sessions.is_empty() {
        println!("该日无会话启动记录。");
        return Ok(());
    }

    let labels: Vec<String> = sessions
        .iter()
        .map(|sid| {
            let n = rows.iter().filter(|r| r.session_id == *sid).count();
            format!("编号 {} · {} 条记录", sid, n)
        })
        .collect();

    let s_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("选择会话")
        .items(&labels)
        .default(0)
        .interact()?;
    let session_id = sessions[s_idx];

    analyze_session(&rows, session_id);
    Ok(())
}
