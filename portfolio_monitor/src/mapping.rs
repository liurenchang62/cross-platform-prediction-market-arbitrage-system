use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::pm::pm_clob_token_id_valid;

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyLegs {
    pub pm_buy_leg: String,
    pub kalshi_buy_leg: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArbPair {
    pub id: String,
    pub kalshi_ticker: String,
    pub pm_market_id: String,
    pub pm_buy_token_id: String,
    pub strategy: StrategyLegs,
    /// Human-readable Kalshi market title（常见于 `desk_events` 行内字段）
    #[serde(default)]
    pub kalshi_title: Option<String>,
    /// Human-readable Polymarket market title（常见于 `desk_events` 行内字段）
    #[serde(default)]
    pub pm_title: Option<String>,
}

#[derive(Deserialize)]
struct DeskEventLine {
    pm_market_id: String,
    kalshi_market_id: String,
    pm_buy_token_id: String,
    pm_side: String,
    kalshi_side: String,
    #[serde(default)]
    kalshi_title: Option<String>,
    #[serde(default)]
    pm_title: Option<String>,
}

/// 将 `desk_events.jsonl` 中尚未出现在 `pairs` 内的 (Kalshi ticker, PM token) 追加写入。返回新增条数。
pub fn augment_pairs_from_desk_events_jsonl(
    pairs: &mut Vec<ArbPair>,
    desk_jsonl: &Path,
) -> Result<usize> {
    let mut seen: HashSet<(String, String)> = pairs
        .iter()
        .map(|p| (p.kalshi_ticker.clone(), p.pm_buy_token_id.clone()))
        .collect();

    if !desk_jsonl.exists() {
        return Ok(0);
    }

    let raw = std::fs::read_to_string(desk_jsonl)
        .with_context(|| format!("读取 {}", desk_jsonl.display()))?;
    let mut added = 0_usize;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: DeskEventLine = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !pm_clob_token_id_valid(&row.pm_buy_token_id) {
            continue;
        }
        let key = (row.kalshi_market_id.clone(), row.pm_buy_token_id.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key.clone());
        pairs.push(ArbPair {
            id: format!(
                "desk_events:{}:{}",
                row.pm_market_id, row.kalshi_market_id
            ),
            kalshi_ticker: row.kalshi_market_id,
            pm_market_id: row.pm_market_id,
            pm_buy_token_id: row.pm_buy_token_id,
            strategy: StrategyLegs {
                pm_buy_leg: row.pm_side,
                kalshi_buy_leg: row.kalshi_side,
            },
            kalshi_title: row.kalshi_title.filter(|s| !s.is_empty()),
            pm_title: row.pm_title.filter(|s| !s.is_empty()),
        });
        added += 1;
    }
    Ok(added)
}

/// 仅从 `desk_events.jsonl` 构建 PM↔Kalshi 映射（无 TOML 叠加）。
pub fn load_pairs_from_desk_events(
    desk_jsonl: &Path,
    announce: bool,
) -> Result<Vec<ArbPair>> {
    let mut pairs = Vec::new();
    augment_pairs_from_desk_events_jsonl(&mut pairs, desk_jsonl)?;
    if announce {
        if pairs.is_empty() {
            eprintln!(
                "   ℹ  desk_events 未产生映射（文件缺失或无有效行）: {}",
                desk_jsonl.display()
            );
        } else {
            eprintln!(
                "   ℹ  PM↔Kalshi 映射 {} 条（desk_events: {}）",
                pairs.len(),
                desk_jsonl.display()
            );
        }
    }
    Ok(pairs)
}

impl ArbPair {
    pub fn pm_side_lower(&self) -> String {
        self.strategy.pm_buy_leg.trim().to_ascii_lowercase()
    }
    pub fn ks_side_lower(&self) -> String {
        self.strategy.kalshi_buy_leg.trim().to_ascii_lowercase()
    }

    pub fn ks_open_contracts_from_position_fp(position_fp: f64) -> f64 {
        position_fp.abs()
    }
}
