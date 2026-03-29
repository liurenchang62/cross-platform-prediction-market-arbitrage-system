//! 实盘套利监控：HTTP/分页、匹配与索引、资金标尺、Kalshi/Polymarket 环境变量名。

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};

// ==================== 仅跟踪 vs 实盘：下单安全锁 ====================
/// 仅当用户在启动时选择**实盘**并输入 `YES` 后为 `true`；**仅跟踪**恒为 `false`。  
/// 任何可能改变资金/仓位的请求入口处必须 [`require_live_orders_allowed`]，防止误调用。
static LIVE_ARBITRAGE_ORDERS_ALLOWED: AtomicBool = AtomicBool::new(false);

/// 在 `main` 解析交互选项后立即调用一次（早于加载秘钥亦无妨，但须与 `live_trading` 一致）。
pub fn set_live_arbitrage_orders_allowed(allowed: bool) {
    LIVE_ARBITRAGE_ORDERS_ALLOWED.store(allowed, Ordering::SeqCst);
}

#[inline]
pub fn live_arbitrage_orders_allowed() -> bool {
    LIVE_ARBITRAGE_ORDERS_ALLOWED.load(Ordering::SeqCst)
}

/// Kalshi IOC、PM FOK、止损卖平/对冲等路径的硬门槛：仅跟踪下直接返回 Err，不发起 HTTP 下单。
pub fn require_live_orders_allowed() -> Result<()> {
    if !live_arbitrage_orders_allowed() {
        bail!(
            "安全锁：当前为仅跟踪模式，禁止执行下单/卖平等任何可能改变资金或仓位的请求；\
             秘钥在本进程中仅用于鉴权 API（如订单簿、fee_changes、fee-rate）。\
             若需真实下单请重启并选择实盘模式。"
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
pub const POLYMARKET_MAX_MARKETS: usize = 40000;

// ==================== Kalshi（行情） ====================
pub const KALSHI_PAGE_LIMIT: u32 = 1000;
pub const KALSHI_MAX_MARKETS: usize = 40000;

/// 生产环境 Trade API 根（无末尾 `/`）。与官方文档一致。
pub const KALSHI_TRADE_API_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";

// ==================== 向量化 / 匹配（与原版一致） ====================
pub const MAX_VOCAB_SIZE: Option<usize> = None;
pub const SIMILARITY_THRESHOLD: f64 = 0.8;
pub const SIMILARITY_TOP_K: usize = 15;
pub const MATCH_MATMUL_CHUNK_ROWS: usize = 512;
pub const FULL_FETCH_INTERVAL: usize = 180;
pub const RESOLUTION_HORIZON_DAYS: i64 = 21;

// ==================== 实盘资金标尺（美元） ====================
/// 策略参考总盘子（用于探针名义上限，与交易所余额无关）。
pub const TOTAL_CAPITAL_USD: f64 = 1_000.0;

/// 单市场对最大名义占参考总盘子比例（两腿合计上限因子）。
pub const PAIR_CAP_FRAC_OF_CAPITAL: f64 = 0.01;

/// 单腿探针名义占参考总盘子比例（PM 或 Kalshi 各一腿）。
pub const PER_LEG_CAP_FRAC_OF_CAPITAL: f64 = 0.005;

/// 缓冲资金比例：仅允许动用 (1 − 此值) × 可用余额 参与 Kalshi 侧收缩（与原先纸面「可用比例」语义一致，此处为 30% 缓冲 → 最多用 70%）。
pub const KALSHI_BALANCE_DEPLOYABLE_FRAC: f64 = 0.70;

// ==================== 止损（Kalshi 先、PM 第二腿失败后） ====================
/// Kalshi IOC **卖出**平仓：最低可接受限价（美分/份）。`1` 表示尽量按可见 bid 撮合（最激进卖平）。
pub const STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS: i64 = 1;

// ==================== 费用模型：非交易摩擦（HTTP 主路径：极小 Gas 绝对值 + 按每对上限比例缓冲） ====================
/// HTTP/CLOB 下单时链上 Gas 近似为 0；仍留极小美元上界作保守杂项。
pub const LIVE_ARBIT_GAS_ASSUMED_USD: f64 = 0.005;
/// 盈余缓冲：按 [`arb_probe_caps`] 第二项「每对名义上限」`pair_cap` 收取 `pair_cap × 本比例`（当前 **0.1%**）。
pub const LIVE_ARBIT_SURPLUS_BUFFER_FRAC_OF_PAIR_CAP: f64 = 0.001;

/// 可接受最小净利润：同锚定 `pair_cap`，`pair_cap × 本比例`（当前 **0.2%**）。  
/// 与缓冲相同参考系，便于随资金标尺缩放。
pub const LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP: f64 = 0.002;

#[inline]
pub fn live_arbit_surplus_buffer_usd(pair_cap_usdt: f64) -> f64 {
    pair_cap_usdt.max(0.0) * LIVE_ARBIT_SURPLUS_BUFFER_FRAC_OF_PAIR_CAP
}

#[inline]
pub fn live_arbit_min_net_profit_usd(pair_cap_usdt: f64) -> f64 {
    pair_cap_usdt.max(0.0) * LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP
}

#[inline]
pub fn arb_probe_caps() -> (f64, f64) {
    let r = TOTAL_CAPITAL_USD;
    (
        r * PER_LEG_CAP_FRAC_OF_CAPITAL,
        r * PAIR_CAP_FRAC_OF_CAPITAL,
    )
}

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

// ==================== 实盘成交日志 ====================
pub const LIVE_TRADES_CSV: &str = "logs/live_trades.csv";

/// 价格追踪周期内并行校验的匹配对数量（固定值，不再通过环境变量调试）。
pub const ARB_TRACK_CONCURRENCY: usize = 12;
