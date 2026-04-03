//! 实盘套利监控：HTTP/分页、匹配与索引、资金标尺、Kalshi/Polymarket 环境变量名。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

// ==================== 实盘：下单安全锁 ====================
/// 用户启动时输入 `CONFIRM` 后为 `true`（或由单测/工具显式设置）。
/// 任何可能改变资金/仓位的请求入口处必须 [`require_live_orders_allowed`]，防止误调用。
static LIVE_ARBITRAGE_ORDERS_ALLOWED: AtomicBool = AtomicBool::new(false);

/// 在 `main` 完成 `CONFIRM` 后立即设为 `true`。
pub fn set_live_arbitrage_orders_allowed(allowed: bool) {
    LIVE_ARBITRAGE_ORDERS_ALLOWED.store(allowed, Ordering::SeqCst);
}

#[inline]
pub fn live_arbitrage_orders_allowed() -> bool {
    LIVE_ARBITRAGE_ORDERS_ALLOWED.load(Ordering::SeqCst)
}

/// Kalshi IOC、PM FOK、Kalshi 卖平等路径的硬门槛：未解锁则返回 Err。
pub fn require_live_orders_allowed() -> Result<()> {
    if !live_arbitrage_orders_allowed() {
        bail!(
            "安全锁：未进入实盘确认状态，禁止执行下单/卖平等任何可能改变资金或仓位的请求。"
        );
    }
    Ok(())
}

// ==================== 通用 HTTP ====================
pub const REQUEST_INTERVAL_MS: u64 = 200;
pub const MAX_RETRIES: u32 = 3;
pub const RETRY_INITIAL_DELAY_MS: u64 = 1000;

// ==================== Polymarket（Gamma / CLOB 读） ====================
pub const POLYMARKET_PAGE_LIMIT: u32 = 200;
pub const POLYMARKET_MAX_MARKETS: usize = 50000;

// ==================== Kalshi（行情） ====================
pub const KALSHI_PAGE_LIMIT: u32 = 1000;
pub const KALSHI_MAX_MARKETS: usize = 50000;

/// 生产环境 Trade API 根（无末尾 `/`）。与官方文档一致。
pub const KALSHI_TRADE_API_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";

// ==================== 向量化 / 匹配（与原版一致） ====================
pub const MAX_VOCAB_SIZE: Option<usize> = None;
pub const SIMILARITY_THRESHOLD: f64 = 0.85;
pub const SIMILARITY_TOP_K: usize = 15;
pub const MATCH_MATMUL_CHUNK_ROWS: usize = 512;
pub const FULL_FETCH_INTERVAL: usize = 180;
pub const RESOLUTION_HORIZON_DAYS: i64 = 21;

// ==================== 实盘资金标尺（美元） ====================
/// 当 [`set_session_reference_capital_usd`] 尚未调用时（单测/工具），`arb_probe_caps` 回退用到的参考总资金。
pub const DEFAULT_REFERENCE_CAPITAL_USD: f64 = 500.0;

static SESSION_REFERENCE_CAPITAL_USD: OnceLock<f64> = OnceLock::new();

/// 交易周期开始、算出 \(\min(KS\_T0, PM\_T0)\) 后写入；之后全进程 [`arb_probe_caps`] 与非备用路径一致用该值。
pub fn set_session_reference_capital_usd(usd: f64) {
    let _ = SESSION_REFERENCE_CAPITAL_USD.set(usd.max(0.0));
}

#[inline]
pub fn session_reference_capital_usd() -> f64 {
    *SESSION_REFERENCE_CAPITAL_USD
        .get()
        .unwrap_or(&DEFAULT_REFERENCE_CAPITAL_USD)
}

/// **已弃用别名**：请用 [`DEFAULT_REFERENCE_CAPITAL_USD`] 或 [`session_reference_capital_usd`].
pub const TOTAL_CAPITAL_USD: f64 = DEFAULT_REFERENCE_CAPITAL_USD;

/// 单市场对最大名义占参考总盘子比例（两腿合计上限因子）。
pub const PAIR_CAP_FRAC_OF_CAPITAL: f64 = 0.01;

/// 单腿探针名义占参考总盘子比例（PM 或 Kalshi 各一腿）。
pub const PER_LEG_CAP_FRAC_OF_CAPITAL: f64 = 0.005;

/// 缓冲资金比例：仅允许动用 (1 − 此值) × 可用余额 参与 Kalshi 侧收缩（与原先纸面「可用比例」语义一致，此处为 30% 缓冲 → 最多用 70%）。
pub const KALSHI_BALANCE_DEPLOYABLE_FRAC: f64 = 0.70;

// ==================== 第二腿（Kalshi 先成交后）：时间窗 / 卖平限价 ====================
/// Kalshi 第一腿成交后，第二腿（PM 对冲 + 可选卖平）的最长决策时间（秒）。
pub const LIVE_SECOND_LEG_DEADLINE_SECS: u64 = 15;

/// 仅在「簿上 Kalshi bid 总深度 ≥ 剩余份数」且「PM ask 总深度 ≥ 剩余份数」时，才比较对冲净利 H 与卖平净利 U。  
/// 若 **H < U − pair_cap×本比例**，则选择卖平 Kalshi（IOC）。比例锚定每对名义上限 `pair_cap`（当前 **0.2%**）。
pub const LIVE_HEDGE_VS_UNWIND_MARGIN_FRAC_OF_PAIR_CAP: f64 = 0.002;

/// Kalshi IOC **卖出**（卖平剩余敞口）：最低可接受限价（美分/份）。`1` 表示尽量按可见 bid 撮合。
pub const STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS: i64 = 1;

// ==================== 费用模型：非交易摩擦（HTTP 主路径：极小 Gas 绝对值 + 按每对上限比例缓冲） ====================
/// Polymarket `GET /fee-rate`（`base_fee`，bps）**拉取失败**时采用的保守替代值：按 **更高** taker 费估算净利，避免用 0 低估或整笔丢弃识别。  
/// 应 ≥ 常见 `base_fee` 上界；可按实盘观察在 `system_params` 微调。
pub const PM_TAKER_FEE_BPS_WHEN_FETCH_FAILS: u32 = 250;

/// HTTP/CLOB 下单时链上 Gas 近似为 0；仍留极小美元上界作保守杂项。
pub const LIVE_ARBIT_GAS_ASSUMED_USD: f64 = 0.005;
/// 盈余缓冲：按 [`arb_probe_caps`] 第二项「每对名义上限」`pair_cap` 收取 `pair_cap × 本比例`（当前 **0.1%**）。
pub const LIVE_ARBIT_SURPLUS_BUFFER_FRAC_OF_PAIR_CAP: f64 = 0.001;

/// 可接受最小净利润：识别与实盘共用；模型中净利须 **大于** `pair_cap × 本比例`（相对「每对名义上限」）。  
/// 例：`0.15` = 至少赚到该对名义上限的 **15%**（`pair_cap≈$5` 时约 **$0.75**）。仅在 `system_params` 改此常量即可调门槛。
pub const LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP: f64 = 0.1;

#[inline]
pub fn live_arbit_surplus_buffer_usd(pair_cap_usdt: f64) -> f64 {
    pair_cap_usdt.max(0.0) * LIVE_ARBIT_SURPLUS_BUFFER_FRAC_OF_PAIR_CAP
}

#[inline]
pub fn live_arbit_min_net_profit_usd(pair_cap_usdt: f64) -> f64 {
    pair_cap_usdt.max(0.0) * LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP
}

#[inline]
pub fn arb_probe_caps_from_reference(reference_capital_usd: f64) -> (f64, f64) {
    let r = reference_capital_usd.max(0.0);
    (
        r * PER_LEG_CAP_FRAC_OF_CAPITAL,
        r * PAIR_CAP_FRAC_OF_CAPITAL,
    )
}

/// 单腿名义上限、每对名义上限（美元）。优先使用 [`session_reference_capital_usd`]（启动快照后已设置）。
#[inline]
pub fn arb_probe_caps() -> (f64, f64) {
    arb_probe_caps_from_reference(session_reference_capital_usd())
}

// ==================== 会话风控：相对启动快照 T0（分平台）====================
/// 该平台启动时总资产 \(T_0\) ≥ 本阈值（**含等于**）则归为「大额」档，现金预警用 `SESSION_LARGE_WARN_CASH_FRAC_OF_T0`。
pub const SESSION_LARGE_CAPITAL_THRESHOLD_USD: f64 = 50_000.0;
/// 大额档：当前可交易现金（Kalshi：`/portfolio/balance` 美元；PM：CLOB Collateral）低于 `本比例 × 该平台 T_0` 时触发预警。
pub const SESSION_LARGE_WARN_CASH_FRAC_OF_T0: f64 = 0.30;
/// 小额档：现金低于「会话参考总资金 × `PER_LEG_CAP_FRAC_OF_CAPITAL` × 本系数」（与 [`arb_probe_caps`].0×1.1 等价）时触发预警。
pub const SESSION_SMALL_WARN_PER_LEG_CAP_MULT: f64 = 1.1;
/// 再平衡提示：一侧已预警时，若另一侧现金 ≥ `本比例 × (KS_T0 + PM_T0)`（两侧启动快照之和），推飞书（单侧不重发刷屏，见 `session_risk`）。
pub const SESSION_REBALANCE_PEER_CASH_FRAC_OF_COMBINED_T0: f64 = 0.20;

// ==================== 秘钥环境变量（不设默认值，由系统/用户态注入） ====================
/// Kalshi API Key ID（UUID）
pub const KALSHI_API_KEY_ID_ENV: &str = "KALSHI_API_KEY_ID";
/// Kalshi RSA 私钥 PEM 文件路径
pub const KALSHI_PRIVATE_KEY_PATH_ENV: &str = "KALSHI_PRIVATE_KEY_PATH";

/// Polymarket CLOB 私钥：见官方 SDK 常量 `PRIVATE_KEY_VAR`（环境变量名 **`POLYMARKET_PRIVATE_KEY`**）。  
/// `0` = EOA，`1` = Proxy，`2` = GnosisSafe（与官方文档一致）
pub const POLYMARKET_SIGNATURE_TYPE_ENV: &str = "POLYMARKET_SIGNATURE_TYPE";
/// Polymarket 私钥（与 `polymarket-client-sdk` 的 `PRIVATE_KEY_VAR` 一致）：可直接为 hex 字符串。
pub const POLYMARKET_PRIVATE_KEY_ENV: &str = "POLYMARKET_PRIVATE_KEY";
/// 文本文件路径，文件内容为 hex 私钥（可带 `0x`、单行即可）；**非空时优先于** `POLYMARKET_PRIVATE_KEY`。
pub const POLYMARKET_PRIVATE_KEY_PATH_ENV: &str = "POLYMARKET_PRIVATE_KEY_PATH";

/// 去掉空白与成对引号（用户在「环境变量」GUI 里有时会误加 `"..."`）。
pub fn normalize_env_file_path(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '\u{201c}' || c == '\u{201d}')
        .to_string()
}

/// 便于排障：不向日志打印秘钥，只描述「是否设置 / 长度」。
pub fn polymarket_private_key_env_diag() -> String {
    let p = std::env::var_os(POLYMARKET_PRIVATE_KEY_PATH_ENV);
    let k = std::env::var_os(POLYMARKET_PRIVATE_KEY_ENV);
    let p_s = p.as_ref().and_then(|v| v.to_str());
    let k_s = k.as_ref().and_then(|v| v.to_str());
    let p_len = p_s.map(|s| s.trim().len()).unwrap_or(0);
    let k_len = k_s.map(|s| s.trim().len()).unwrap_or(0);
    format!(
        "{POLYMARKET_PRIVATE_KEY_PATH_ENV}: {} | {POLYMARKET_PRIVATE_KEY_ENV}: {}（长度分别为 path={} inline={}；non-UTF8 则长度记 0）",
        if p.is_some() { "已设置" } else { "未设置" },
        if k.is_some() { "已设置" } else { "未设置" },
        p_len,
        k_len
    )
}

/// 优先读 `POLYMARKET_PRIVATE_KEY_PATH`；否则读 `POLYMARKET_PRIVATE_KEY`。内容为 EVM 私钥 hex（可选 `0x`），取首条非空非 `#` 行。
pub fn load_polymarket_private_key_hex() -> Result<String> {
    if let Some(os) = std::env::var_os(POLYMARKET_PRIVATE_KEY_PATH_ENV) {
        let p = os.to_str().with_context(|| {
            format!(
                "{POLYMARKET_PRIVATE_KEY_PATH_ENV} 须为 UTF-8 字符串（路径请不要含非法编码）；可改用 {POLYMARKET_PRIVATE_KEY_ENV} 内联 hex"
            )
        })?;
        let path_norm = normalize_env_file_path(p);
        if path_norm.is_empty() {
            bail!(
                "{POLYMARKET_PRIVATE_KEY_PATH_ENV} 已定义但去掉空白与引号后为空；请填入 txt 的绝对路径，或删除该变量并改用 {POLYMARKET_PRIVATE_KEY_ENV}"
            );
        }
        let mut contents = std::fs::read_to_string(&path_norm).with_context(|| {
            format!(
                "无法读取 {POLYMARKET_PRIVATE_KEY_PATH_ENV} 文件（路径字符数 {}）。请确认磁盘路径存在且当前进程有读权限",
                path_norm.len()
            )
        })?;
        if contents.starts_with('\u{FEFF}') {
            contents.remove(0);
        }
        let hex = extract_hex_private_key_line(contents.as_str())
            .context("解析私钥文件内容（需为 hex）")?;
        if hex.is_empty() {
            bail!("{POLYMARKET_PRIVATE_KEY_PATH_ENV} 文件解析后无有效 hex");
        }
        return Ok(hex);
    }

    let inline = std::env::var(POLYMARKET_PRIVATE_KEY_ENV).with_context(|| {
        format!(
            "未设置 {POLYMARKET_PRIVATE_KEY_PATH_ENV} 且未设置 {POLYMARKET_PRIVATE_KEY_ENV}。\n\
             若在系统里已配置，请**完全关闭并重启 Cursor/终端**后再运行。\n\
             当前诊断：{}",
            polymarket_private_key_env_diag()
        )
    })?;
    let hex = extract_hex_private_key_line(normalize_env_file_path(&inline).as_str())
        .context("解析 POLYMARKET_PRIVATE_KEY")?;
    if hex.is_empty() {
        bail!("{POLYMARKET_PRIVATE_KEY_ENV} 为空");
    }
    Ok(hex)
}

fn extract_hex_private_key_line(text: &str) -> Result<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or_else(|| text.trim());
    if line.is_empty() {
        bail!("私钥文本为空");
    }
    let raw = line
        .strip_prefix("0x")
        .or_else(|| line.strip_prefix("0X"))
        .unwrap_or(line)
        .trim();
    if raw.is_empty() {
        bail!("私钥 hex 为空");
    }
    Ok(raw.to_string())
}

// ==================== 调试（环境变量） ====================
/// 为 `true` 时，追踪周期结束额外打印 Kalshi 订单簿拉取细分与阶梯失败抽样（不致刷屏，每类最多数条）。
/// PowerShell: `$env:KALSHI_ORDERBOOK_DEBUG="1"`
pub fn kalshi_orderbook_debug_enabled() -> bool {
    matches!(
        std::env::var("KALSHI_ORDERBOOK_DEBUG")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes")),
        Ok(true)
    )
}

// ==================== 实盘成交日志（JSON Lines，一行一条） ====================
pub const DESK_EVENTS_JSONL: &str = "logs/desk_events.jsonl";

// ==================== 追踪对与套利快照（回读 `track_backtest`） ====================
pub const TRACKING_PAIRS_CSV: &str = "logs/tracking_pairs.csv";
pub const TRACKING_ARB_EVENTS_CSV: &str = "logs/tracking_arbitrage_events.csv";

/// 价格追踪周期内并行校验的匹配对数量（固定值，不再通过环境变量调试）。
pub const ARB_TRACK_CONCURRENCY: usize = 12;
