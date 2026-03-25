// src/tracking.rs
use chrono::{DateTime, Utc};

use crate::market::{Market, MarketPrices};
use crate::market_filter::tracked_pair_exceeds_horizon;

/// Polymarket / Kalshi 二元侧（YES/NO）取反，用于同一匹配对下「另一腿」方向（PM 买 YES+KS 买 NO 与 PM 买 NO+KS 买 YES）。
pub fn flip_binary_side(side: &str) -> Option<&'static str> {
    match side {
        "YES" => Some("NO"),
        "NO" => Some("YES"),
        _ => None,
    }
}

/// 追踪键 / 模拟盘持仓键：同一对市场 + 具体 PM 侧与 Kalshi 侧各唯一。
pub fn oriented_track_id(pm_id: &str, ks_id: &str, pm_side: &str, kalshi_side: &str) -> String {
    format!("{}:{}|{}|{}", pm_id, ks_id, pm_side, kalshi_side)
}

/// 追踪的套利对
pub struct TrackedArbitrage {
    pub pair_id: String,
    pub pm_market: Market,
    pub kalshi_market: Market,
    pub similarity: f64,
    pub pm_side: String,
    pub kalshi_side: String,
    pub needs_inversion: bool,
    pub last_pm_price: Option<MarketPrices>,
    pub last_kalshi_price: Option<MarketPrices>,
    pub best_profit: f64,
    pub last_check: DateTime<Utc>,
    pub active: bool,
}

impl TrackedArbitrage {
    pub fn new(
        pm_market: Market, 
        kalshi_market: Market, 
        similarity: f64,
        pm_side: String,
        kalshi_side: String,
        needs_inversion: bool,
    ) -> Self {
        Self {
            pair_id: oriented_track_id(
                &pm_market.market_id,
                &kalshi_market.market_id,
                &pm_side,
                &kalshi_side,
            ),
            pm_market,
            kalshi_market,
            similarity,
            pm_side,
            kalshi_side,
            needs_inversion,
            last_pm_price: None,
            last_kalshi_price: None,
            best_profit: 0.0,
            last_check: Utc::now(),
            active: true,
        }
    }
}

/// 监控状态
pub struct MonitorState {
    pub tracked_pairs: Vec<TrackedArbitrage>,
    pub current_cycle: usize,
    pub full_match_interval: usize,
    pub market_limit: usize,
}

impl MonitorState {
    pub fn new(full_match_interval: usize, market_limit: usize) -> Self {
        Self {
            tracked_pairs: Vec::new(),
            current_cycle: 0,
            full_match_interval,
            market_limit,
        }
    }

    pub fn should_full_match(&self) -> bool {
        self.current_cycle % self.full_match_interval == 0
    }

    pub fn next_cycle(&mut self) {
        self.current_cycle += 1;
    }

    pub fn update_tracked_pairs(&mut self, new_matches: Vec<(Market, Market, f64, String, String, bool)>) {
        // 标记旧的为不活跃
        for pair in &mut self.tracked_pairs {
            pair.active = false;
        }

        // 添加新的匹配对
        for (pm, kalshi, similarity, pm_side, kalshi_side, needs_inversion) in new_matches {
            let pair_id = oriented_track_id(&pm.market_id, &kalshi.market_id, &pm_side, &kalshi_side);
            if let Some(existing) = self.tracked_pairs.iter_mut().find(|p| p.pair_id == pair_id) {
                existing.active = true;
                existing.similarity = similarity;
                existing.pm_side = pm_side;
                existing.kalshi_side = kalshi_side;
                existing.needs_inversion = needs_inversion;
            } else {
                self.tracked_pairs.push(TrackedArbitrage::new(
                    pm, kalshi, similarity, pm_side, kalshi_side, needs_inversion
                ));
            }
        }

        // 清理不活跃的
        self.tracked_pairs.retain(|p| p.active);
    }

    pub fn get_active_pairs(&self) -> Vec<&TrackedArbitrage> {
        self.tracked_pairs.iter().filter(|p| p.active).collect()
    }

    /// 剔除任一侧「有解析日且晚于 horizon」的追踪对（与全量池筛选规则一致）。
    pub fn prune_tracked_beyond_resolution_horizon(&mut self, now: DateTime<Utc>) {
        self.tracked_pairs.retain(|p| {
            !tracked_pair_exceeds_horizon(&p.pm_market, &p.kalshi_market, now)
        });
    }
}