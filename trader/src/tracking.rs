// src/tracking.rs
use chrono::{DateTime, NaiveDate, Utc};

use crate::market::Market;
use crate::market_filter::tracked_pair_exceeds_horizon;

/// Polymarket / Kalshi 二元侧（YES/NO）取反，用于同一匹配对下「另一腿」方向（PM 买 YES+KS 买 NO 与 PM 买 NO+KS 买 YES）。
pub fn flip_binary_side(side: &str) -> Option<&'static str> {
    match side {
        "YES" => Some("NO"),
        "NO" => Some("YES"),
        _ => None,
    }
}

/// 追踪键：同一对市场 + 具体 PM 侧与 Kalshi 侧各唯一。
pub fn oriented_track_id(pm_id: &str, ks_id: &str, pm_side: &str, kalshi_side: &str) -> String {
    format!("{}:{}|{}|{}", pm_id, ks_id, pm_side, kalshi_side)
}

/// 追踪的套利对
#[derive(Clone)]
pub struct TrackedArbitrage {
    pub pair_id: String,
    pub pm_market: Market,
    pub kalshi_market: Market,
    pub similarity: f64,
    pub pm_side: String,
    pub kalshi_side: String,
    pub needs_inversion: bool,
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
            best_profit: 0.0,
            last_check: Utc::now(),
            active: true,
        }
    }
}

/// 写入 `desk_events` / 追踪 CSV 时携带的「会话内周期」上下文（单线程捕获，并发任务在 spawn 前 Clone）。
#[derive(Clone, Copy)]
pub struct DeskCycleCtx {
    pub trading_session_utc_date: NaiveDate,
    pub session_cycle_id: usize,
    pub global_tick: usize,
}

/// 监控状态
pub struct MonitorState {
    pub tracked_pairs: Vec<TrackedArbitrage>,
    /// 单调递增 tick，仅用于「每隔 N tick 做一次全量匹配」调度（**不**随 UTC 日切归零）。
    pub global_tick: usize,
    pub full_match_interval: usize,
    /// 最近一次全量拉取并过滤后的 PM 池大小（追踪周期沿用，供 CSV 参数列）
    pub last_pm_pool: usize,
    /// 最近一次全量拉取并过滤后的 Kalshi 池大小
    pub last_ks_pool: usize,
    /// 当前交易会话锚定的 **UTC 日历日**；跨 UTC 零点后由 [`MonitorState::roll_session_if_new_utc_day`] 更新。
    pub trading_session_utc_date: NaiveDate,
    /// **仅在同一 `trading_session_utc_date` 内**可横向比较的周期序号；新 UTC 日归零后从 0 重新开始。
    pub session_cycle_seq: usize,
}

impl MonitorState {
    pub fn new(full_match_interval: usize) -> Self {
        let d = Utc::now().date_naive();
        Self {
            tracked_pairs: Vec::new(),
            global_tick: 0,
            full_match_interval,
            last_pm_pool: 0,
            last_ks_pool: 0,
            trading_session_utc_date: d,
            session_cycle_seq: 0,
        }
    }

    pub fn should_full_match(&self) -> bool {
        self.global_tick % self.full_match_interval == 0
    }

    /// 每个「监控周期」迭代开始时调用：若已进入新 UTC 日，则开启新会话并将 `session_cycle_seq` 置 0。
    pub fn roll_session_if_new_utc_day(&mut self) {
        let today = Utc::now().date_naive();
        if today != self.trading_session_utc_date {
            self.trading_session_utc_date = today;
            self.session_cycle_seq = 0;
        }
    }

    /// 主循环每次睡眠结束后调用：`global_tick` 与 `session_cycle_seq` 各 +1（新 UTC 日的归零仅在下一周期开头的 [`roll_session_if_new_utc_day`]）。
    pub fn advance_tick_after_iteration(&mut self) {
        self.global_tick += 1;
        self.session_cycle_seq += 1;
    }

    /// 初始化阶段：完成周期 #0 全量匹配后，将「下一轮追踪」标为会话内第 1 周期（兼容原 `current_cycle` 语义）。
    pub fn finish_init_advance_scheduling_counters(&mut self) {
        self.global_tick = 1;
        self.session_cycle_seq = 1;
    }

    pub fn desk_cycle_ctx(&self) -> DeskCycleCtx {
        DeskCycleCtx {
            trading_session_utc_date: self.trading_session_utc_date,
            session_cycle_id: self.session_cycle_seq,
            global_tick: self.global_tick,
        }
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

    /// 剔除任一侧「有解析日且晚于 horizon」的追踪对（与全量池筛选规则一致）。
    pub fn prune_tracked_beyond_resolution_horizon(&mut self, now: DateTime<Utc>) {
        self.tracked_pairs.retain(|p| {
            !tracked_pair_exceeds_horizon(&p.pm_market, &p.kalshi_market, now)
        });
    }
}