use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

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
}

#[derive(Debug, Deserialize)]
struct FileRoot {
    pairs: Vec<ArbPair>,
}

pub fn load_mapping(path: &Path) -> Result<Vec<ArbPair>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取映射 {}", path.display()))?;
    let root: FileRoot = toml::from_str(&raw).context("解析 TOML 映射")?;
    Ok(root.pairs)
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
