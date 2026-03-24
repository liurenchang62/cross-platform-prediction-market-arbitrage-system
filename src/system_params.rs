// system_params.rs
//! 系统级常量：HTTP/分页、匹配与索引、市场窗口、模拟交易（Paper）等。

// ==================== 通用参数 ====================
/// 请求间隔（毫秒）
pub const REQUEST_INTERVAL_MS: u64 = 200;

/// 最大重试次数
pub const MAX_RETRIES: u32 = 3;

/// 重试初始等待时间（毫秒）
pub const RETRY_INITIAL_DELAY_MS: u64 = 1000;

// ==================== Polymarket 参数 ====================
/// Polymarket 单次请求最大事件数
pub const POLYMARKET_PAGE_LIMIT: u32 = 200;

/// Polymarket 最大获取市场数
pub const POLYMARKET_MAX_MARKETS: usize = 50000;

// ==================== Kalshi 参数 ====================
/// Kalshi 每页市场数
pub const KALSHI_PAGE_LIMIT: u32 = 1000;

/// Kalshi 最大获取市场数
pub const KALSHI_MAX_MARKETS: usize = 50000;

// ==================== 向量化参数 ====================
/// 最大词汇表大小（降维用）
/// Some(500) 表示限制到500维，None 表示无上限
pub const MAX_VOCAB_SIZE: Option<usize> = None;

// ==================== 匹配参数 ====================
/// 相似度阈值
pub const SIMILARITY_THRESHOLD: f64 = 0.8;

/// 每个 (query, 类别) 在索引中保留的候选条数（精确按余弦排序后截断）。
/// 仅当提高此值时才会扩大初筛候选集；降低可能影响召回，变更前应做审计对比。
pub const SIMILARITY_TOP_K: usize = 15;

/// 全量获取周期（每 N 个追踪周期执行一次全量获取）
pub const FULL_FETCH_INTERVAL: usize = 180;

// ==================== 市场时间窗口 ====================
/// 21 天窗口：按每条 `Market.resolution_date` 与「当前 UTC + 本天数」比较（**与终端 PM/Kalshi「到期」展示同源**）。
/// - Polymarket：由 `clients::parse_polymarket_market_resolution_date` 写入，主字段 **`endDate`**（及既定备用链）。
/// - Kalshi：由 `clients::parse_kalshi_market_resolution_date` 写入，优先 **`expected_expiration_time`**，再 `close_time` 等。
/// 无 `resolution_date` 的市场保留；任一侧日期 **严格晚于** 截止则在全量筛选中剔除，追踪对见 `market_filter::tracked_pair_exceeds_horizon`。
pub const RESOLUTION_HORIZON_DAYS: i64 = 21;

// ==================== 模拟交易（Paper）====================
/// 是否启用模拟盘：仅当本常量为 `true` 时构造 `PaperEngine`（与运行环境变量无关）
pub const PAPER_TRADING_ENABLED: bool = true;

/// 是否写入模拟交易单一 CSV（`PAPER_TRADES_CSV`）；为 `false` 时仍做资金与持仓模拟，但不创建/追加文件
pub const PAPER_WRITE_TRADE_LOG: bool = true;

/// 环境变量名：可选，写入交易 CSV 的 `notes` 列，区分测试轮次
pub const PAPER_RUN_LABEL_ENV: &str = "PAPER_RUN_LABEL";

/// 每进程启动递增，用于 CSV `session_id`
pub const PAPER_SESSION_COUNTER_FILE: &str = "logs/paper_session_counter.txt";

/// 模拟交易记录 CSV
pub const PAPER_TRADES_CSV: &str = "logs/paper_trades.csv";

/// 每腿探针本金上限（USDT），与监控套利验证一致
pub const PAPER_PER_LEG_CAP_USDT: f64 = 100.0;

/// 平仓后同 pair 需等待的监控周期数方可再开仓
pub const PAPER_COOLDOWN_CYCLES: u32 = 5;

/// 模拟初始资金（USDT）
pub const PAPER_INITIAL_CASH: f64 = 100_000.0;

/// 同时最多持仓对数
pub const PAPER_MAX_OPEN_POSITIONS: usize = 50;

/// 提前平仓相对「持有到期」估算回款的最小优势（美元），避免略优即平
pub const PAPER_MIN_EDGE_EARLY_USD: f64 = 0.5;

/// 到期结算时按名义 n 估算的额外手续费（与两腿费率之和 × n 对齐）
pub fn paper_settlement_fee_estimate(n: f64, fee_pm: f64, fee_ks: f64) -> f64 {
    if n <= 0.0 || !n.is_finite() {
        return 0.0;
    }
    n * (fee_pm + fee_ks)
}
