use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use plotters::prelude::*;

use crate::kalshi::{self, KalshiAuth};

#[derive(serde::Deserialize)]
struct DeskEvent {
    time_utc_rfc3339: String,
    status: String,
    #[allow(dead_code)]
    pair_label: String,
}

pub fn trades_per_day_from_jsonl(path: &Path) -> Result<BTreeMap<NaiveDate, u32>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut m: BTreeMap<NaiveDate, u32> = BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: DeskEvent = serde_json::from_str(line).context("jsonl desk event")?;
        if ev.status != "OK" {
            continue;
        }
        let d = chrono::DateTime::parse_from_rfc3339(&ev.time_utc_rfc3339)
            .map(|x| x.date_naive())
            .unwrap_or_else(|_| {
                ev.time_utc_rfc3339
                    .split('T')
                    .next()
                    .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                    .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
            });
        *m.entry(d).or_insert(0) += 1;
    }
    Ok(m)
}

pub fn render_trades_chart(days: &BTreeMap<NaiveDate, u32>, out_png: &Path) -> Result<()> {
    let _ = out_png.parent().map(std::fs::create_dir_all);
    let root = BitMapBackend::new(out_png, (900, 480)).into_drawing_area();
    root.fill(&WHITE)?;
    if days.is_empty() {
        return Ok(());
    }
    let max_c = *days.values().max().unwrap_or(&1) as f64;
    let x0 = *days.keys().next().unwrap();
    let x1 = *days.keys().next_back().unwrap();
    let mut chart = ChartBuilder::on(&root)
        .caption("OK 套利事件 / 日（来自 desk_events.jsonl）", ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(40)
        .y_label_area_size(40)
        .build_cartesian_2d(x0..x1, 0f64..max_c * 1.15)?;

    chart.configure_mesh().light_line_style(WHITE).draw()?;
    chart.draw_series(LineSeries::new(
        days.iter().map(|(d, c)| (*d, *c as f64)),
        BLUE.stroke_width(3),
    ))?;
    root.present()?;
    Ok(())
}

/// 按映射 ticker 拉 Kalshi **官方 settlements**，按日累加 `revenue/100 - fee_cost`。
pub async fn cumulative_kalshi_settlements(
    http: &reqwest::Client,
    auth: &KalshiAuth,
    tickers: &[String],
) -> Result<Vec<(NaiveDate, f64)>> {
    let mut points: Vec<(NaiveDate, f64)> = Vec::new();
    for t in tickers {
        let rows = kalshi::all_settlements_for_ticker(http, auth, t).await?;
        for s in rows {
            let rev = s["revenue"].as_i64().unwrap_or(0) as f64 / 100.0;
            let fee = s["fee_cost"].as_str().map(kalshi::fp_dollars).unwrap_or(0.0);
            let pnl = rev - fee;
            let settled = s["settled_time"]
                .as_str()
                .and_then(|x| chrono::DateTime::parse_from_rfc3339(x).ok())
                .map(|x| x.date_naive())
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
            points.push((settled, pnl));
        }
    }
    points.sort_by(|a, b| a.0.cmp(&b.0));
    let mut cum = Vec::new();
    let mut run = 0.0_f64;
    for (d, p) in points {
        run += p;
        cum.push((d, run));
    }
    Ok(cum)
}

pub fn render_cumulative_realized(series: &[(NaiveDate, f64)], out_png: &Path) -> Result<()> {
    let _ = out_png.parent().map(std::fs::create_dir_all);
    let root = BitMapBackend::new(out_png, (900, 480)).into_drawing_area();
    root.fill(&WHITE)?;
    if series.is_empty() {
        return Ok(());
    }
    let ymin = series
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::INFINITY, f64::min)
        .min(0.0);
    let ymax = series
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::NEG_INFINITY, f64::max)
        .max(0.0);
    let x0 = series.first().unwrap().0;
    let x1 = series.last().unwrap().0;
    let mut chart = ChartBuilder::on(&root)
        .caption(
            "Kalshi 官方 settlements 累计（revenue−fee，仅 KS 腿；未含 PM）",
            ("sans-serif", 20),
        )
        .margin(10)
        .x_label_area_size(40)
        .y_label_area_size(50)
        .build_cartesian_2d(x0..x1, ymin..ymax * 1.05)?;

    chart.configure_mesh().draw()?;
    chart.draw_series(LineSeries::new(series.iter().map(|(d, y)| (*d, *y)), RED))?;
    root.present()?;
    Ok(())
}
