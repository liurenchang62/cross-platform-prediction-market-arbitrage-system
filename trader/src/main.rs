// src/main.rs
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio::time;
use chrono::{DateTime, Local, Utc};

mod trades_cache;
mod market;
mod market_matcher;
mod text_vectorizer;
mod vector_index;
mod arbitrage_detector;
mod clients;
mod category_mapper;
mod validation;
mod tracking;
mod system_params;
mod category_vectorizer;
mod market_filter;
mod desk_events;
mod feishu_push;
mod trade_reconciliation;
mod tracking_log;
mod kalshi_trade;
mod polymarket_trade;
mod fee_model;
mod kalshi_fee_schedule;
mod session_risk;

use std::collections::HashSet;
use std::sync::Mutex;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use crate::category_mapper::CategoryMapper;
use crate::desk_events::{DeskEventsLog, ExecRaw};
use crate::fee_model::{kalshi_series_from_ticker, pm_exponent_for_pm_market, ArbCostParams};
use crate::kalshi_fee_schedule::{fetch_kalshi_fee_schedule, KalshiFeeScheduleState};
use crate::market::Market;
use clients::{KalshiOrderbookFetchOutcome, KalshiClient, PolymarketClient};
use kalshi_trade::{
    get_balance_cents as kalshi_get_balance_cents,
    place_buy_ioc as kalshi_place_buy_ioc, place_sell_ioc as kalshi_place_sell_ioc,
    KalshiIocResult, KalshiTradeConfig,
};
use polymarket_trade::{PolymarketClobTrader, PostOrderResponse};
use polymarket_client_sdk::clob::types::OrderStatusType;
use polymarket_client_sdk::types::Decimal;
use reqwest::Client as HttpClient;
use market_matcher::{MarketMatcher, MarketMatcherConfig};
use arbitrage_detector::{ArbitrageDetector, ArbitrageOpportunity};
use crate::tracking::{oriented_track_id, DeskCycleCtx, MonitorState, TrackedArbitrage};
use crate::system_params::{
    ARB_TRACK_CONCURRENCY, LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP,
    ARB_ROUNDS_STORE_PATH, DESK_EVENTS_JSONL, NAKED_POSITIONS_JSONL, MAX_RETRIES,
    PM_TAKER_FEE_BPS_WHEN_FETCH_FAILS, RESOLUTION_HORIZON_DAYS,
    RETRY_INITIAL_DELAY_MS,
    MAX_SETTLEMENT_DATE_SKEW_DAYS, SIMILARITY_THRESHOLD, STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS,
    arb_probe_caps, kalshi_orderbook_debug_enabled, require_live_orders_allowed,
    set_live_arbitrage_orders_allowed,
    POLYMARKET_SIGNATURE_TYPE_ENV, load_env_file_if_missing,
};
use crate::tracking_log::print_arbitrage_detection_books;
use crate::session_risk::SessionRiskHandle;
use crate::market_filter::{
    filter_markets_by_resolution_horizon, pair_resolves_imminently, pair_settlement_dates_skew_exceeds,
};
use crate::arbitrage_detector::{
    build_pair_orderbook_ladders_result, kalshi_ladder_fail_tag,
    orderbook_best_ask_price, pm_buy_no_uses_yes_token_complement,
    polymarket_clob_token_id_for_buy, total_size_buy_ladder,
    validate_opportunity_from_ladders, PairLadderBuildFail,
};

/// 同一 PM–Kalshi 匹配在验证器给出的腿向上，再增加「两侧同时取反」的另一腿（如 PM YES+KS NO 与 PM NO+KS YES），去重后用于追踪与验证。
fn expand_dual_orientations(
    matches: Vec<(Market, Market, f64, String, String, bool)>,
) -> Vec<(Market, Market, f64, String, String, bool)> {
    let mut out = Vec::new();
    let mut seen: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    for (pm, ks, sim, pms, kss, inv) in matches {
        let mut try_push = |ps: String, ks_: String| {
            let key = (
                pm.market_id.clone(),
                ks.market_id.clone(),
                ps.clone(),
                ks_.clone(),
            );
            if seen.insert(key) {
                out.push((pm.clone(), ks.clone(), sim, ps, ks_, inv));
            }
        };
        try_push(pms.clone(), kss.clone());
        if let (Some(a), Some(b)) = (
            crate::tracking::flip_binary_side(&pms),
            crate::tracking::flip_binary_side(&kss),
        ) {
            try_push(a.to_string(), b.to_string());
        }
    }
    out
}

/// 启动时必须完整输入 `CONFIRM`（全大写），否则退出进程。
fn prompt_confirm_live() -> Result<()> {
    println!("\n⚠️  本进程为实盘交易：将使用真实资金在 Kalshi / Polymarket 下单，并写入 {}", DESK_EVENTS_JSONL);
    println!("   若确认，请完整输入 CONFIRM（全大写）后回车；其他任何输入均视为放弃并退出程序。");

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let eof = || anyhow::anyhow!("标准输入不可用（无法交互确认）");

    print!("确认实盘: ");
    stdout.flush().ok();
    let mut confirm = String::new();
    if stdin.lock().read_line(&mut confirm).context("读取 stdin 失败")? == 0 {
        return Err(eof());
    }
    if confirm.trim() == "CONFIRM" {
        println!("✅ 已确认进入实盘模式。\n");
        trades_cache::init_on_live_session_start().context("创建/清空 logs/trades_cache.txt")?;
        trades_cache::append_session_banner(Utc::now());
        return Ok(());
    }
    println!("已取消：未确认实盘，程序退出。");
    std::process::exit(0);
}

fn format_resolution_expiry(label: &str, dt: Option<DateTime<Utc>>) -> String {
    match dt {
        Some(t) => {
            let days = (t - Utc::now()).num_days();
            let day_hint = if days > 0 {
                format!("距今 {days} 天")
            } else if days < 0 {
                format!("已过期 {} 天", -days)
            } else {
                "今天到期".to_string()
            };
            format!(
                "{} 到期: {} UTC ({})",
                label,
                t.format("%Y-%m-%d %H:%M"),
                day_hint
            )
        }
        None => format!("{} 到期: 未知", label),
    }
}

#[derive(Default)]
struct TrackingArbDiag {
    /// 本周期进入 `validate_arbitrage_pair` 的活跃追踪对次数
    attempts: usize,
    no_pm_clob_token: usize,
    pm_book_missing: usize,
    ks_book_missing: usize,
    ladders_failed: usize,
    ladder_pm_invalid_side: usize,
    ladder_pm_missing_bids: usize,
    ladder_pm_missing_asks: usize,
    ladder_pm_malformed: usize,
    /// PM：该腿 asks/bids 无任何行（空盘口）。
    ladder_pm_liq_empty_side: usize,
    /// PM：有档位行但均在 (0,1)+size>0 下被剔除。
    ladder_pm_liq_all_filtered: usize,
    ladder_ks_no_body: usize,
    ladder_ks_invalid_side: usize,
    ladder_ks_malformed: usize,
    ladder_ks_liq_empty_side: usize,
    ladder_ks_liq_all_filtered: usize,
    /// 已有双边阶梯但严格 detector 未通过
    rejected_strict: usize,
    /// 两侧 `resolution_date` 齐备且结算时刻差严格大于 [`MAX_SETTLEMENT_DATE_SKEW_DAYS`]
    rejected_settlement_skew: usize,
    missing_best_ask: usize,
    /// 两 leg 最优卖价之和 ≥ 1（无「买双 YES 赢一面」结构套利）
    sum_ask_ge_1: usize,
    /// 和 < 1 但严格 min_profit 等仍判否
    sum_ask_lt_1: usize,
    /// 和 < 1 且将 min_profit 放至极低后出现机会（说明主要是净利门槛 / 费损）
    loose_pass_strict_fail: usize,
    /// 和 < 1 放宽后仍无（流动性/缩放/n 为 0 / cost_for_exact 等）
    loose_still_fail: usize,
    accepted: usize,
    /// Kalshi 订单簿：`get_order_book` 等价于 json 缺失时的细分（需 `KALSHI_ORDERBOOK_DEBUG` 才打印样本）
    ks_ob_transport: usize,
    ks_ob_http_429: usize,
    ks_ob_http_4xx: usize,
    ks_ob_http_5xx: usize,
    ks_ob_http_other: usize,
    ks_ob_json_bad: usize,
    /// 每周期至多各 [`DIAG_SAMPLE_CAP`] 条，合并后不超 [`DIAG_MERGED_SAMPLE_MAX`]
    ks_ob_samples: Vec<String>,
    ks_ladder_samples: Vec<String>,
}

/// 单 worker / 单线程追踪时，每类调试样本条数上限
const DIAG_SAMPLE_CAP: usize = 4;
/// 并发合并多段 diag 后，打印时保留的样本总行数上限
const DIAG_MERGED_SAMPLE_MAX: usize = 8;

impl TrackingArbDiag {
    fn push_diag_sample(buf: &mut Vec<String>, line: String, cap: usize) {
        if buf.len() >= cap || buf.iter().any(|x| x == &line) {
            return;
        }
        buf.push(line);
    }

    fn merge_samples(dst: &mut Vec<String>, src: &[String]) {
        for s in src {
            if dst.len() >= DIAG_MERGED_SAMPLE_MAX {
                break;
            }
            if !dst.contains(s) {
                dst.push(s.clone());
            }
        }
    }

    fn record_kalshi_ob_outcome(&mut self, ticker: &str, o: &KalshiOrderbookFetchOutcome) {
        match o {
            KalshiOrderbookFetchOutcome::Ok => {}
            KalshiOrderbookFetchOutcome::Transport(msg) => {
                self.ks_ob_transport += 1;
                Self::push_diag_sample(
                    &mut self.ks_ob_samples,
                    format!("ob {ticker} transport {msg}"),
                    DIAG_SAMPLE_CAP,
                );
            }
            KalshiOrderbookFetchOutcome::Http {
                status,
                body_preview,
            } => {
                match *status {
                    429 => self.ks_ob_http_429 += 1,
                    500..=599 => self.ks_ob_http_5xx += 1,
                    400..=499 => self.ks_ob_http_4xx += 1,
                    _ => self.ks_ob_http_other += 1,
                }
                Self::push_diag_sample(
                    &mut self.ks_ob_samples,
                    format!("ob {ticker} HTTP{} {}", status, body_preview),
                    DIAG_SAMPLE_CAP,
                );
            }
            KalshiOrderbookFetchOutcome::JsonError(msg) => {
                self.ks_ob_json_bad += 1;
                Self::push_diag_sample(
                    &mut self.ks_ob_samples,
                    format!("ob {ticker} json_err {msg}"),
                    DIAG_SAMPLE_CAP,
                );
            }
        }
    }

    fn record_ladder_build_fail(&mut self, r: PairLadderBuildFail, ks_ticker: Option<&str>) {
        self.ladders_failed += 1;
        match r {
            PairLadderBuildFail::PmInvalidBuySide => self.ladder_pm_invalid_side += 1,
            PairLadderBuildFail::PmMissingBidsArray => self.ladder_pm_missing_bids += 1,
            PairLadderBuildFail::PmMissingAsksArray => self.ladder_pm_missing_asks += 1,
            PairLadderBuildFail::PmMalformedQuote => self.ladder_pm_malformed += 1,
            PairLadderBuildFail::PmNoAskLiquidityEmptySide => self.ladder_pm_liq_empty_side += 1,
            PairLadderBuildFail::PmNoAskLiquidityAllRowsFiltered => {
                self.ladder_pm_liq_all_filtered += 1;
            }
            PairLadderBuildFail::KsMissingOrderbookBody => self.ladder_ks_no_body += 1,
            PairLadderBuildFail::KsInvalidBuySide => self.ladder_ks_invalid_side += 1,
            PairLadderBuildFail::KsMalformedQuote => self.ladder_ks_malformed += 1,
            PairLadderBuildFail::KsNoAskLiquidityEmptySide => self.ladder_ks_liq_empty_side += 1,
            PairLadderBuildFail::KsNoAskLiquidityAllRowsFiltered => {
                self.ladder_ks_liq_all_filtered += 1;
            }
        }
        if let Some(t) = ks_ticker {
            if let Some(tag) = kalshi_ladder_fail_tag(r) {
                Self::push_diag_sample(
                    &mut self.ks_ladder_samples,
                    format!("ladder {t} {tag}"),
                    DIAG_SAMPLE_CAP,
                );
            }
        }
    }

    fn merge_from(&mut self, o: &Self) {
        self.attempts += o.attempts;
        self.no_pm_clob_token += o.no_pm_clob_token;
        self.pm_book_missing += o.pm_book_missing;
        self.ks_book_missing += o.ks_book_missing;
        self.ladders_failed += o.ladders_failed;
        self.ladder_pm_invalid_side += o.ladder_pm_invalid_side;
        self.ladder_pm_missing_bids += o.ladder_pm_missing_bids;
        self.ladder_pm_missing_asks += o.ladder_pm_missing_asks;
        self.ladder_pm_malformed += o.ladder_pm_malformed;
        self.ladder_pm_liq_empty_side += o.ladder_pm_liq_empty_side;
        self.ladder_pm_liq_all_filtered += o.ladder_pm_liq_all_filtered;
        self.ladder_ks_no_body += o.ladder_ks_no_body;
        self.ladder_ks_invalid_side += o.ladder_ks_invalid_side;
        self.ladder_ks_malformed += o.ladder_ks_malformed;
        self.ladder_ks_liq_empty_side += o.ladder_ks_liq_empty_side;
        self.ladder_ks_liq_all_filtered += o.ladder_ks_liq_all_filtered;
        self.rejected_strict += o.rejected_strict;
        self.rejected_settlement_skew += o.rejected_settlement_skew;
        self.missing_best_ask += o.missing_best_ask;
        self.sum_ask_ge_1 += o.sum_ask_ge_1;
        self.sum_ask_lt_1 += o.sum_ask_lt_1;
        self.loose_pass_strict_fail += o.loose_pass_strict_fail;
        self.loose_still_fail += o.loose_still_fail;
        self.accepted += o.accepted;
        self.ks_ob_transport += o.ks_ob_transport;
        self.ks_ob_http_429 += o.ks_ob_http_429;
        self.ks_ob_http_4xx += o.ks_ob_http_4xx;
        self.ks_ob_http_5xx += o.ks_ob_http_5xx;
        self.ks_ob_http_other += o.ks_ob_http_other;
        self.ks_ob_json_bad += o.ks_ob_json_bad;
        Self::merge_samples(&mut self.ks_ob_samples, &o.ks_ob_samples);
        Self::merge_samples(&mut self.ks_ladder_samples, &o.ks_ladder_samples);
    }

    /// 每追踪周期一行摘要，便于判断「真没机会」还是「订单簿拉取异常」。
    fn print_compact(&self) {
        println!(
            "   📎 追踪判定摘要: 尝试 {} | 通过 {} | 无PM token {} | PM簿缺 {} KS簿缺 {} | 阶梯失败 {} | 严格拒 {} (其中和≥1: {}) | 结算日差>{}天: {}",
            self.attempts,
            self.accepted,
            self.no_pm_clob_token,
            self.pm_book_missing,
            self.ks_book_missing,
            self.ladders_failed,
            self.rejected_strict,
            self.sum_ask_ge_1,
            MAX_SETTLEMENT_DATE_SKEW_DAYS,
            self.rejected_settlement_skew,
        );
        if kalshi_orderbook_debug_enabled() {
            self.print_kalshi_orderbook_debug();
        }
    }

    /// 环境变量 `KALSHI_ORDERBOOK_DEBUG=1` 时由 [`print_compact`] 调用；只在小样本与汇总计数层面打印。
    fn print_kalshi_orderbook_debug(&self) {
        let ob_sum = self.ks_ob_transport
            + self.ks_ob_http_429
            + self.ks_ob_http_4xx
            + self.ks_ob_http_5xx
            + self.ks_ob_http_other
            + self.ks_ob_json_bad;
        if ob_sum == 0 && self.ks_ladder_samples.is_empty() {
            return;
        }
        if ob_sum > 0 {
            println!(
                "   🔧 Kalshi 簿缺细分: transport={} 429={} 4xx={} 5xx={} http其它={} json坏={}（应与 KS簿缺 {} 合计对照）",
                self.ks_ob_transport,
                self.ks_ob_http_429,
                self.ks_ob_http_4xx,
                self.ks_ob_http_5xx,
                self.ks_ob_http_other,
                self.ks_ob_json_bad,
                self.ks_book_missing
            );
            for line in &self.ks_ob_samples {
                println!("      · {}", line);
            }
        }
        if !self.ks_ladder_samples.is_empty() {
            println!(
                "   🔧 Kalshi 阶梯失败样本 (至多 {} 条):",
                self.ks_ladder_samples.len()
            );
            for line in &self.ks_ladder_samples {
                println!("      · {}", line);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 优先从 env/credentials.env 加载缺失的凭证（系统环境变量已设置则不覆盖）
    load_env_file_if_missing();
    println!("🚀 跨平台套利监控");
    prompt_confirm_live()?;
    set_live_arbitrage_orders_allowed(true);
    println!("📌 实盘交易：成交 JSONL: {DESK_EVENTS_JSONL}；终端摘录: logs/trades_cache.txt");
    println!(
        "🔒 资金安全：无合格套利时主循环**仅**请求行情/订单簿/费率等只读接口；Kalshi IOC / PM FOK / 卖平敞口仅在 `validate_arbitrage_pair_post_success` 内且通过 `require_live_orders_allowed`，不因「空转」自动下单或划帐。"
    );
    println!(
        "🔒 Polymarket：CLOB 鉴权为链下；仅在**程序启动、进入主循环前**经 Relayer 尝试赎回可结算头寸并刷新 Collateral（需 API key）；非此类链上操作仍可能于他处发生。"
    );
    println!("📊 平台: Polymarket ↔ Kalshi");
    if kalshi_orderbook_debug_enabled() {
        println!(
            "🔧 KALSHI_ORDERBOOK_DEBUG：追踪周期结束将打印 Kalshi 簿缺细分（transport/HTTP/json）与阶梯失败样本，各至多 {} 行。",
            DIAG_MERGED_SAMPLE_MAX
        );
    }

    println!("📚 加载类别配置...");
    let category_mapper = CategoryMapper::from_file("config/categories.toml")
        .context("加载类别配置文件失败")?;

    let polymarket = PolymarketClient::new();
    let kalshi = KalshiClient::new();

    println!("🔐 加载 Kalshi / Polymarket 凭证（环境变量）…");
    let kalshi_cfg = KalshiTradeConfig::try_from_env()?;
    let pm_trader = PolymarketClobTrader::connect().await?;
    println!("   ✅ 凭证已就绪（订单簿、fee、成交路径）。秘钥勿用于本程序以外。");
    let live_log = DeskEventsLog::new()?;

    let ks_http = portfolio_monitor::http_client_env::build_http_client(Duration::from_secs(25))
        .context("构建 Kalshi HTTP 客户端")?;

    preflight_live_readonly_trading_apis(&ks_http, &kalshi_cfg, &pm_trader).await?;
    println!("   ✅ 启动预检：两侧余额非零、Kalshi/PM 只读可用、geoblock 允许交易");

    println!("   💰 Polymarket（Proxy）：Relayer 授权(仅本进程这一次) / 待赎回结算 / 刷新 CLOB Collateral…");
    let rpc_pm = portfolio_monitor::pm_relayer_automation::polygon_rpc_for_automation();
    let relay_pm = portfolio_monitor::pm_relayer_automation::relayer_url_for_automation();
    match portfolio_monitor::pm_relayer_automation::desk_hydrate_pm_proxy(
        &ks_http,
        &rpc_pm,
        &relay_pm,
        true,
    )
    .await
    {
        Ok(rep) => {
            if let Some(line) =
                portfolio_monitor::pm_relayer_automation::feishu_pm_redeem_line(&rep, 0, 0)
            {
                let _ = feishu_push::push_trade_summary_to_feishu(&line);
            }
        }
        Err(e) => eprintln!("   ⚠️ PM 启动前赎回/同步未完成: {}", e),
    }

    let snap = crate::session_risk::fetch_live_snapshot(&ks_http, &kalshi_cfg, &pm_trader, &polymarket)
        .await
        .context("会话风控：启动快照（余额+持仓+PM MTM）")?;
    let session_risk_ctrl = crate::session_risk::SessionRiskController::from_startup_snapshot(snap.clone());
    let startup_feishu = crate::session_risk::format_session_startup(&snap, session_risk_ctrl.baselines());
    if let Err(e) = feishu_push::push_trade_summary_to_feishu(&startup_feishu) {
        eprintln!("   ⚠️ 启动体检飞书未送达: {}", e);
    }
    let baselines_startup = session_risk_ctrl.baselines().clone();
    let session_risk: SessionRiskHandle = Arc::new(Mutex::new(session_risk_ctrl));

    let (pl, pp) = arb_probe_caps();
    println!(
        "   ── 本交易周期 · 账户快照（USD）──\n\
         \x20   Kalshi   总资产 ${:.2}  现金 ${:.2}  市场 {} 个  token {} 个  持仓市值 ${:.2}\n\
         \x20   PM       总资产 ${:.2}  现金 ${:.2}  市场 {} 个  token {} 个  持仓市值 ${:.2}\n\
         \x20   持仓市值均为订单簿买卖中间价乘以持仓规模之和\n\
         \x20   「市场」为较上层统计（Kalshi 为有敞口的 event 去重，Polymarket 为 Gamma 市场 id 去重）\n\
         \x20   「token」为较细层（Kalshi 为有敞口的 ticker 合约腿去重，Polymarket 为 CLOB outcome token 条数）\n\
         \x20   Polymarket 条件仓 API 为 micro 计值，须先 ÷1e6 再乘中间价\n\
         \x20   簿不可用或单侧无流动性则该头寸不计入市值\n\
         \x20   参考总资金 R=min(两侧总资产) ${:.2} → 每腿 ${:.4} | 每对 ${:.4}",
        snap.ks_total_usd(),
        snap.ks_cash_usd,
        snap.ks_markets_n,
        snap.ks_tokens_n,
        snap.ks_position_mtm_usd,
        snap.pm_total_usd(),
        snap.pm_collateral_usd,
        snap.pm_conditional_markets_n,
        snap.pm_conditional_tokens_n,
        snap.pm_cond_mtm_usd,
        baselines_startup.reference_capital_usd,
        pl,
        pp,
    );
    println!(
        "   💵 Kalshi 可部署比例 {:.0}% | 最低 ROI 门槛 = 净利/真实成本 >= {:.2}%（与打印 ROI 同语境）",
        crate::system_params::KALSHI_BALANCE_DEPLOYABLE_FRAC * 100.0,
        LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP * 100.0
    );
    println!(
        "   🚀 价格追踪并发 = {}（`system_params::ARB_TRACK_CONCURRENCY`）",
        ARB_TRACK_CONCURRENCY
    );

    let matcher_config = MarketMatcherConfig {
        similarity_threshold: SIMILARITY_THRESHOLD,
        use_date_boost: true,
        use_category_boost: true,
        date_boost_factor: 0.05,
        category_boost_factor: 0.03,
        ..Default::default()
    };

    let mut matcher = MarketMatcher::new(matcher_config, category_mapper);

    let arb_detector = ArbitrageDetector::new(LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP);
    
    // 初始化监控状态，并从磁盘恢复跨会话永久套利轮次计数
    // full_match_interval 参数已废弃（改为挂钟调度），传 0 占位。
    let mut monitor_state = MonitorState::new(0);
    monitor_state.load_rounds_from_disk(ARB_ROUNDS_STORE_PATH);
    
    println!("📡 首次获取市场并构建索引...");
    
    // 首次获取全量市场
    let (kalshi_markets, polymarket_markets) = match fetch_initial_markets(&polymarket, &kalshi).await {
        Ok(markets) => markets,
        Err(e) => {
            eprintln!("❌ 首次获取市场失败: {}", e);
            return Err(e);
        }
    };

   
    
    // 按类别训练向量化器
    matcher.fit_vectorizer(&kalshi_markets, &polymarket_markets)?;
    
    // 构建双索引
    println!("🌲 构建 Kalshi 市场索引...");
    matcher.build_kalshi_index(&kalshi_markets)?;
    
    println!("🌲 构建 Polymarket 市场索引...");
    matcher.build_polymarket_index(&polymarket_markets)?;

    println!("\n🔍 初始化 · 全量匹配（周期 #0，沿用本次拉取与索引，不重复请求、不重训）...");
    let (per_leg_cap, pair_cap) = arb_probe_caps();
    let mut no_track_arb_diag: Option<TrackingArbDiag> = None;
    run_full_match_work(
        &polymarket,
        &kalshi,
        &mut matcher,
        &arb_detector,
        &mut monitor_state,
        &polymarket_markets,
        &kalshi_markets,
        per_leg_cap,
        pair_cap,
        per_leg_cap,
        &live_log,
        &session_risk,
        &ks_http,
        &kalshi_cfg,
        &pm_trader,
        false,
        &mut no_track_arb_diag,
    )
    .await?;
    live_guard_pm_collateral_after_full_match(&pm_trader).await?;

    println!("\n✅ 初始化完成（全量匹配已作为周期 #0 完成）");
    println!("   📊 Kalshi 市场数: {}", kalshi_markets.len());
    println!("   📊 Polymarket 市场数: {}", polymarket_markets.len());
    println!("   📊 Kalshi 索引大小: {}", matcher.kalshi_index_size());
    println!("   📊 Polymarket 索引大小: {}", matcher.polymarket_index_size());
    println!("   📌 监控从会话内周期 #1 起为价格追踪；下一全量重建约 90 分钟后（挂钟计时，与 tick 无关）");
    monitor_state.mark_full_match_done();
    monitor_state.finish_init_advance_scheduling_counters();

    println!("\n🔄 开始监控循环 (间隔: 2秒，全量重匹配每 90 分钟挂钟触发)...\n");
    
    // 主循环
    loop {
        match run_cycle(
            &polymarket,
            &kalshi,
            &mut matcher,
            &arb_detector,
            &mut monitor_state,
            &live_log,
            &session_risk,
            &ks_http,
            &kalshi_cfg,
            &pm_trader,
        )
        .await
        {
            Ok(stats) => {
                println!("📊 周期统计: 新匹配 {} 对, 套利 {} 个, 追踪 {} 对", 
                    stats.new_matches, stats.arbitrage_opportunities, monitor_state.tracked_pairs.len());
            }
            Err(e) => {
                eprintln!("❌ 周期错误: {}", e);
            }
        }
        
        monitor_state.advance_tick_after_iteration();
        time::sleep(Duration::from_secs(2)).await;
    }
}

/// 实盘：全量匹配结束后、进入后续价格追踪前，若 CLOB Collateral 为 0 则立即终止（避免签名类型/缓存导致的误判与空仓继续跑）。
async fn live_guard_pm_collateral_after_full_match(pm_trader: &PolymarketClobTrader) -> Result<()> {
    let bal = pm_trader.fetch_clob_collateral_balance_for_guard().await?;
    if bal == Decimal::ZERO {
        anyhow::bail!(
            "实盘保护：Polymarket CLOB Collateral 余额为 0，程序已停止。请检查 {}（网页钱包常见为 1=Proxy）、入金与 USDC 授权；可设置 PM_CLOB_BALANCE_SYNC=1 后重试。",
            POLYMARKET_SIGNATURE_TYPE_ENV
        );
    }
    println!(
        "   🛡️ 实盘保护：Polymarket CLOB Collateral ≈ ${:.2} USD",
        polymarket_trade::clob_collateral_micro_usdc_to_usd(&bal)
    );
    Ok(())
}

/// 追踪周期内从 Gamma 刷新 PM 快照（与原先 `run_tracking_cycle` 内联逻辑一致）
async fn refresh_pm_market_from_gamma(pm: &PolymarketClient, target: &mut Market) {
    if let Ok(fresh_pm) = pm.fetch_market_snapshot_by_id(&target.market_id).await {
        target.outcome_prices = fresh_pm.outcome_prices.or(target.outcome_prices);
        target.best_ask = fresh_pm.best_ask.or(target.best_ask);
        target.best_bid = fresh_pm.best_bid.or(target.best_bid);
        target.last_trade_price = fresh_pm.last_trade_price.or(target.last_trade_price);
        target.volume_24h = fresh_pm.volume_24h;
        if !fresh_pm.token_ids.is_empty() {
            target.token_ids = fresh_pm.token_ids;
        }
        if target.resolution_date.is_none() {
            target.resolution_date = fresh_pm.resolution_date;
        }
    }
}

/// Kalshi 第一腿成交后第二腿彻底失败（裸仓）时，将关键信息追加写入 `logs/naked_positions.jsonl`。
/// 写失败仅打印警告，不 panic。
fn append_naked_position_log(
    ticker: &str,
    side: &str,
    qty: i64,
    pm_market_id: &str,
    error_detail: &str,
) {
    use std::fs::OpenOptions;
    let ts = Utc::now().to_rfc3339();
    let line = format!(
        "{{\"ts\":\"{ts}\",\"ks_ticker\":\"{ticker}\",\"ks_side\":\"{side}\",\"qty\":{qty},\"pm_market_id\":\"{pm_market_id}\",\"error\":{error_json}}}\n",
        error_json = serde_json::to_string(error_detail).unwrap_or_else(|_| "\"\"".to_string()),
    );
    if let Some(parent) = std::path::Path::new(NAKED_POSITIONS_JSONL).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(NAKED_POSITIONS_JSONL)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!("⚠️  naked_positions.jsonl 写入失败: {e}");
            }
        }
        Err(e) => eprintln!("⚠️  naked_positions.jsonl 打开失败: {e}"),
    }
}

/// Polymarket `fee-rate` 失败时用 [`PM_TAKER_FEE_BPS_WHEN_FETCH_FAILS`] 保守估算（不丢弃机会、不按 0 bps）。
/// 先重试 2 次（间隔 200 ms），仍失败才 fallback，减少因瞬时网络抖动触发保守值。
async fn pm_fee_bps_with_conservative_fallback(
    pm_trader: &PolymarketClobTrader,
    token_id: &str,
    log_ctx: &str,
) -> u32 {
    const RETRIES: u32 = 2;
    const RETRY_DELAY_MS: u64 = 200;
    let mut last_err = String::new();
    for attempt in 0..=RETRIES {
        match pm_trader.get_fee_bps_for_token(token_id).await {
            Ok(b) => return b,
            Err(e) => {
                last_err = format!("{}", e);
                if attempt < RETRIES {
                    tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS)).await;
                }
            }
        }
    }
    eprintln!(
        "   ⚠️ [{}] Polymarket fee_rate 拉取失败（重试{}次），保守使用 {} bps: {}",
        log_ctx, RETRIES, PM_TAKER_FEE_BPS_WHEN_FETCH_FAILS, last_err
    );
    PM_TAKER_FEE_BPS_WHEN_FETCH_FAILS
}

/// 拉双边簿、建阶梯、严格阈值判定；**不含**终端详表 / CSV（实盘见 `validate_arbitrage_pair_post_success`）。  
/// PM 与 Kalshi 订单簿在单对内 `join` 并行，减少一轮 RTT。
///
/// `print_tracking_positive_roi_preview`：仅追踪周期（`price_track`）传 `true`；用净利门槛 0 的探针计算 [`ArbitrageOpportunity::roi_100_percent`]，
/// 若 **> 0** 则**只打印** PM 标题、Kalshi 标题与 ROI，**不改变**是否通过严格校验或是否下单。
async fn validate_arbitrage_core(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    arb_detector: &ArbitrageDetector,
    pm_market: &Market,
    kalshi_market: &Market,
    pm_side: &str,
    kalshi_side: &str,
    needs_inversion: bool,
    capital_usdt: f64,
    pair_cap_usdt: f64,
    ks_fee: &KalshiFeeScheduleState,
    category_mapper: &CategoryMapper,
    pm_trader: &PolymarketClobTrader,
    arb_diag: &mut Option<TrackingArbDiag>,
    print_tracking_positive_roi_preview: bool,
) -> Result<Option<(ArbitrageOpportunity, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
    if let Some(d) = arb_diag.as_mut() {
        d.attempts += 1;
    }

    if pair_settlement_dates_skew_exceeds(
        pm_market,
        kalshi_market,
        MAX_SETTLEMENT_DATE_SKEW_DAYS,
    ) {
        if let Some(d) = arb_diag.as_mut() {
            d.rejected_settlement_skew += 1;
        }
        return Ok(None);
    }

    // 任一侧距结算不足 IMMINENT_RESOLUTION_SKIP_MINS 分钟：价格已反映结算共识，跳过。
    if pair_resolves_imminently(pm_market, kalshi_market, Utc::now()) {
        return Ok(None);
    }

    let pm_buy_no_via_yes = pm_buy_no_uses_yes_token_complement(pm_market, pm_side);
    let Some(tid) = polymarket_clob_token_id_for_buy(pm_market, pm_side) else {
        if let Some(d) = arb_diag.as_mut() {
            d.no_pm_clob_token += 1;
        }
        return Ok(None);
    };
    let (pm_book_res, ks_ob_report) = tokio::join!(
        polymarket.get_order_book(tid),
        kalshi.fetch_order_book_raw(&kalshi_market.market_id),
    );
    let Some(pm_book) = pm_book_res.ok().flatten() else {
        if let Some(d) = arb_diag.as_mut() {
            d.pm_book_missing += 1;
        }
        return Ok(None);
    };
    let Some(ks_book) = ks_ob_report.json.as_ref() else {
        if let Some(d) = arb_diag.as_mut() {
            d.ks_book_missing += 1;
            d.record_kalshi_ob_outcome(&kalshi_market.market_id, &ks_ob_report.outcome);
        }
        return Ok(None);
    };

    let ladders = match build_pair_orderbook_ladders_result(
        &pm_book,
        ks_book,
        pm_side,
        kalshi_side,
        pm_buy_no_via_yes,
    ) {
        Ok(l) => l,
        Err(reason) => {
            if let Some(d) = arb_diag.as_mut() {
                d.record_ladder_build_fail(reason, Some(&kalshi_market.market_id));
            }
            return Ok(None);
        }
    };

    let pm_bps = pm_fee_bps_with_conservative_fallback(pm_trader, tid, "套利识别").await;
    let scan_cost = ArbCostParams {
        ks_taker_fee_multiplier: ks_fee.multiplier_for_ticker(&kalshi_market.market_id),
        pm_fee_bps: pm_bps,
        pm_exponent: pm_exponent_for_pm_market(category_mapper, pm_market),
    };

    if print_tracking_positive_roi_preview {
        let preview_det = ArbitrageDetector::new(0.0);
        if let Some(prev) = validate_opportunity_from_ladders(
            &preview_det,
            &ladders,
            pm_side,
            kalshi_side,
            needs_inversion,
            capital_usdt,
            pair_cap_usdt,
            &scan_cost,
        ) {
            if prev.roi_100_percent > 0.0 {
                crate::trade_out!(
                    "   📈 [追踪·探针ROI>0] PM: {} | Kalshi: {} | ROI {:.2}%",
                    pm_market.title,
                    kalshi_market.title,
                    prev.roi_100_percent
                );
            }
        }
    }

    let Some(opp) = validate_opportunity_from_ladders(
        arb_detector,
        &ladders,
        pm_side,
        kalshi_side,
        needs_inversion,
        capital_usdt,
        pair_cap_usdt,
        &scan_cost,
    ) else {
        if let Some(d) = arb_diag.as_mut() {
            d.rejected_strict += 1;
            let p = orderbook_best_ask_price(&ladders.pm_asks);
            let k = orderbook_best_ask_price(&ladders.ks_asks);
            match (p, k) {
                (Some(pv), Some(kv)) => {
                    if pv + kv >= 1.0 {
                        d.sum_ask_ge_1 += 1;
                    } else {
                        d.sum_ask_lt_1 += 1;
                        let loose_det = ArbitrageDetector::new(0.0);
                        if validate_opportunity_from_ladders(
                            &loose_det,
                            &ladders,
                            pm_side,
                            kalshi_side,
                            needs_inversion,
                            capital_usdt,
                            pair_cap_usdt,
                            &scan_cost,
                        )
                        .is_some()
                        {
                            d.loose_pass_strict_fail += 1;
                        } else {
                            d.loose_still_fail += 1;
                        }
                    }
                }
                _ => d.missing_best_ask += 1,
            }
        }
        return Ok(None);
    };

    if let Some(d) = arb_diag.as_mut() {
        d.accepted += 1;
    }

    // 将识别阶段的 PM fee bps 附到机会对象上，执行阶段直接复用，避免两腿之间额外 HTTP 请求。
    let mut opp = opp;
    opp.pm_fee_bps = pm_bps;

    let pm_expiry = match pm_market.resolution_date {
        Some(d) => Some(d),
        None => polymarket.fetch_resolution_by_market_id(&pm_market.market_id).await,
    };
    let ks_expiry = match kalshi_market.resolution_date {
        Some(d) => Some(d),
        None => kalshi.fetch_resolution_by_ticker(&kalshi_market.market_id).await,
    };

    Ok(Some((opp, pm_expiry, ks_expiry)))
}

/// 实盘启动后、**拉取全量市场之前**：**只读**校验 Kalshi / Polymarket CLOB 余额均非零，并检查 Polymarket geoblock；不下单、不涉及资金划转。
async fn preflight_live_readonly_trading_apis(
    http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
) -> Result<()> {
    println!("   🔎 实盘预检：查询 Kalshi / Polymarket CLOB 余额（拉取市场前）…");
    let ks_cents = kalshi_get_balance_cents(http, kalshi_cfg)
        .await
        .context("实盘预检：Kalshi GET /portfolio/balance 不可访问或鉴权失败")?;
    if ks_cents <= 0 {
        anyhow::bail!(
            "实盘预检：Kalshi 余额为 {} 美分（须 > 0）。请先充值后再启动实盘。",
            ks_cents
        );
    }
    println!(
        "      Kalshi /portfolio/balance = {}¢（${:.2}）",
        ks_cents,
        ks_cents as f64 / 100.0
    );

    let pm_collateral = pm_trader
        .fetch_clob_collateral_balance_for_guard()
        .await
        .context("实盘预检：Polymarket CLOB balance-allowance（Collateral）失败")?;
    if pm_collateral == Decimal::ZERO {
        anyhow::bail!(
            "实盘预检：Polymarket CLOB Collateral 为 0（须 > 0）。请检查 {}（常见网页账户为 1=Proxy）与私钥一致；可设 PM_CLOB_BALANCE_SYNC=1 后重试。",
            POLYMARKET_SIGNATURE_TYPE_ENV
        );
    }
    let pm_u = polymarket_trade::clob_collateral_micro_usdc_to_usd(&pm_collateral);
    println!(
        "      Polymarket CLOB Collateral ≈ ${:.2} USD（micro USDC ÷1e6，raw={}）",
        pm_u, pm_collateral
    );

    let resp = http
        .get("https://polymarket.com/api/geoblock")
        .send()
        .await
        .context("实盘预检：GET https://polymarket.com/api/geoblock 失败")?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "实盘预检：Polymarket geoblock HTTP {}（须可访问以确认地理限制）",
            resp.status().as_u16()
        );
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .context("实盘预检：geoblock 响应非 JSON")?;
    let blocked = v.get("blocked").and_then(|x| x.as_bool()).unwrap_or(true);
    if blocked {
        anyhow::bail!(
            "实盘预检：Polymarket geoblock 禁止从此环境交易（blocked=true），country={:?}",
            v.get("country")
        );
    }
    Ok(())
}

async fn validate_arbitrage_pair_post_success(
    polymarket: &PolymarketClient,
    pm_market: &Market,
    kalshi_market: &Market,
    similarity: f64,
    pm_side: &str,
    kalshi_side: &str,
    needs_inversion: bool,
    capital_usdt: f64,
    desk_ctx: DeskCycleCtx,
    cycle_phase: &str,
    pair_label: &str,
    live_log: &DeskEventsLog,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    kalshi_fee: &KalshiFeeScheduleState,
    per_leg_cap_usd: f64,
    opp: &ArbitrageOpportunity,
    pm_expiry: Option<DateTime<Utc>>,
    ks_expiry: Option<DateTime<Utc>>,
    session_risk: &SessionRiskHandle,
) -> Result<()> {
    require_live_orders_allowed().context("validate_arbitrage_pair_post_success")?;
    let Some(pm_token_id) = polymarket_clob_token_id_for_buy(pm_market, pm_side) else {
        anyhow::bail!("validate_arbitrage_pair_post_success: missing PM clob token");
    };

    crate::trade_out!(
        "\n  ── 实盘执行：识别 ✓ → 两腿并行（Kalshi IOC + PM FAK 限价单同时发出）──"
    );

    let inv = if needs_inversion { " (Y/N颠倒)" } else { "" };
    let pm_slip = if opp.pm_optimal > 0.0 {
        (opp.pm_avg_slipped - opp.pm_optimal) / opp.pm_optimal * 100.0
    } else {
        0.0
    };
    let ks_slip = if opp.kalshi_optimal > 0.0 {
        (opp.kalshi_avg_slipped - opp.kalshi_optimal) / opp.kalshi_optimal * 100.0
    } else {
        0.0
    };

    crate::trade_out!("\n  📌 验证通过 (相似度: {:.3}){}", similarity, inv);
    crate::trade_out!("     PM:      {}", pm_market.title);
    crate::trade_out!("     Kalshi:  {}", kalshi_market.title);
    crate::trade_out!("     📅 {}", format_resolution_expiry("PM", pm_expiry));
    crate::trade_out!("     📅 {}", format_resolution_expiry("Kalshi", ks_expiry));
    crate::trade_out!("     ─────────────────────────────────────────────────────────");
    crate::trade_out!("     📗 PM 订单簿(买{}) Top5:", pm_side);
    for (j, (p, s)) in opp.orderbook_pm_top5.iter().take(5).enumerate() {
        crate::trade_out!("         #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_pm_top5.is_empty() {
        crate::trade_out!("         (无订单簿)");
    }
    crate::trade_out!("     📗 Kalshi 订单簿(买{}) Top5:", kalshi_side);
    for (j, (p, s)) in opp.orderbook_kalshi_top5.iter().take(5).enumerate() {
        crate::trade_out!("         #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_kalshi_top5.is_empty() {
        crate::trade_out!("         (无订单簿)");
    }
    crate::trade_out!("     ─────────────────────────────────────────────────────────");
    crate::trade_out!(
        "     📊 策略: Polymarket 买 {}  +  Kalshi 买 {}",
        pm_side, kalshi_side
    );
    crate::trade_out!("     ─────────────────────────────────────────────────────────");
    crate::trade_out!("     📊 对冲份数 n: {:.4}", opp.contracts);
    crate::trade_out!(
        "     💵 最优 Ask:     PM {:.4}  |  Kalshi {:.4}",
        opp.pm_optimal, opp.kalshi_optimal
    );
    crate::trade_out!(
        "     📉 滑点后均价:   PM {:.4} ({:+.2}%)  |  Kalshi {:.4} ({:+.2}%)",
        opp.pm_avg_slipped, pm_slip, opp.kalshi_avg_slipped, ks_slip
    );
    crate::trade_out!("     ─────────────────────────────────────────────────────────");
    crate::trade_out!(
        "     💰 投入 ${:.2}（每腿探针上限）利润拆解:",
        capital_usdt
    );
    crate::trade_out!("        毛利润(兑付): ${:.2}", opp.contracts);
    crate::trade_out!("        成本:          ${:.2}", opp.capital_used);
    crate::trade_out!(
        "        交易手续费:    Polymarket ${:.2}  |  Kalshi ${:.2}",
        opp.fee_polymarket_usd, opp.fee_kalshi_usd
    );
    crate::trade_out!("        链上执行成本（估）: ${:.4}", opp.gas_assumed_usd);
    crate::trade_out!("        摩擦成本（friction cost）: ${:.4}", opp.surplus_buffer_usd);
    crate::trade_out!("        = 净利润:      ${:.2}", opp.net_profit_100);
    crate::trade_out!("        ROI:           {:.1}%", opp.roi_100_percent);
    crate::trade_out!("     ─────────────────────────────────────────────────────────");

    // —— 并行下单：Kalshi IOC + PM FAK 限价单同时发出 ——
    let n_target = opp.contracts as i64;
    let ks_client_oid = uuid::Uuid::new_v4().to_string();

    crate::trade_out!(
        "   ⚡ [并行下单] Kalshi IOC {} 份 @ {}¢  ‖  PM FAK {} 份 @ ${:.4}/token",
        n_target, opp.ks_limit_cents, n_target, opp.pm_limit_price
    );

    let (ks_result, pm_result) = tokio::join!(
        kalshi_place_buy_ioc(
            ks_http, kalshi_cfg, &kalshi_market.market_id,
            kalshi_side, &opp, &ks_client_oid, per_leg_cap_usd,
        ),
        pm_trader.buy_outcome_token_limit_fak_n(
            pm_token_id, n_target, opp.pm_limit_price,
        )
    );

    let (kalshi_fill, ks_err_s) = match ks_result {
        Ok(r) => { crate::trade_out!("   📗 [Kalshi] order_id={} fill={}/{}", r.order_id, r.fill_count, n_target); (Some(r), String::new()) }
        Err(e) => { eprintln!("   ⚠️ [Kalshi] 下单失败: {}", e); (None, format!("{}", e)) }
    };
    let (pm_fill_opt, pm_err_s) = match pm_result {
        Ok(r) => { crate::trade_out!("   📗 [PM] order_id={} making={} taking={}", r.order_id, r.making_amount, r.taking_amount); (Some(r), String::new()) }
        Err(e) => { eprintln!("   ⚠️ [PM] 下单失败: {}", e); (None, format!("{}", e)) }
    };

    let k1: i64 = kalshi_fill.as_ref().map_or(0, |r| r.fill_count);
    // PM BUY 订单语义：taking_amount = 收到的 token 数；making_amount = 支付的 USDC
    let pm_tokens: f64 = pm_fill_opt.as_ref().map_or(0.0, |r| r.taking_amount.to_string().parse().unwrap_or(0.0));
    let pm_cost_usdc: f64 = pm_fill_opt.as_ref().map_or(0.0, |r| r.making_amount.to_string().parse().unwrap_or(0.0));
    let k2: i64 = pm_tokens.round() as i64;
    let ks_order_id = kalshi_fill.as_ref().map(|r| r.order_id.clone()).unwrap_or_default();

    crate::trade_out!("   📊 成交汇总: Kalshi={}/{} PM≈{}/{} (tokens≈{:.4} cost≈${:.4})", k1, n_target, k2, n_target, pm_tokens, pm_cost_usdc);

    // —— 事后对账 ——
    let mut ks_err = ks_err_s;
    let mut pm_err = pm_err_s;
    let mut ks_unwind_order_ids: Vec<String> = Vec::new();
    let mut pm_post_responses: Vec<PostOrderResponse> = pm_fill_opt.into_iter().collect();
    let pm_filled_total = pm_tokens;
    let pm_cost_usdc_total = pm_cost_usdc;

    let mut status: &str = match (k1, k2) {
        (0, 0) => "NO_FILL",
        (k1, k2) if k1 == k2 => "OK",
        _ => "RECONCILE",
    };
    let log_contracts = k1.min(k2) as f64;

    let excess_ks = (k1 - k2).max(0);
    let excess_pm = (k2 - k1).max(0);

    if excess_ks > 0 {
        let ref_price = opp.kalshi_avg_slipped;
        if excess_ks as f64 * ref_price >= 1.0 {
            crate::trade_out!("   📙 [对账] Kalshi 多 {} 份 (K={} PM={}) → IOC 卖平", excess_ks, k1, k2);
            let recon_oid = uuid::Uuid::new_v4().to_string();
            match kalshi_place_sell_ioc(
                ks_http, kalshi_cfg, &kalshi_market.market_id, kalshi_side,
                excess_ks, STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS, &recon_oid,
            ).await {
                Ok(r) => {
                    crate::trade_out!("   📙 [对账] Kalshi 卖平 order_id={} fill={}/{}", r.order_id, r.fill_count, excess_ks);
                    ks_unwind_order_ids.push(r.order_id.clone());
                    if r.fill_count < excess_ks {
                        let naked = excess_ks - r.fill_count;
                        append_naked_position_log(&kalshi_market.market_id, kalshi_side, naked, &pm_market.market_id, "recon_ks_partial");
                        pm_err = format!("recon_ks_partial naked={}", naked);
                        status = "NAKED_KS";
                    } else {
                        status = "RECONCILE_KS";
                    }
                }
                Err(e) => {
                    eprintln!("   ⚠️ [对账] Kalshi 卖平失败: {}", e);
                    append_naked_position_log(&kalshi_market.market_id, kalshi_side, excess_ks, &pm_market.market_id, &format!("recon_ks_failed: {}", e));
                    pm_err = format!("recon_ks_failed: {}", e);
                    status = "NAKED_KS";
                }
            }
        } else {
            crate::trade_out!("   📙 [对账] Kalshi 多 {} 份，名义 ${:.4} < $1.00，记录裸仓", excess_ks, excess_ks as f64 * ref_price);
            append_naked_position_log(&kalshi_market.market_id, kalshi_side, excess_ks, &pm_market.market_id, "recon_ks_below_min");
            status = "NAKED_KS_SMALL";
        }
    }

    if excess_pm > 0 {
        let ref_price = opp.pm_avg_slipped;
        if excess_pm as f64 * ref_price >= 1.0 {
            crate::trade_out!("   📙 [对账] PM 多 {} 份 (K={} PM={}) → FAK 卖出", excess_pm, k1, k2);
            match pm_trader.sell_outcome_token_fak(pm_token_id, excess_pm).await {
                Ok(r) => {
                    // SELL 语义：making_amount = 卖出 token 数，taking_amount = 收到 USDC
                    let sold: f64 = r.making_amount.to_string().parse().unwrap_or(0.0);
                    let usdc_in: f64 = r.taking_amount.to_string().parse().unwrap_or(0.0);
                    crate::trade_out!("   📙 [对账] PM 卖出 order_id={} tokens_sold≈{:.3} usdc_in≈${:.4}", r.order_id, sold, usdc_in);
                    pm_post_responses.push(r);
                    let sold_int = sold.round() as i64;
                    if sold_int < excess_pm {
                        append_naked_position_log(&pm_market.market_id, pm_side, excess_pm - sold_int, pm_token_id, "recon_pm_partial");
                        status = "NAKED_PM";
                    } else {
                        status = "RECONCILE_PM";
                    }
                }
                Err(e) => {
                    eprintln!("   ⚠️ [对账] PM 卖出失败: {}", e);
                    append_naked_position_log(&pm_market.market_id, pm_side, excess_pm, pm_token_id, &format!("recon_pm_failed: {}", e));
                    pm_err = format!("recon_pm_failed: {}", e);
                    status = "NAKED_PM";
                }
            }
        } else {
            crate::trade_out!("   📙 [对账] PM 多 {} 份，名义 ${:.4} < $1.00，记录裸仓", excess_pm, excess_pm as f64 * ref_price);
            status = "NAKED_PM_SMALL";
        }
    }

    if k1 == 0 && k2 == 0 {
        if !ks_err.is_empty() || !pm_err.is_empty() {
            pm_err = format!("no_fill ks_err=[{}] pm_err=[{}]", ks_err, pm_err);
        }
    }

    let pm_json = serde_json::json!({
        "polymarket_orders": pm_post_responses.iter().map(|p| serde_json::json!({
            "order_id": &p.order_id,
            "success": p.success,
            "status": format!("{:?}", p.status),
            "taking_amount": p.taking_amount.to_string(),
            "making_amount": p.making_amount.to_string(),
            "trade_ids": &p.trade_ids,
        })).collect::<Vec<serde_json::Value>>(),
    })
    .to_string();

    let exec_raw = ExecRaw {
        ks_fill_count:        kalshi_fill.as_ref().map_or(0,   |r| r.fill_count),
        ks_fill_price_cents:  kalshi_fill.as_ref().map_or(0,   |r| r.limit_price_cents),
        pm_fill_tokens:       pm_filled_total,
        pm_cost_usdc:         pm_cost_usdc_total,
        roi_model:            opp.roi_100_percent,
        pm_fee_bps:           opp.pm_fee_bps,
        ks_fee_multiplier:    kalshi_fee.multiplier_for_ticker(&kalshi_market.market_id),
    };
    live_log.append(
        desk_ctx.trading_session_utc_date,
        desk_ctx.session_cycle_id,
        desk_ctx.global_tick,
        cycle_phase,
        pair_label,
        &pm_market.market_id,
        &kalshi_market.market_id,
        pm_token_id,
        pm_side,
        kalshi_side,
        &kalshi_market.title,
        &pm_market.title,
        log_contracts,
        opp.net_profit_100,
        opp.capital_used,
        &ks_order_id,
        &ks_client_oid,
        &pm_json,
        &pm_err,
        &ks_err,
        &ks_unwind_order_ids,
        status,
        pm_post_responses.len(),
        &exec_raw,
    )?;

    trade_reconciliation::print_live_pair_reconciliation(
        ks_http,
        kalshi_cfg,
        pm_trader,
        pm_token_id,
        pair_label,
        status,
        &ks_order_id,
        &ks_unwind_order_ids,
        &pm_post_responses,
    )
    .await;

    if feishu_push::feishu_should_notify(status) && !ks_order_id.is_empty() {
        let body = feishu_push::format_trade_summary(
            pair_label,
            &desk_ctx
                .trading_session_utc_date
                .format("%Y-%m-%d")
                .to_string(),
            desk_ctx.session_cycle_id,
            desk_ctx.global_tick,
            cycle_phase,
            &pm_market.title,
            &kalshi_market.title,
            &pm_market.market_id,
            &kalshi_market.market_id,
            pm_side,
            kalshi_side,
            status,
            &ks_order_id,
            &ks_client_oid,
            log_contracts,
            pm_post_responses.len(),
            ks_unwind_order_ids.len(),
            opp.net_profit_100,
            opp.capital_used,
        );
        if let Err(e) = feishu_push::push_trade_summary_to_feishu(&body) {
            eprintln!("   ⚠️ 飞书推送未成功: {}", e);
        }
    }

    crate::session_risk::refresh_and_notify(
        session_risk,
        ks_http,
        kalshi_cfg,
        pm_trader,
        polymarket,
    )
    .await
    .context("会话风控：成交后复检")?;

    Ok(())
}

/// 验证单个套利对（带滑点检查）；本周期对 PM/Kalshi **各拉一次**订单簿，解析 ask+bid。
async fn validate_arbitrage_pair(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    arb_detector: &ArbitrageDetector,
    pm_market: &Market,
    kalshi_market: &Market,
    similarity: f64,
    pm_side: &str,
    kalshi_side: &str,
    needs_inversion: bool,
    capital_usdt: f64,
    pair_cap_usdt: f64,
    per_leg_cap_usd: f64,
    desk_ctx: DeskCycleCtx,
    cycle_phase: &str,
    pair_label: &str,
    live_log: &DeskEventsLog,
    session_risk: &SessionRiskHandle,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    ks_fee: &KalshiFeeScheduleState,
    category_mapper: &CategoryMapper,
    arb_diag: &mut Option<TrackingArbDiag>,
) -> Result<Option<(ArbitrageOpportunity, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
    let Some((opp, pm_exp, ks_exp)) = validate_arbitrage_core(
        polymarket,
        kalshi,
        arb_detector,
        pm_market,
        kalshi_market,
        pm_side,
        kalshi_side,
        needs_inversion,
        capital_usdt,
        pair_cap_usdt,
        ks_fee,
        category_mapper,
        pm_trader,
        arb_diag,
        cycle_phase == "price_track",
    )
    .await?
    else {
        return Ok(None);
    };
    let paused = session_risk
        .lock()
        .map(|g| g.trading_paused())
        .unwrap_or(false);
    if paused {
        println!("   🔒 会话风控：现金预警未解除，跳过本对实盘下单。");
        print_arbitrage_detection_books(
            &opp,
            &pm_market.title,
            &kalshi_market.title,
            similarity,
            needs_inversion,
            pm_side,
            kalshi_side,
            pm_exp,
            ks_exp,
        );
        return Ok(Some((opp, pm_exp, ks_exp)));
    }
    validate_arbitrage_pair_post_success(
        polymarket,
        pm_market,
        kalshi_market,
        similarity,
        pm_side,
        kalshi_side,
        needs_inversion,
        capital_usdt,
        desk_ctx,
        cycle_phase,
        pair_label,
        live_log,
        ks_http,
        kalshi_cfg,
        pm_trader,
        ks_fee,
        per_leg_cap_usd,
        &opp,
        pm_exp,
        ks_exp,
        session_risk,
    )
    .await?;
    Ok(Some((opp, pm_exp, ks_exp)))
}


async fn run_full_match_work(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    matcher: &mut MarketMatcher,
    arb_detector: &ArbitrageDetector,
    monitor_state: &mut MonitorState,
    polymarket_markets: &[Market],
    kalshi_markets: &[Market],
    trade_amount: f64,
    pair_cap_usdt: f64,
    per_leg_cap_usd: f64,
    live_log: &DeskEventsLog,
    session_risk: &SessionRiskHandle,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    rebuild_index: bool,
    arb_diag: &mut Option<TrackingArbDiag>,
) -> Result<(usize, usize)> {
    if rebuild_index {
        println!("\n   🔄 重建索引...");
        matcher.fit_vectorizer(kalshi_markets, polymarket_markets)?;
        matcher.build_kalshi_index(kalshi_markets)?;
        matcher.build_polymarket_index(polymarket_markets)?;
    }

    monitor_state.last_pm_pool = polymarket_markets.len();
    monitor_state.last_ks_pool = kalshi_markets.len();

    println!("   🔍 匹配市场...");
    let raw_matches = matcher.find_matches_bidirectional(polymarket_markets, kalshi_markets).await;
    let base_pairs = raw_matches.len();
    let matches = expand_dual_orientations(raw_matches);
    println!(
        "      ✅ 基础匹配 {} 对，展开双向腿后 {} 条追踪配置",
        base_pairs,
        matches.len()
    );

    let n_skew_cfg = matches
        .iter()
        .filter(|(pm, ks, _, _, _, _)| {
            pair_settlement_dates_skew_exceeds(pm, ks, MAX_SETTLEMENT_DATE_SKEW_DAYS)
        })
        .count();
    let eligible: Vec<_> = matches
        .into_iter()
        .filter(|(pm, ks, _, _, _, _)| {
            !pair_settlement_dates_skew_exceeds(pm, ks, MAX_SETTLEMENT_DATE_SKEW_DAYS)
        })
        .collect();
    if n_skew_cfg > 0 {
        println!(
            "      结算日差 > {} 天：剔除 {} 条（不入追踪、本周期不校验）",
            MAX_SETTLEMENT_DATE_SKEW_DAYS, n_skew_cfg
        );
    }

    // 通过结算日约束的匹配对加入追踪列表（带方向信息，每条腿独立键）
    let mut all_matches: Vec<(Market, Market, f64, String, String, bool)> = Vec::new();
    for (pm_market, kalshi_market, similarity, pm_side, kalshi_side, needs_inversion) in &eligible {
        all_matches.push((
            pm_market.clone(),
            kalshi_market.clone(),
            *similarity,
            pm_side.clone(),
            kalshi_side.clone(),
            *needs_inversion,
        ));
    }

    let mut series_keys: HashSet<String> = HashSet::new();
    for (_, ks, _, _, _, _) in &eligible {
        series_keys.insert(kalshi_series_from_ticker(&ks.market_id));
    }
    let ks_fee = Arc::new(fetch_kalshi_fee_schedule(ks_http, kalshi_cfg, &series_keys).await?);
    let category_mapper = Arc::new(matcher.category_mapper.clone());
    
    let mut verified_count = 0;
    let conc = ARB_TRACK_CONCURRENCY;
    let desk_ctx = monitor_state.desk_cycle_ctx();

    if conc <= 1 {
        for (pm_market, kalshi_market, similarity, pm_side, kalshi_side, needs_inversion) in &eligible {
            let pair_label = oriented_track_id(
                &pm_market.market_id,
                &kalshi_market.market_id,
                pm_side,
                kalshi_side,
            );
            if validate_arbitrage_pair(
                polymarket,
                kalshi,
                arb_detector,
                pm_market,
                kalshi_market,
                *similarity,
                pm_side,
                kalshi_side,
                *needs_inversion,
                trade_amount,
                pair_cap_usdt,
                per_leg_cap_usd,
                desk_ctx,
                "full_match",
                &pair_label,
                live_log,
                session_risk,
                ks_http,
                kalshi_cfg,
                pm_trader,
                ks_fee.as_ref(),
                category_mapper.as_ref(),
                arb_diag,
            )
            .await?
            .is_some()
            {
                verified_count += 1;
                monitor_state.commit_pair_round(&pair_label, ARB_ROUNDS_STORE_PATH);
            }
        }
    } else {
        println!(
            "      … 全量套利验证并发 {}（{} 条配置，与追踪共用 ARB_TRACK_CONCURRENCY）",
            conc,
            eligible.len()
        );
        type MRow = (Market, Market, f64, String, String, bool);
        let jobs: Vec<MRow> = eligible.iter().cloned().collect();
        let ks_fee_j = ks_fee.clone();
        let cm_j = category_mapper.clone();
        let pm_tc = pm_trader.clone();
        // 流式驱动：与 price_track 一致，检测并发、发现机会立即执行，无需等全批完成。
        let mut det_stream = stream::iter(jobs.into_iter())
            .map(|(pm_market, kalshi_market, similarity, pm_side, kalshi_side, needs_inversion)| {
                let pm = polymarket.clone();
                let ks = kalshi.clone();
                let det = arb_detector.clone();
                let ks_fee = ks_fee_j.clone();
                let cm = cm_j.clone();
                let pm_tr = pm_tc.clone();
                async move {
                    let pair_label = oriented_track_id(
                        &pm_market.market_id,
                        &kalshi_market.market_id,
                        &pm_side,
                        &kalshi_side,
                    );
                    let mut dslot = Some(TrackingArbDiag::default());
                    let core = validate_arbitrage_core(
                        &pm,
                        &ks,
                        &det,
                        &pm_market,
                        &kalshi_market,
                        &pm_side,
                        &kalshi_side,
                        needs_inversion,
                        trade_amount,
                        pair_cap_usdt,
                        ks_fee.as_ref(),
                        cm.as_ref(),
                        &pm_tr,
                        &mut dslot,
                        false,
                    )
                    .await;
                    let frag = dslot.unwrap_or_default();
                    (
                        (
                            pm_market,
                            kalshi_market,
                            similarity,
                            pm_side,
                            kalshi_side,
                            needs_inversion,
                            pair_label,
                        ),
                        core,
                        frag,
                    )
                }
            })
            .buffer_unordered(conc);

        while let Some(((pm_market, kalshi_market, similarity, pm_side, kalshi_side, needs_inversion, pair_label), core_res, frag)) = det_stream.next().await
        {
            if let Some(d) = arb_diag.as_mut() {
                d.merge_from(&frag);
            }
            match core_res {
                Ok(Some((opp, pm_exp, ks_exp))) => {
                    let paused = session_risk
                        .lock()
                        .map(|g| g.trading_paused())
                        .unwrap_or(false);
                    if paused {
                        println!("   🔒 会话风控：现金预警未解除，跳过本对实盘下单。");
                        print_arbitrage_detection_books(
                            &opp,
                            &pm_market.title,
                            &kalshi_market.title,
                            similarity,
                            needs_inversion,
                            &pm_side,
                            &kalshi_side,
                            pm_exp,
                            ks_exp,
                        );
                    } else {
                        validate_arbitrage_pair_post_success(
                            polymarket,
                            &pm_market,
                            &kalshi_market,
                            similarity,
                            &pm_side,
                            &kalshi_side,
                            needs_inversion,
                            trade_amount,
                            desk_ctx,
                            "full_match",
                            &pair_label,
                            live_log,
                            ks_http,
                            kalshi_cfg,
                            pm_trader,
                            ks_fee.as_ref(),
                            per_leg_cap_usd,
                            &opp,
                            pm_exp,
                            ks_exp,
                            session_risk,
                        )
                        .await?;
                        monitor_state.commit_pair_round(&pair_label, ARB_ROUNDS_STORE_PATH);
                    }
                    verified_count += 1;
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
    }

    monitor_state.update_tracked_pairs(all_matches);

    Ok((eligible.len(), verified_count))
}

/// 执行全量匹配周期（重新拉取市场并重训索引后匹配）
async fn run_full_match_cycle(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    matcher: &mut MarketMatcher,
    arb_detector: &ArbitrageDetector,
    monitor_state: &mut MonitorState,
    trade_amount: f64,
    pair_cap_usdt: f64,
    per_leg_cap_usd: f64,
    live_log: &DeskEventsLog,
    session_risk: &SessionRiskHandle,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
) -> Result<(usize, usize)> {
    println!("   📡 执行全量匹配（拉取最新市场并重建索引）...");

    let polymarket_raw: Vec<Market> = polymarket.fetch_all_markets().await?;
    let kalshi_raw: Vec<Market> = kalshi.fetch_all_markets().await?;
    let now = Utc::now();
    let polymarket_markets = filter_markets_by_resolution_horizon(polymarket_raw, now);
    let kalshi_markets = filter_markets_by_resolution_horizon(kalshi_raw, now);

    println!(
        "      Polymarket: {} 个市场 ({}d 窗口内), Kalshi: {} 个市场 ({}d 窗口内)",
        polymarket_markets.len(),
        RESOLUTION_HORIZON_DAYS,
        kalshi_markets.len(),
        RESOLUTION_HORIZON_DAYS
    );

    let mut no_track_arb_diag: Option<TrackingArbDiag> = None;
    run_full_match_work(
        polymarket,
        kalshi,
        matcher,
        arb_detector,
        monitor_state,
        &polymarket_markets,
        &kalshi_markets,
        trade_amount,
        pair_cap_usdt,
        per_leg_cap_usd,
        live_log,
        session_risk,
        ks_http,
        kalshi_cfg,
        pm_trader,
        true,
        &mut no_track_arb_diag,
    )
    .await
}

/// 执行价格追踪周期
async fn run_tracking_cycle(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    arb_detector: &ArbitrageDetector,
    monitor_state: &mut MonitorState,
    trade_amount: f64,
    pair_cap_usdt: f64,
    per_leg_cap_usd: f64,
    live_log: &DeskEventsLog,
    session_risk: &SessionRiskHandle,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    category_mapper: &CategoryMapper,
) -> Result<usize> {
    crate::trade_out!("   📡 执行价格追踪...");
    crate::trade_out!("      追踪 {} 个匹配对", monitor_state.tracked_pairs.len());
    let desk_ctx = monitor_state.desk_cycle_ctx();

    let mut series_keys: HashSet<String> = HashSet::new();
    for p in monitor_state.tracked_pairs.iter().filter(|x| x.active) {
        series_keys.insert(kalshi_series_from_ticker(&p.kalshi_market.market_id));
    }
    let ks_fee = Arc::new(fetch_kalshi_fee_schedule(ks_http, kalshi_cfg, &series_keys).await?);
    let category_mapper = Arc::new(category_mapper.clone());

    let mut opportunity_count = 0;
    let conc = ARB_TRACK_CONCURRENCY;
    let n_active = monitor_state.tracked_pairs.iter().filter(|p| p.active).count();
    let tick = Instant::now();

    let mut diag_total = TrackingArbDiag::default();

    if conc <= 1 || n_active == 0 {
        let mut done_active = 0usize;
        let mut diag_slot: Option<TrackingArbDiag> = Some(TrackingArbDiag::default());
        let mut rounds_batch: Vec<(String, u32)> = Vec::new();
        for pair in monitor_state.tracked_pairs.iter_mut() {
            if !pair.active {
                continue;
            }
            done_active += 1;

            if done_active % 25 == 0 || done_active == n_active {
                println!(
                    "      … 追踪校验进度 {}/{}，已用 {:?}",
                    done_active,
                    n_active,
                    tick.elapsed()
                );
            }

            if let Some((verified, _pm_exp, _ks_exp)) = validate_arbitrage_pair(
                polymarket,
                kalshi,
                arb_detector,
                &pair.pm_market,
                &pair.kalshi_market,
                pair.similarity,
                &pair.pm_side,
                &pair.kalshi_side,
                pair.needs_inversion,
                trade_amount,
                pair_cap_usdt,
                per_leg_cap_usd,
                desk_ctx,
                "price_track",
                &pair.pair_id,
                live_log,
                session_risk,
                ks_http,
                kalshi_cfg,
                pm_trader,
                ks_fee.as_ref(),
                category_mapper.as_ref(),
                &mut diag_slot,
            )
            .await?
            {
                opportunity_count += 1;
                pair.arb_rounds_executed += 1;
                rounds_batch.push((pair.pair_id.clone(), pair.arb_rounds_executed));
                pair.last_check = Utc::now();
                if verified.net_profit_100 > pair.best_profit {
                    pair.best_profit = verified.net_profit_100;
                }
            }
        }
        monitor_state.persist_round_counts_batch(&rounds_batch, ARB_ROUNDS_STORE_PATH);
        diag_total = diag_slot.take().unwrap_or_default();
    } else {
        println!(
            "      … 使用并发 {} 校验 {} 个活跃对（sweep 模式：确认无机会的不重扫，交易后立即继续扫剩余对）",
            conc, n_active
        );

        let ks_fee_j = ks_fee.clone();
        let cm_j = category_mapper.clone();
        let pm_tc = pm_trader.clone();

        // ── sweep 架构 ──────────────────────────────────────────────────────────────
        // scanned：已返回结果（无论 Some/None）的 pair 索引集合。
        // 每轮 sweep 迭代：对未扫描对发起新 buffer_unordered 检测。
        //   • 发现机会 → 原子执行 → break 内循环（取消在途 future）→ 立即以剩余未扫描对开新轮
        //   • 全部无机会 → break 外循环（本次 sweep 完成）
        // 结果：已确认无机会的对在本次 sweep 内不重复检测；交易后剩余对立即继续扫，无等待。
        let mut scanned: HashSet<usize> = HashSet::new();

        'sweep: loop {
            let remaining: Vec<(usize, TrackedArbitrage)> = monitor_state
                .tracked_pairs
                .iter()
                .enumerate()
                .filter(|(i, p)| p.active && !scanned.contains(i))
                .map(|(i, p)| (i, p.clone()))
                .collect();

            if remaining.is_empty() {
                break 'sweep;
            }

            let ks_fee_s = ks_fee_j.clone();
            let cm_s = cm_j.clone();
            let pm_ts = pm_tc.clone();

            let mut det_stream = stream::iter(remaining.into_iter())
                .map(|(idx, pair)| {
                    let pm = polymarket.clone();
                    let ks = kalshi.clone();
                    let det = arb_detector.clone();
                    let kf = ks_fee_s.clone();
                    let cm = cm_s.clone();
                    let pm_tr = pm_ts.clone();
                    async move {
                        let mut dslot = Some(TrackingArbDiag::default());
                        let core = validate_arbitrage_core(
                            &pm,
                            &ks,
                            &det,
                            &pair.pm_market,
                            &pair.kalshi_market,
                            &pair.pm_side,
                            &pair.kalshi_side,
                            pair.needs_inversion,
                            trade_amount,
                            pair_cap_usdt,
                            kf.as_ref(),
                            cm.as_ref(),
                            &pm_tr,
                            &mut dslot,
                            true,
                        )
                        .await;
                        let frag = dslot.unwrap_or_default();
                        (idx, pair, core, frag)
                    }
                })
                .buffer_unordered(conc);

            let mut traded = false;
            let mut traded_pair_id: Option<String> = None;
            let mut traded_rounds: u32 = 0;

            while let Some((idx, pair, core_res, frag)) = det_stream.next().await {
                diag_total.merge_from(&frag);
                scanned.insert(idx); // 已得到结果，本次 sweep 不再重扫
                let tr = &mut monitor_state.tracked_pairs[idx];
                tr.pm_market = pair.pm_market;
                match core_res {
                    Ok(Some((opp, pm_exp, ks_exp))) => {
                        let paused = session_risk
                            .lock()
                            .map(|g| g.trading_paused())
                            .unwrap_or(false);
                        if paused {
                            println!("   🔒 会话风控：现金预警未解除，跳过本对实盘下单。");
                            print_arbitrage_detection_books(
                                &opp,
                                &tr.pm_market.title,
                                &tr.kalshi_market.title,
                                tr.similarity,
                                tr.needs_inversion,
                                &tr.pm_side,
                                &tr.kalshi_side,
                                pm_exp,
                                ks_exp,
                            );
                        } else {
                            validate_arbitrage_pair_post_success(
                                polymarket,
                                &tr.pm_market,
                                &tr.kalshi_market,
                                tr.similarity,
                                &tr.pm_side,
                                &tr.kalshi_side,
                                tr.needs_inversion,
                                trade_amount,
                                desk_ctx,
                                "price_track",
                                &tr.pair_id,
                                live_log,
                                ks_http,
                                kalshi_cfg,
                                pm_trader,
                                ks_fee.as_ref(),
                                per_leg_cap_usd,
                                &opp,
                                pm_exp,
                                ks_exp,
                                session_risk,
                            )
                            .await?;
                            tr.arb_rounds_executed += 1;
                            traded_pair_id = Some(tr.pair_id.clone());
                            traded_rounds = tr.arb_rounds_executed;
                            traded = true;
                        }
                        opportunity_count += 1;
                        tr.last_check = Utc::now();
                        if opp.net_profit_100 > tr.best_profit {
                            tr.best_profit = opp.net_profit_100;
                        }
                        // 发现并处理机会后 break 内循环：
                        //   • det_stream drop → 取消所有在途 future（价格数据作废）
                        //   • 外循环立即用剩余未扫描对（!scanned）重启新一轮检测
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
            }
            drop(det_stream); // 显式取消所有挂起 future

            if let Some(pid) = traded_pair_id {
                monitor_state.persist_round_counts_batch(&[(pid, traded_rounds)], ARB_ROUNDS_STORE_PATH);
            }

            if !traded {
                // 本次 sweep 迭代中所有对均已扫完且无机会 → sweep 结束
                break 'sweep;
            }
            // traded=true：继续 sweep 外循环，立即对剩余未扫对重新检测
        }

        println!("      … 并发追踪校验结束，总用时 {:?}", tick.elapsed());
    }

    diag_total.print_compact();

    Ok(opportunity_count)
}
/// 运行单个监控周期
async fn run_cycle(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    matcher: &mut MarketMatcher,
    arb_detector: &ArbitrageDetector,
    monitor_state: &mut MonitorState,
    live_log: &DeskEventsLog,
    session_risk: &SessionRiskHandle,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
) -> Result<CycleStats> {
    let start_time = Local::now();
    monitor_state.roll_session_if_new_utc_day();
    crate::trade_out!(
        "🔄 新监控迭代 UTC会话日 {} | 会话内周期 #{} | 调度 tick {} — 本地 {}",
        monitor_state.trading_session_utc_date,
        monitor_state.session_cycle_seq,
        monitor_state.global_tick,
        start_time.format("%H:%M:%S")
    );

    let cycle_snap = crate::session_risk::fetch_live_snapshot(
        ks_http,
        kalshi_cfg,
        pm_trader,
        polymarket,
    )
    .await
    .context("本周期账户快照")?;
    crate::trade_out!(
        "   ── 本周期 · 账户（USD）── Kalshi 总 ${:.2} · 现金 ${:.2} │ PM 总 ${:.2} · 现金(Collateral) ${:.2}",
        cycle_snap.ks_total_usd(),
        cycle_snap.ks_cash_usd,
        cycle_snap.pm_total_usd(),
        cycle_snap.pm_collateral_usd,
    );
    session_risk
        .lock()
        .unwrap()
        .apply_snapshot_with_notifications(&cycle_snap);

    monitor_state.prune_tracked_beyond_resolution_horizon(Utc::now());
    monitor_state.prune_tracked_settlement_skew_too_wide();

    let (per_leg_cap, pair_cap) = arb_probe_caps();
    let trade_amount = per_leg_cap;
    let is_full_match_cycle = monitor_state.should_full_match();

    let (new_matches, opportunities) = if is_full_match_cycle {
        let (m, v) = run_full_match_cycle(
            polymarket,
            kalshi,
            matcher,
            arb_detector,
            monitor_state,
            trade_amount,
            pair_cap,
            per_leg_cap,
            live_log,
            session_risk,
            ks_http,
            kalshi_cfg,
            pm_trader,
        )
        .await?;
        live_guard_pm_collateral_after_full_match(pm_trader).await?;
        monitor_state.mark_full_match_done();
        (m, v)
    } else {
        let c = run_tracking_cycle(
            polymarket,
            kalshi,
            arb_detector,
            monitor_state,
            trade_amount,
            pair_cap,
            per_leg_cap,
            live_log,
            session_risk,
            ks_http,
            kalshi_cfg,
            pm_trader,
            &matcher.category_mapper,
        )
        .await?;
        (0, c)
    };

    let elapsed = Local::now() - start_time;
    println!("   ⏱️ 周期完成, 耗时: {}ms", elapsed.num_milliseconds());

    Ok(CycleStats {
        new_matches,
        arbitrage_opportunities: opportunities,
    })
}

/// 首次获取市场
async fn fetch_initial_markets(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
) -> Result<(Vec<Market>, Vec<Market>)> {
    println!("   📡 获取 Polymarket 市场...");
    let polymarket_raw: Vec<Market> = polymarket.fetch_all_markets().await?;
    println!("   📡 获取 Kalshi 市场...");
    let kalshi_raw: Vec<Market> = kalshi.fetch_all_markets().await?;

    let now = Utc::now();
    // 同上：resolution_date 与展示用到期同源（见 system_params::RESOLUTION_HORIZON_DAYS）
    let polymarket_markets = filter_markets_by_resolution_horizon(polymarket_raw, now);
    let kalshi_markets = filter_markets_by_resolution_horizon(kalshi_raw, now);

    println!(
        "      ✅ Polymarket: {} 个 ({}d 窗口), Kalshi: {} 个 ({}d 窗口)",
        polymarket_markets.len(),
        RESOLUTION_HORIZON_DAYS,
        kalshi_markets.len(),
        RESOLUTION_HORIZON_DAYS
    );

    Ok((kalshi_markets, polymarket_markets))
}

/// 周期统计
struct CycleStats {
    new_matches: usize,
    arbitrage_opportunities: usize,
}