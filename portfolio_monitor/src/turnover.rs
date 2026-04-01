//! 回望 N 天：`desk_events.jsonl` 中 OK 事件次数与成交额曲线（覆盖写入 PNG）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use plotters::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct DeskEv {
    time_utc_rfc3339: String,
    status: String,
    #[serde(default)]
    capital_used_model: f64,
}

fn pad_axis(x0: NaiveDate, x1: NaiveDate) -> (NaiveDate, NaiveDate) {
    if x0 == x1 {
        (
            x0.checked_sub_signed(Duration::days(1)).unwrap_or(x0),
            x1.checked_add_signed(Duration::days(1)).unwrap_or(x1),
        )
    } else {
        (x0, x1)
    }
}

/// 成交额口径：双边名义近似 = 2 × capital_used_model（USD）。
pub fn run_turnover(events_path: &Path, out_dir: &Path, lookback_days: u32) -> Result<()> {
    if lookback_days == 0 {
        anyhow::bail!("回望天数须 ≥ 1");
    }
    std::fs::create_dir_all(out_dir).ok();
    let cutoff = Utc::now() - Duration::days(lookback_days as i64);

    let raw = std::fs::read_to_string(events_path)
        .with_context(|| format!("读取 {}", events_path.display()))?;
    let mut count_by_day: BTreeMap<NaiveDate, u32> = BTreeMap::new();
    let mut vol_by_day: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    let mut total_n = 0u32;
    let mut total_vol = 0.0_f64;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: DeskEv = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if ev.status != "OK" {
            continue;
        }
        let dt = match DateTime::parse_from_rfc3339(&ev.time_utc_rfc3339) {
            Ok(d) => d.with_timezone(&Utc),
            Err(_) => continue,
        };
        if dt < cutoff {
            continue;
        }
        let d = dt.date_naive();
        *count_by_day.entry(d).or_insert(0) += 1;
        let v = 2.0 * ev.capital_used_model;
        *vol_by_day.entry(d).or_insert(0.0) += v;
        total_n += 1;
        total_vol += v;
    }

    println!("回望 {} 天 | OK 事件 {} 笔 | 成交额合计 ≈ {:.2} USD（2×capital_used）", lookback_days, total_n, total_vol);

    let p1 = out_dir.join("turnover_count.png");
    let p2 = out_dir.join("turnover_volume.png");

    if !count_by_day.is_empty() {
        let x0 = *count_by_day.keys().next().unwrap();
        let x1 = *count_by_day.keys().next_back().unwrap();
        let (rx0, rx1) = pad_axis(x0, x1);
        let max_c = *count_by_day.values().max().unwrap_or(&1) as f64;

        let root = BitMapBackend::new(&p1, (900, 480)).into_drawing_area();
        root.fill(&WHITE)?;
        let mut chart = ChartBuilder::on(&root)
            .caption(
                "交易次数 / 日（desk_events status=OK）",
                ("sans-serif", 20),
            )
            .margin(10)
            .x_label_area_size(40)
            .y_label_area_size(40)
            .build_cartesian_2d(rx0..rx1, 0f64..max_c * 1.15 + 0.1)?;
        chart.configure_mesh().light_line_style(WHITE).draw()?;
        chart.draw_series(LineSeries::new(
            count_by_day.iter().map(|(d, c)| (*d, *c as f64)),
            BLUE.stroke_width(3),
        ))?;
        root.present()?;
    } else {
        let root = BitMapBackend::new(&p1, (600, 120)).into_drawing_area();
        root.fill(&WHITE)?;
        root.present()?;
    }

    if !vol_by_day.is_empty() {
        let x0 = *vol_by_day.keys().next().unwrap();
        let x1 = *vol_by_day.keys().next_back().unwrap();
        let (rx0, rx1) = pad_axis(x0, x1);
        let max_v = *vol_by_day.values().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap_or(&1.0);

        let root = BitMapBackend::new(&p2, (900, 480)).into_drawing_area();
        root.fill(&WHITE)?;
        let mut chart = ChartBuilder::on(&root)
            .caption(
                "成交额 / 日 USD（≈2×capital_used_model）",
                ("sans-serif", 20),
            )
            .margin(10)
            .x_label_area_size(40)
            .y_label_area_size(40)
            .build_cartesian_2d(rx0..rx1, 0f64..max_v * 1.12 + 1.0)?;
        chart.configure_mesh().light_line_style(WHITE).draw()?;
        chart.draw_series(LineSeries::new(
            vol_by_day.iter().map(|(d, v)| (*d, *v)),
            GREEN.stroke_width(3),
        ))?;
        root.present()?;
    } else {
        let root = BitMapBackend::new(&p2, (600, 120)).into_drawing_area();
        root.fill(&WHITE)?;
        root.present()?;
    }

    println!("已写入 {:?} 与 {:?}", p1, p2);
    Ok(())
}
