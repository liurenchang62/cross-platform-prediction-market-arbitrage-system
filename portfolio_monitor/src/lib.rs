//! 映射驱动的 Kalshi / Polymarket 持仓与对账（独立 crate，供四个 CLI 共用）。

#[path = "../../src/system_params.rs"]
mod system_params;

pub mod charts;
pub mod gamma;
pub mod kalshi;
pub mod mapping;
pub mod pm;
pub mod tasks;

pub use mapping::{load_mapping, ArbPair, StrategyLegs};
