//! Kalshi / Polymarket 组合工具库（快照、现金流、利润、换手；Relayer 自动化）。

#[path = "../../trader/src/system_params.rs"]
mod system_params;

pub mod cashflow;
pub mod http_client_env;
pub mod gamma;
pub mod kalshi;
pub mod mapping;
pub mod pm;
pub mod pm_relayer_automation;
pub mod profit;
pub mod snapshot_report;
pub mod turnover;

pub use mapping::{load_pairs_from_desk_events, ArbPair, StrategyLegs};
