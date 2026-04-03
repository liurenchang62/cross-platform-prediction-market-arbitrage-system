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
    /// Human-readable Kalshi market title (stored by desk_events since upgrade; optional in TOML).
    #[serde(default)]
    pub kalshi_title: Option<String>,
    /// Human-readable Polymarket market title (stored by desk_events since upgrade; optional in TOML).
    #[serde(default)]
    pub pm_title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FileRoot {
    pairs: Vec<ArbPair>,
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

pub fn load_mapping(path: &Path) -> Result<Vec<ArbPair>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取映射 {}", path.display()))?;
    let root: FileRoot = toml::from_str(&raw).context("解析 TOML 映射")?;
    Ok(root.pairs)
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

pub fn load_pairs_merged_with_desk_at(
    config: &Path,
    desk_jsonl: Option<&Path>,
    announce_if_added: bool,
) -> Result<Vec<ArbPair>> {
    let mut pairs = load_mapping(config)?;
    let desk_path = desk_jsonl.unwrap_or_else(|| Path::new(crate::system_params::DESK_EVENTS_JSONL));
    let desk_added = augment_pairs_from_desk_events_jsonl(&mut pairs, desk_path)?;
    if announce_if_added && desk_added > 0 {
        eprintln!(
            "   ℹ  已从 {} 自动补足 {} 条 PM↔Kalshi 映射",
            desk_path.display(),
            desk_added
        );
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
