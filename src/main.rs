// src/main.rs
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio::time;
use chrono::{DateTime, Utc, Local};

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
mod live_trades;
mod kalshi_trade;
mod polymarket_trade;
mod fee_model;
mod kalshi_fee_schedule;

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use crate::category_mapper::CategoryMapper;
use crate::fee_model::{kalshi_series_from_ticker, pm_exponent_for_pm_market, ArbCostParams};
use crate::kalshi_fee_schedule::{fetch_kalshi_fee_schedule, KalshiFeeScheduleState};
use crate::live_trades::LiveTradesLog;
use crate::market::Market;
use clients::{PolymarketClient, KalshiClient};
use kalshi_trade::{
    place_buy_ioc as kalshi_place_buy_ioc, place_sell_ioc as kalshi_place_sell_ioc,
    KalshiIocResult, KalshiTradeConfig,
};
use polymarket_trade::PolymarketClobTrader;
use reqwest::Client as HttpClient;
use market_matcher::{MarketMatcher, MarketMatcherConfig};
use arbitrage_detector::{ArbitrageDetector, ArbitrageOpportunity};
use crate::tracking::{oriented_track_id, MonitorState, TrackedArbitrage};
use crate::system_params::{
    ARB_TRACK_CONCURRENCY, FULL_FETCH_INTERVAL, LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP,
    LIVE_TRADES_CSV, MAX_RETRIES,
    RETRY_INITIAL_DELAY_MS,
    SIMILARITY_THRESHOLD, STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS, TOTAL_CAPITAL_USD,
    arb_probe_caps, require_live_orders_allowed, set_live_arbitrage_orders_allowed,
};
use crate::market_filter::filter_markets_by_resolution_horizon;
use crate::arbitrage_detector::{
    build_pair_orderbook_ladders_result, cost_for_exact_contracts, orderbook_best_ask_price,
    pm_buy_no_uses_yes_token_complement, polymarket_clob_token_id_for_buy,
    try_parse_kalshi_sell_bids, validate_opportunity_from_ladders, PairLadderBuildFail,
};

/// 第二腿（Polymarket）：在已成功拉到订单簿的前提下，**业务上**最多尝试次数（含首次），失败可重算净利后再试；与网络层重试无关。
const LIVE_SECOND_LEG_BUSINESS_ATTEMPTS: usize = 3;

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

/// 启动时在终端选择：实盘需二次输入 `YES` 确认；否则仅跟踪（无 stdin 则报错）。
fn prompt_run_mode() -> Result<bool> {
    println!("\n请选择运行模式：");
    println!(
        "  [1] 实盘交易 (live_trading) — 真实下单，并写入 {}",
        LIVE_TRADES_CSV
    );
    println!("  [2] 仅跟踪 (tracking_only) — 只校验机会，不下单、不写 CSV");

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let eof = || anyhow::anyhow!("标准输入不可用（无法交互选择模式）");

    loop {
        print!("请输入 1 或 2 后回车: ");
        stdout.flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).context("读取 stdin 失败")? == 0 {
            return Err(eof());
        }
        match line.trim() {
            "1" => {
                println!("\n⚠️  实盘将使用真实资金在 Kalshi / Polymarket 下单，并写入 {}。", LIVE_TRADES_CSV);
                println!("   若确认，请完整输入 YES（三个大写字母）后回车；其他任何输入均视为放弃并退出程序。");
                print!("确认实盘: ");
                stdout.flush().ok();
                let mut confirm = String::new();
                if stdin.lock().read_line(&mut confirm).context("读取 stdin 失败")? == 0 {
                    return Err(eof());
                }
                if confirm.trim() == "YES" {
                    println!("✅ 已确认进入实盘模式。\n");
                    return Ok(true);
                }
                println!("已取消：未确认实盘，程序退出。");
                std::process::exit(0);
            }
            "2" => {
                println!("✅ 已选择仅跟踪模式（不下单、不写 CSV）。\n");
                return Ok(false);
            }
            _ => println!("无效输入，请只输入 1 或 2。"),
        }
    }
}

/// Top10 / 追踪共用的套利行（含双方解析日）
type OpportunityRow = (
    ArbitrageOpportunity,
    String,
    String,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

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
}

impl TrackingArbDiag {
    fn record_ladder_build_fail(&mut self, r: PairLadderBuildFail) {
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
        self.missing_best_ask += o.missing_best_ask;
        self.sum_ask_ge_1 += o.sum_ask_ge_1;
        self.sum_ask_lt_1 += o.sum_ask_lt_1;
        self.loose_pass_strict_fail += o.loose_pass_strict_fail;
        self.loose_still_fail += o.loose_still_fail;
        self.accepted += o.accepted;
    }

    /// 每追踪周期一行摘要，便于判断「真没机会」还是「订单簿拉取异常」。
    fn print_compact(&self) {
        println!(
            "   📎 追踪判定摘要: 尝试 {} | 通过 {} | 无PM token {} | PM簿缺 {} KS簿缺 {} | 阶梯失败 {} | 严格拒 {} (其中和≥1: {})",
            self.attempts,
            self.accepted,
            self.no_pm_clob_token,
            self.pm_book_missing,
            self.ks_book_missing,
            self.ladders_failed,
            self.rejected_strict,
            self.sum_ask_ge_1
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 跨平台套利监控");
    let live_trading = prompt_run_mode()?;
    set_live_arbitrage_orders_allowed(live_trading);
    if live_trading {
        println!("📌 当前：实盘交易 (live_trading)，日志文件 {LIVE_TRADES_CSV}");
    } else {
        println!("📌 当前：仅跟踪 (tracking_only)");
        println!(
            "🔒 已锁定：本进程禁止一切下单/平仓（`require_live_orders_allowed`）；秘钥仅用于只读/鉴权 API。"
        );
    }
    println!("📊 平台: Polymarket ↔ Kalshi");

    println!("📚 加载类别配置...");
    let category_mapper = CategoryMapper::from_file("config/categories.toml")
        .context("加载类别配置文件失败")?;

    let polymarket = PolymarketClient::new();
    let kalshi = KalshiClient::new();

    println!("🔐 加载 Kalshi / Polymarket 凭证（环境变量）…");
    let kalshi_cfg = KalshiTradeConfig::try_from_env()?;
    let pm_trader = PolymarketClobTrader::connect().await?;
    if live_trading {
        println!(
            "   ✅ 凭证已就绪。用途：调用交易所 API（订单簿、Kalshi fee_changes、PM fee-rate、后续若有机会则下单等）。"
        );
        println!("   ⚠️ 实盘：仅在校验通过的路径中发起 IOC/FOK；秘钥仍不应用于本程序以外的操作。");
    } else {
        println!(
            "   ✅ 凭证已就绪。用途：**仅限**本程序内的 API 调用（与公开行情相同的「查询」类：订单簿、费率、签名 GET 等），用于还原实盘可用的成本与价差。"
        );
        println!(
            "   🔒 再次确认：仅跟踪模式下**不会**调用下单接口；若 `require_live_orders_allowed` 被绕过会直接报错退出，不发送改单请求。"
        );
    }
    let live_log: Option<LiveTradesLog> = if live_trading {
        Some(LiveTradesLog::new()?)
    } else {
        None
    };

    let ks_http = HttpClient::builder()
        .timeout(Duration::from_secs(25))
        .build()
        .context("构建 Kalshi HTTP 客户端")?;

    let (pl, pp) = arb_probe_caps();
    println!(
        "   💵 参考总资金 ${} | 每腿探针 ${:.2} | 每对名义上限 ${:.2}（Kalshi 余额另按 {:.0}% 可动用）",
        TOTAL_CAPITAL_USD as i32,
        pl,
        pp,
        crate::system_params::KALSHI_BALANCE_DEPLOYABLE_FRAC * 100.0
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
    
    // 初始化监控状态
    let mut monitor_state = MonitorState::new(FULL_FETCH_INTERVAL);
    
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
    let top10_caption_init = format!(
        "live · 参考资金 ${} · 每腿 ${:.2} · 每对 ${:.2}",
        TOTAL_CAPITAL_USD as i32,
        per_leg_cap,
        pair_cap
    );
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
        live_log.as_ref(),
        live_trading,
        &ks_http,
        &kalshi_cfg,
        &pm_trader,
        &top10_caption_init,
        false,
        &mut no_track_arb_diag,
    )
    .await?;

    println!("\n✅ 初始化完成（全量匹配已作为周期 #0 完成）");
    println!("   📊 Kalshi 市场数: {}", kalshi_markets.len());
    println!("   📊 Polymarket 市场数: {}", polymarket_markets.len());
    println!("   📊 Kalshi 索引大小: {}", matcher.kalshi_index_size());
    println!("   📊 Polymarket 索引大小: {}", matcher.polymarket_index_size());
    println!(
        "   📌 监控从周期 #1 起为价格追踪；下一全量重建约在周期 #{}",
        FULL_FETCH_INTERVAL
    );
    monitor_state.next_cycle();

    println!("\n🔄 开始监控循环 (间隔: 30秒)...\n");
    
    // 主循环
    loop {
        match run_cycle(
            &polymarket,
            &kalshi,
            &mut matcher,
            &arb_detector,
            &mut monitor_state,
            live_log.as_ref(),
            live_trading,
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
        
        monitor_state.next_cycle();
        println!("⏳ 等待下一周期...\n");
        time::sleep(Duration::from_secs(30)).await;
    }
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

/// 拉双边簿、建阶梯、严格阈值判定；**不含**终端详表 / CSV（实盘见 `validate_arbitrage_pair_post_success`）。  
/// PM 与 Kalshi 订单簿在单对内 `join` 并行，减少一轮 RTT。
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
) -> Result<Option<(ArbitrageOpportunity, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
    if let Some(d) = arb_diag.as_mut() {
        d.attempts += 1;
    }

    let pm_buy_no_via_yes = pm_buy_no_uses_yes_token_complement(pm_market, pm_side);
    let Some(tid) = polymarket_clob_token_id_for_buy(pm_market, pm_side) else {
        if let Some(d) = arb_diag.as_mut() {
            d.no_pm_clob_token += 1;
        }
        return Ok(None);
    };
    let (pm_book_res, ks_book_res) = tokio::join!(
        polymarket.get_order_book(tid),
        kalshi.get_order_book(&kalshi_market.market_id),
    );
    let Some(pm_book) = pm_book_res.ok().flatten() else {
        if let Some(d) = arb_diag.as_mut() {
            d.pm_book_missing += 1;
        }
        return Ok(None);
    };
    let Some(ks_book) = ks_book_res.ok().flatten() else {
        if let Some(d) = arb_diag.as_mut() {
            d.ks_book_missing += 1;
        }
        return Ok(None);
    };

    let ladders = match build_pair_orderbook_ladders_result(
        &pm_book,
        &ks_book,
        pm_side,
        kalshi_side,
        pm_buy_no_via_yes,
    ) {
        Ok(l) => l,
        Err(reason) => {
            if let Some(d) = arb_diag.as_mut() {
                d.record_ladder_build_fail(reason);
            }
            return Ok(None);
        }
    };

    let pm_bps = match pm_trader.get_fee_bps_for_token(tid).await {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    let scan_cost = ArbCostParams {
        ks_taker_fee_multiplier: ks_fee.multiplier_for_ticker(&kalshi_market.market_id),
        pm_fee_bps: pm_bps,
        pm_exponent: pm_exponent_for_pm_market(category_mapper, pm_market),
    };

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

/// 第二腿（PM）在业务重试后仍失败，且 Kalshi 第一腿已成交：在 **放宽 PM 对冲** 与 **Kalshi IOC 卖平** 间择**预估净利更高**者执行；主路径失败时尝试另一条。
async fn try_stop_loss_min_loss(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    arb_detector: &ArbitrageDetector,
    pm_market: &Market,
    kalshi_market: &Market,
    pm_side: &str,
    kalshi_side: &str,
    pm_token_id: &str,
    ks_res: &KalshiIocResult,
    leg_fee: &ArbCostParams,
    ks_fill_price_dollars: f64,
    per_leg_cap_usd: f64,
    pair_cap_usdt: f64,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    pm_json: &mut String,
    pm_err: &mut String,
    log_contracts: &mut f64,
) -> Result<Option<&'static str>> {
    require_live_orders_allowed().context("try_stop_loss_min_loss")?;
    let n1 = ks_res.fill_count.max(1);
    let kalshi_cost = ks_res.fill_cost_usd_upper();
    let pm_buy_no_via_yes = pm_buy_no_uses_yes_token_complement(pm_market, pm_side);

    let (pm_book_res, ks_book_res) = tokio::join!(
        polymarket.get_order_book(pm_token_id),
        kalshi.get_order_book(&kalshi_market.market_id),
    );
    let Some(pm_book) = pm_book_res.ok().flatten() else {
        eprintln!("   ⚠️ [止损] 拉取 Polymarket 订单簿失败，跳过损失比较");
        return Ok(None);
    };
    let Some(ks_book) = ks_book_res.ok().flatten() else {
        eprintln!("   ⚠️ [止损] 拉取 Kalshi 订单簿失败，跳过损失比较");
        return Ok(None);
    };
    let ladders = match build_pair_orderbook_ladders_result(
        &pm_book,
        &ks_book,
        pm_side,
        kalshi_side,
        pm_buy_no_via_yes,
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("   ⚠️ [止损] 构建阶梯失败 {:?}，跳过损失比较", e);
            return Ok(None);
        }
    };

    let hedge_best = arb_detector.best_pm_hedge_net_after_kalshi(
        n1,
        &ladders.pm_asks,
        kalshi_cost,
        leg_fee,
        ks_fill_price_dollars,
        pair_cap_usdt,
    );
    let unwind_net_est = match try_parse_kalshi_sell_bids(&ks_book, kalshi_side) {
        Ok(bids) => arb_detector.estimate_kalshi_sell_unwind_net(
            n1,
            &bids,
            kalshi_cost,
            leg_fee,
            pair_cap_usdt,
        ),
        Err(e) => {
            eprintln!("   ⚠️ [止损] 解析 Kalshi 卖盘失败 {:?}，无法估算卖平净利", e);
            None
        }
    };

    let prefer_hedge = match (hedge_best, unwind_net_est) {
        (Some((_, hn)), Some(un)) => hn >= un,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => {
            eprintln!("   ⚠️ [止损] 无可用放宽对冲亦无卖盘估算，放弃自动处置");
            return Ok(None);
        }
    };

    println!(
        "   📛 [止损] 对比路径（预估净利，美元；卖平含单腿 Gas、对冲含两腿 Gas，略偏于卖平）：PM放宽={:?} | Kalshi卖平≈{:?} → 先执行{}",
        hedge_best.map(|(n, net)| format!("n={} net≈{:.6}", n, net)),
        unwind_net_est.map(|u| format!("{:.6}", u)),
        if prefer_hedge { "放宽PM对冲" } else { "Kalshi卖平" }
    );

    if prefer_hedge {
        if let Some((n2, _)) = hedge_best {
            if let Some((_, pm_avg)) = cost_for_exact_contracts(&ladders.pm_asks, n2 as f64) {
                match pm_trader
                    .buy_outcome_token_fok_n(pm_token_id, n2 as f64, pm_avg, per_leg_cap_usd)
                    .await
                {
                    Ok(j) => {
                        *pm_json = j;
                        pm_err.clear();
                        *log_contracts = n2 as f64;
                        return Ok(Some("STOP_HEDGE_OK"));
                    }
                    Err(e) => eprintln!("   ⚠️ [止损] 放宽 PM 对冲失败: {}，改试 Kalshi 卖平", e),
                }
            } else {
                eprintln!("   ⚠️ [止损] 放宽 PM 阶梯不足以支撑 n2={}，改试 Kalshi 卖平", n2);
            }
        }
        let oid = uuid::Uuid::new_v4().to_string();
        match kalshi_place_sell_ioc(
            ks_http,
            kalshi_cfg,
            &kalshi_market.market_id,
            kalshi_side,
            n1,
            STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS,
            &oid,
        )
        .await
        {
            Ok(res) => {
                println!(
                    "   📛 [止损] Kalshi IOC 卖平 order_id={} fill_count={}/{}",
                    res.order_id, res.fill_count, n1
                );
                *pm_err = format!(
                    "stop_unwind_kalshi order_id={} fill_count={}",
                    res.order_id, res.fill_count
                );
                *log_contracts = res.fill_count as f64;
                return Ok(Some("STOP_UNWIND_OK"));
            }
            Err(e) => eprintln!("   ⚠️ [止损] Kalshi 卖平失败: {}", e),
        }
    } else {
        let oid = uuid::Uuid::new_v4().to_string();
        match kalshi_place_sell_ioc(
            ks_http,
            kalshi_cfg,
            &kalshi_market.market_id,
            kalshi_side,
            n1,
            STOP_LOSS_KALSHI_SELL_MIN_PRICE_CENTS,
            &oid,
        )
        .await
        {
            Ok(res) => {
                println!(
                    "   📛 [止损] Kalshi IOC 卖平 order_id={} fill_count={}/{}",
                    res.order_id, res.fill_count, n1
                );
                *pm_err = format!(
                    "stop_unwind_kalshi order_id={} fill_count={}",
                    res.order_id, res.fill_count
                );
                *log_contracts = res.fill_count as f64;
                return Ok(Some("STOP_UNWIND_OK"));
            }
            Err(e) => eprintln!("   ⚠️ [止损] Kalshi 卖平失败: {}，改试放宽 PM 对冲", e),
        }
        if let Some((n2, _)) = hedge_best {
            if let Some((_, pm_avg)) = cost_for_exact_contracts(&ladders.pm_asks, n2 as f64) {
                match pm_trader
                    .buy_outcome_token_fok_n(pm_token_id, n2 as f64, pm_avg, per_leg_cap_usd)
                    .await
                {
                    Ok(j) => {
                        *pm_json = j;
                        pm_err.clear();
                        *log_contracts = n2 as f64;
                        return Ok(Some("STOP_HEDGE_OK"));
                    }
                    Err(e) => eprintln!("   ⚠️ [止损] 放宽 PM 对冲失败: {}", e),
                }
            }
        }
    }

    Ok(Some("STOP_LOSS_FAIL"))
}

async fn validate_arbitrage_pair_post_success(
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
    cycle_id: usize,
    cycle_phase: &str,
    pair_label: &str,
    live_log: &LiveTradesLog,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    kalshi_fee: &KalshiFeeScheduleState,
    category_mapper: &CategoryMapper,
    per_leg_cap_usd: f64,
    pair_cap_usdt: f64,
    opp: &ArbitrageOpportunity,
    pm_expiry: Option<DateTime<Utc>>,
    ks_expiry: Option<DateTime<Utc>>,
) -> Result<()> {
    require_live_orders_allowed().context("validate_arbitrage_pair_post_success")?;
    let Some(pm_token_id) = polymarket_clob_token_id_for_buy(pm_market, pm_side) else {
        anyhow::bail!("validate_arbitrage_pair_post_success: missing PM clob token");
    };

    println!("\n  ── 实盘执行：识别 ✓ → 第一腿 Kalshi → 第二腿 Polymarket（每次取簿重算净利，第二腿业务最多 {LIVE_SECOND_LEG_BUSINESS_ATTEMPTS} 次）──");

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

    println!("\n  📌 验证通过 (相似度: {:.3}){}", similarity, inv);
    println!("     PM:      {}", pm_market.title);
    println!("     Kalshi:  {}", kalshi_market.title);
    println!("     📅 {}", format_resolution_expiry("PM", pm_expiry));
    println!("     📅 {}", format_resolution_expiry("Kalshi", ks_expiry));
    println!("     ─────────────────────────────────────────────────────────");
    println!("     📗 PM 订单簿(买{}) Top5:", pm_side);
    for (j, (p, s)) in opp.orderbook_pm_top5.iter().take(5).enumerate() {
        println!("         #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_pm_top5.is_empty() {
        println!("         (无订单簿)");
    }
    println!("     📗 Kalshi 订单簿(买{}) Top5:", kalshi_side);
    for (j, (p, s)) in opp.orderbook_kalshi_top5.iter().take(5).enumerate() {
        println!("         #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_kalshi_top5.is_empty() {
        println!("         (无订单簿)");
    }
    println!("     ─────────────────────────────────────────────────────────");
    println!(
        "     📊 策略: Polymarket 买 {}  +  Kalshi 买 {}",
        pm_side, kalshi_side
    );
    println!("     ─────────────────────────────────────────────────────────");
    println!("     📊 对冲份数 n: {:.4}", opp.contracts);
    println!(
        "     💵 最优 Ask:     PM {:.4}  |  Kalshi {:.4}",
        opp.pm_optimal, opp.kalshi_optimal
    );
    println!(
        "     📉 滑点后均价:   PM {:.4} ({:+.2}%)  |  Kalshi {:.4} ({:+.2}%)",
        opp.pm_avg_slipped, pm_slip, opp.kalshi_avg_slipped, ks_slip
    );
    println!("     ─────────────────────────────────────────────────────────");
    println!(
        "     💰 投入 ${:.2}（每腿探针上限）利润拆解:",
        capital_usdt
    );
    println!("        毛利润(兑付): ${:.2}", opp.contracts);
    println!("        成本:          ${:.2}", opp.capital_used);
    println!(
        "        交易手续费:    Polymarket ${:.2}  |  Kalshi ${:.2}",
        opp.fee_polymarket_usd, opp.fee_kalshi_usd
    );
    println!("        链上执行成本（估）: ${:.4}", opp.gas_assumed_usd);
    println!("        摩擦成本（friction cost）: ${:.4}", opp.surplus_buffer_usd);
    println!("        = 净利润:      ${:.2}", opp.net_profit_100);
    println!("        ROI:           {:.1}%", opp.roi_100_percent);
    println!("     ─────────────────────────────────────────────────────────");

    // —— 第一腿：Kalshi IOC（网络层仍用 MAX_RETRIES）——
    let mut ks_order_id = String::new();
    let mut ks_client_oid = String::new();
    let mut ks_err = String::new();
    let mut kalshi_fill: Option<KalshiIocResult> = None;
    for attempt in 0..MAX_RETRIES {
        ks_client_oid = uuid::Uuid::new_v4().to_string();
        match kalshi_place_buy_ioc(
            ks_http,
            kalshi_cfg,
            &kalshi_market.market_id,
            kalshi_side,
            opp,
            &ks_client_oid,
            per_leg_cap_usd,
        )
        .await
        {
            Ok(res) => {
                ks_order_id = res.order_id.clone();
                println!(
                    "   📗 [第一腿 Kalshi] IOC 已成交 order_id={} ticker={} fill_count={}/{}",
                    ks_order_id,
                    kalshi_market.market_id,
                    res.fill_count,
                    res.requested_count
                );
                kalshi_fill = Some(res);
                ks_err.clear();
                break;
            }
            Err(e) => {
                ks_err = format!("{}", e);
                eprintln!(
                    "   ⚠️ [Kalshi] 下单失败 (网络/接口重试 {}/{}): {}",
                    attempt + 1,
                    MAX_RETRIES,
                    e
                );
                if attempt + 1 < MAX_RETRIES {
                    let wait_ms = RETRY_INITIAL_DELAY_MS.saturating_mul(attempt as u64 + 1);
                    time::sleep(Duration::from_millis(wait_ms)).await;
                }
            }
        }
    }

    let mut pm_json = String::new();
    let mut pm_err = String::new();
    let mut log_contracts = opp.contracts;
    let mut status: &str = "INIT";

    if !ks_err.is_empty() {
        pm_err = "skipped_pm_kalshi_failed".to_string();
        status = "KS_FAIL";
        } else if let Some(ref ks_res) = kalshi_fill {
        let n1 = ks_res.fill_count;
        let kalshi_cost = ks_res.fill_cost_usd_upper();
        let ks_fill_px = ks_res.limit_price_cents as f64 / 100.0;
        log_contracts = n1 as f64;

        pm_trader.invalidate_clob_caches();
        let leg_pm_bps = pm_trader
            .get_fee_bps_for_token(pm_token_id)
            .await
            .unwrap_or(0);
        let leg_fee = ArbCostParams {
            ks_taker_fee_multiplier: kalshi_fee.multiplier_for_ticker(&kalshi_market.market_id),
            pm_fee_bps: leg_pm_bps,
            pm_exponent: pm_exponent_for_pm_market(category_mapper, pm_market),
        };

        let pm_buy_no_via_yes = pm_buy_no_uses_yes_token_complement(pm_market, pm_side);
        let mut pm_placed = false;
        let mut pm_fail_precheck_only = true;

        for biz_attempt in 0..LIVE_SECOND_LEG_BUSINESS_ATTEMPTS {
            let (pm_book_res, ks_book_res) = tokio::join!(
                polymarket.get_order_book(pm_token_id),
                kalshi.get_order_book(&kalshi_market.market_id),
            );
            let Some(pm_book) = pm_book_res.ok().flatten() else {
                pm_fail_precheck_only = false;
                eprintln!(
                    "   ⚠️ [第二腿] 拉取 Polymarket 订单簿失败 (业务尝试 {}/{})",
                    biz_attempt + 1,
                    LIVE_SECOND_LEG_BUSINESS_ATTEMPTS
                );
                if biz_attempt + 1 < LIVE_SECOND_LEG_BUSINESS_ATTEMPTS {
                    let wait_ms = RETRY_INITIAL_DELAY_MS.saturating_mul(biz_attempt as u64 + 1);
                    time::sleep(Duration::from_millis(wait_ms)).await;
                }
                continue;
            };
            let Some(ks_book) = ks_book_res.ok().flatten() else {
                pm_fail_precheck_only = false;
                eprintln!(
                    "   ⚠️ [第二腿] 拉取 Kalshi 订单簿失败 (业务尝试 {}/{})",
                    biz_attempt + 1,
                    LIVE_SECOND_LEG_BUSINESS_ATTEMPTS
                );
                if biz_attempt + 1 < LIVE_SECOND_LEG_BUSINESS_ATTEMPTS {
                    let wait_ms = RETRY_INITIAL_DELAY_MS.saturating_mul(biz_attempt as u64 + 1);
                    time::sleep(Duration::from_millis(wait_ms)).await;
                }
                continue;
            };

            let ladders = match build_pair_orderbook_ladders_result(
                &pm_book,
                &ks_book,
                pm_side,
                kalshi_side,
                pm_buy_no_via_yes,
            ) {
                Ok(l) => l,
                Err(e) => {
                    pm_fail_precheck_only = false;
                    eprintln!(
                        "   ⚠️ [第二腿] 解析订单簿阶梯失败 {:?} (业务尝试 {}/{})",
                        e,
                        biz_attempt + 1,
                        LIVE_SECOND_LEG_BUSINESS_ATTEMPTS
                    );
                    if biz_attempt + 1 < LIVE_SECOND_LEG_BUSINESS_ATTEMPTS {
                        let wait_ms = RETRY_INITIAL_DELAY_MS.saturating_mul(biz_attempt as u64 + 1);
                        time::sleep(Duration::from_millis(wait_ms)).await;
                    }
                    continue;
                }
            };

            let Some(n2) = arb_detector.pick_pm_hedge_n_after_kalshi(
                n1,
                &ladders.pm_asks,
                kalshi_cost,
                &leg_fee,
                ks_fill_px,
                pair_cap_usdt,
            ) else {
                pm_err = "pm_precheck_no_profitable_n".to_string();
                eprintln!(
                    "   ⚠️ [第二腿] 当前簿上无满足净利阈值(>{:.6})的规模 n≤n1={} (业务尝试 {}/{})",
                    arb_detector.min_profit_usd_for_pair_cap(pair_cap_usdt),
                    n1,
                    biz_attempt + 1,
                    LIVE_SECOND_LEG_BUSINESS_ATTEMPTS
                );
                if biz_attempt + 1 < LIVE_SECOND_LEG_BUSINESS_ATTEMPTS {
                    let wait_ms = RETRY_INITIAL_DELAY_MS.saturating_mul(biz_attempt as u64 + 1);
                    time::sleep(Duration::from_millis(wait_ms)).await;
                }
                continue;
            };

            let Some((_, pm_avg)) = cost_for_exact_contracts(&ladders.pm_asks, n2 as f64) else {
                pm_fail_precheck_only = false;
                pm_err = "pm_ladder_insufficient_for_n".to_string();
                continue;
            };

            if n2 < n1 {
                println!(
                    "   ⚠️ [第二腿] 次优 n2={} < Kalshi 已成交 n1={}，剩余 {} 份未对冲（平仓未实现）",
                    n2,
                    n1,
                    n1 - n2
                );
            }

            let est_net = arb_detector
                .net_profit_pm_hedge_n(
                    n2 as f64,
                    &ladders.pm_asks,
                    n1 as f64,
                    kalshi_cost,
                    &leg_fee,
                    ks_fill_px,
                    pair_cap_usdt,
                )
                .unwrap_or(f64::NAN);
            println!(
                "   📘 [第二腿 Polymarket] 业务尝试 {}/{}: n2={} pm_avg≈{:.6} 预估净利≈{:.6}",
                biz_attempt + 1,
                LIVE_SECOND_LEG_BUSINESS_ATTEMPTS,
                n2,
                pm_avg,
                est_net
            );

            log_contracts = n2 as f64;
            match pm_trader
                .buy_outcome_token_fok_n(&pm_token_id, n2 as f64, pm_avg, per_leg_cap_usd)
                .await
            {
                Ok(j) => {
                    pm_json = j;
                    pm_err.clear();
                    pm_placed = true;
                    status = if n2 < n1 {
                        "PARTIAL_PM_OK"
                    } else {
                        "OK"
                    };
                    println!("   📗 [第二腿 Polymarket] FOK 已提交");
                    break;
                }
                Err(e) => {
                    pm_fail_precheck_only = false;
                    pm_err = format!("{}", e);
                    eprintln!(
                        "   ⚠️ [Polymarket] 下单失败 (业务尝试 {}/{}): {}",
                        biz_attempt + 1,
                        LIVE_SECOND_LEG_BUSINESS_ATTEMPTS,
                        e
                    );
                    if biz_attempt + 1 < LIVE_SECOND_LEG_BUSINESS_ATTEMPTS {
                        let wait_ms = RETRY_INITIAL_DELAY_MS.saturating_mul(biz_attempt as u64 + 1);
                        time::sleep(Duration::from_millis(wait_ms)).await;
                    }
                }
            }
        }

        if !pm_placed && ks_err.is_empty() {
            let prelim: &'static str = if pm_fail_precheck_only && pm_err == "pm_precheck_no_profitable_n"
            {
                "PM_SKIP_UNPROFITABLE"
            } else {
                "PM_FAIL"
            };
            status = prelim;
            if let Some(st) = try_stop_loss_min_loss(
                polymarket,
                kalshi,
                arb_detector,
                pm_market,
                kalshi_market,
                pm_side,
                kalshi_side,
                pm_token_id,
                ks_res,
                &leg_fee,
                ks_fill_px,
                per_leg_cap_usd,
                pair_cap_usdt,
                ks_http,
                kalshi_cfg,
                pm_trader,
                &mut pm_json,
                &mut pm_err,
                &mut log_contracts,
            )
            .await?
            {
                status = st;
            }
        }
    }

    live_log.append(
        cycle_id,
        cycle_phase,
        pair_label,
        &pm_market.market_id,
        &kalshi_market.market_id,
        pm_side,
        kalshi_side,
        log_contracts,
        opp.net_profit_100,
        opp.capital_used,
        &ks_order_id,
        &ks_client_oid,
        &pm_json,
        &pm_err,
        &ks_err,
        status,
    )?;

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
    cycle_id: usize,
    cycle_phase: &str,
    pair_label: &str,
    live_log: Option<&LiveTradesLog>,
    live_trading: bool,
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
    )
    .await?
    else {
        return Ok(None);
    };
    if live_trading {
        let log = live_log.context("实盘模式需已创建 LiveTradesLog")?;
        validate_arbitrage_pair_post_success(
            polymarket,
            kalshi,
            arb_detector,
            pm_market,
            kalshi_market,
            similarity,
            pm_side,
            kalshi_side,
            needs_inversion,
            capital_usdt,
            cycle_id,
            cycle_phase,
            pair_label,
            log,
            ks_http,
            kalshi_cfg,
            pm_trader,
            ks_fee,
            category_mapper,
            per_leg_cap_usd,
            pair_cap_usdt,
            &opp,
            pm_exp,
            ks_exp,
        )
        .await?;
    }
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
    live_log: Option<&LiveTradesLog>,
    live_trading: bool,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    top10_mode_caption: &str,
    rebuild_index: bool,
    arb_diag: &mut Option<TrackingArbDiag>,
) -> Result<(usize, usize, String)> {
    if rebuild_index {
        println!("\n   🔄 重建索引...");
        matcher.fit_vectorizer(kalshi_markets, polymarket_markets)?;
        matcher.build_kalshi_index(kalshi_markets)?;
        matcher.build_polymarket_index(polymarket_markets)?;
    }

    println!("   🔍 匹配市场...");
    let raw_matches = matcher.find_matches_bidirectional(polymarket_markets, kalshi_markets).await;
    let base_pairs = raw_matches.len();
    let matches = expand_dual_orientations(raw_matches);
    println!(
        "      ✅ 基础匹配 {} 对，展开双向腿后 {} 条追踪配置",
        base_pairs,
        matches.len()
    );
    
    // 所有匹配对都加入追踪列表（带方向信息，每条腿独立键）
    let mut all_matches: Vec<(Market, Market, f64, String, String, bool)> = Vec::new();
    for (pm_market, kalshi_market, similarity, pm_side, kalshi_side, needs_inversion) in &matches {
        all_matches.push((
            pm_market.clone(), 
            kalshi_market.clone(), 
            *similarity,
            pm_side.clone(),
            kalshi_side.clone(),
            *needs_inversion
        ));
    }

    let mut series_keys: HashSet<String> = HashSet::new();
    for (_, ks, _, _, _, _) in &matches {
        series_keys.insert(kalshi_series_from_ticker(&ks.market_id));
    }
    let ks_fee = Arc::new(fetch_kalshi_fee_schedule(ks_http, kalshi_cfg, &series_keys).await?);
    let category_mapper = Arc::new(matcher.category_mapper.clone());
    
    // 验证每个匹配对，统计有套利机会的，并收集用于 Top 10 统计
    let mut verified_count = 0;
    let mut opportunities: Vec<OpportunityRow> = Vec::new();
    let conc = ARB_TRACK_CONCURRENCY;
    let full_cycle_id = monitor_state.current_cycle;

    if conc <= 1 {
        for (pm_market, kalshi_market, similarity, pm_side, kalshi_side, needs_inversion) in &matches {
            let pair_label = oriented_track_id(
                &pm_market.market_id,
                &kalshi_market.market_id,
                pm_side,
                kalshi_side,
            );
            if let Some((verified, pm_exp, ks_exp)) = validate_arbitrage_pair(
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
                full_cycle_id,
                "full_match",
                &pair_label,
                live_log,
                live_trading,
                ks_http,
                kalshi_cfg,
                pm_trader,
                ks_fee.as_ref(),
                category_mapper.as_ref(),
                arb_diag,
            )
            .await?
            {
                verified_count += 1;
                opportunities.push((
                    verified.clone(),
                    pm_market.title.clone(),
                    kalshi_market.title.clone(),
                    pm_exp,
                    ks_exp,
                ));
            }
        }
    } else {
        println!(
            "      … 全量套利验证并发 {}（{} 条配置，与追踪共用 ARB_TRACK_CONCURRENCY）",
            conc,
            matches.len()
        );
        type MRow = (Market, Market, f64, String, String, bool);
        let jobs: Vec<MRow> = matches.clone();
        let ks_fee_j = ks_fee.clone();
        let cm_j = category_mapper.clone();
        let pm_tc = pm_trader.clone();
        let results = stream::iter(jobs.into_iter())
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
            .buffer_unordered(conc)
            .collect::<Vec<_>>()
            .await;

        for ((pm_market, kalshi_market, similarity, pm_side, kalshi_side, needs_inversion, pair_label), core_res, frag) in results
        {
            if let Some(d) = arb_diag.as_mut() {
                d.merge_from(&frag);
            }
            match core_res {
                Ok(Some((opp, pm_exp, ks_exp))) => {
                    if live_trading {
                        let log = live_log.context("实盘模式需已创建 LiveTradesLog")?;
                        validate_arbitrage_pair_post_success(
                            polymarket,
                            kalshi,
                            arb_detector,
                            &pm_market,
                            &kalshi_market,
                            similarity,
                            &pm_side,
                            &kalshi_side,
                            needs_inversion,
                            trade_amount,
                            full_cycle_id,
                            "full_match",
                            &pair_label,
                            log,
                            ks_http,
                            kalshi_cfg,
                            pm_trader,
                            ks_fee.as_ref(),
                            category_mapper.as_ref(),
                            per_leg_cap_usd,
                            pair_cap_usdt,
                            &opp,
                            pm_exp,
                            ks_exp,
                        )
                        .await?;
                    }
                    verified_count += 1;
                    opportunities.push((
                        opp.clone(),
                        pm_market.title.clone(),
                        kalshi_market.title.clone(),
                        pm_exp,
                        ks_exp,
                    ));
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
    }

    // 全量匹配周期结束：输出 Top 10 利润（与文件报告同源字符串）
    let top10_block = format_top10_opportunities(&opportunities, top10_mode_caption);
    print!("{}", top10_block);
    // 更新追踪列表（所有匹配对，带方向信息）
    monitor_state.update_tracked_pairs(all_matches);
    
    Ok((matches.len(), verified_count, top10_block))
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
    live_log: Option<&LiveTradesLog>,
    live_trading: bool,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    top10_mode_caption: &str,
) -> Result<(usize, usize, String)> {
    println!("   📡 执行全量匹配（拉取最新市场并重建索引）...");

    let polymarket_raw: Vec<Market> = polymarket.fetch_all_markets().await?;
    let kalshi_raw: Vec<Market> = kalshi.fetch_all_markets().await?;
    let now = Utc::now();
    let polymarket_markets = filter_markets_by_resolution_horizon(polymarket_raw, now);
    let kalshi_markets = filter_markets_by_resolution_horizon(kalshi_raw, now);

    println!(
        "      Polymarket: {} 个市场 (21d 窗口内), Kalshi: {} 个市场 (21d 窗口内)",
        polymarket_markets.len(),
        kalshi_markets.len()
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
        live_trading,
        ks_http,
        kalshi_cfg,
        pm_trader,
        top10_mode_caption,
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
    live_log: Option<&LiveTradesLog>,
    live_trading: bool,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    category_mapper: &CategoryMapper,
    top10_mode_caption: &str,
) -> Result<(usize, String)> {
    println!("   📡 执行价格追踪...");
    println!("      追踪 {} 个匹配对", monitor_state.tracked_pairs.len());

    let mut series_keys: HashSet<String> = HashSet::new();
    for p in monitor_state.tracked_pairs.iter().filter(|x| x.active) {
        series_keys.insert(kalshi_series_from_ticker(&p.kalshi_market.market_id));
    }
    let ks_fee = Arc::new(fetch_kalshi_fee_schedule(ks_http, kalshi_cfg, &series_keys).await?);
    let category_mapper = Arc::new(category_mapper.clone());

    let mut opportunity_count = 0;
    let mut opportunities: Vec<OpportunityRow> = Vec::new();
    let conc = ARB_TRACK_CONCURRENCY;
    let n_active = monitor_state.tracked_pairs.iter().filter(|p| p.active).count();
    let tick = Instant::now();

    let mut diag_total = TrackingArbDiag::default();

    if conc <= 1 || n_active == 0 {
        let mut done_active = 0usize;
        let mut diag_slot: Option<TrackingArbDiag> = Some(TrackingArbDiag::default());
        for pair in monitor_state.tracked_pairs.iter_mut() {
            if !pair.active {
                continue;
            }
            done_active += 1;
            refresh_pm_market_from_gamma(polymarket, &mut pair.pm_market).await;

            if done_active % 25 == 0 || done_active == n_active {
                println!(
                    "      … 追踪校验进度 {}/{}，已用 {:?}",
                    done_active,
                    n_active,
                    tick.elapsed()
                );
            }

            if let Some((verified, pm_exp, ks_exp)) = validate_arbitrage_pair(
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
                monitor_state.current_cycle,
                "price_track",
                &pair.pair_id,
                live_log,
                live_trading,
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
                pair.last_check = Utc::now();
                if verified.net_profit_100 > pair.best_profit {
                    pair.best_profit = verified.net_profit_100;
                }
                opportunities.push((
                    verified.clone(),
                    pair.pm_market.title.clone(),
                    pair.kalshi_market.title.clone(),
                    pm_exp,
                    ks_exp,
                ));
            }
        }
        diag_total = diag_slot.take().unwrap_or_default();
    } else {
        println!(
            "      … 使用并发 {} 校验 {} 个活跃对（Gamma 仍每对一次，PM/KS 簿并行拉取）",
            conc, n_active
        );
        let jobs: Vec<(usize, TrackedArbitrage)> = monitor_state
            .tracked_pairs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.active)
            .map(|(i, p)| (i, p.clone()))
            .collect();

        let ks_fee_j = ks_fee.clone();
        let cm_j = category_mapper.clone();
        let pm_tc = pm_trader.clone();

        let results: Vec<(
            usize,
            TrackedArbitrage,
            Result<Option<(ArbitrageOpportunity, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>>,
            TrackingArbDiag,
        )> = stream::iter(jobs.into_iter())
            .map(|(idx, mut pair)| {
                let pm = polymarket.clone();
                let ks = kalshi.clone();
                let det = arb_detector.clone();
                let ks_fee = ks_fee_j.clone();
                let cm = cm_j.clone();
                let pm_tr = pm_tc.clone();
                async move {
                    refresh_pm_market_from_gamma(&pm, &mut pair.pm_market).await;
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
                        ks_fee.as_ref(),
                        cm.as_ref(),
                        &pm_tr,
                        &mut dslot,
                    )
                    .await;
                    let frag = dslot.unwrap_or_default();
                    (idx, pair, core, frag)
                }
            })
            .buffer_unordered(conc)
            .collect()
            .await;

        for (idx, pair, core_res, frag) in results {
            diag_total.merge_from(&frag);
            let tr = &mut monitor_state.tracked_pairs[idx];
            tr.pm_market = pair.pm_market;
            match core_res {
                Ok(Some((opp, pm_exp, ks_exp))) => {
                    if live_trading {
                        let log = live_log.context("实盘模式需已创建 LiveTradesLog")?;
                        validate_arbitrage_pair_post_success(
                            polymarket,
                            kalshi,
                            arb_detector,
                            &tr.pm_market,
                            &tr.kalshi_market,
                            tr.similarity,
                            &tr.pm_side,
                            &tr.kalshi_side,
                            tr.needs_inversion,
                            trade_amount,
                            monitor_state.current_cycle,
                            "price_track",
                            &tr.pair_id,
                            log,
                            ks_http,
                            kalshi_cfg,
                            pm_trader,
                            ks_fee.as_ref(),
                            category_mapper.as_ref(),
                            per_leg_cap_usd,
                            pair_cap_usdt,
                            &opp,
                            pm_exp,
                            ks_exp,
                        )
                        .await?;
                    }
                    opportunity_count += 1;
                    tr.last_check = Utc::now();
                    if opp.net_profit_100 > tr.best_profit {
                        tr.best_profit = opp.net_profit_100;
                    }
                    opportunities.push((
                        opp.clone(),
                        tr.pm_market.title.clone(),
                        tr.kalshi_market.title.clone(),
                        pm_exp,
                        ks_exp,
                    ));
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
        println!("      … 并发追踪校验结束，总用时 {:?}", tick.elapsed());
    }

    diag_total.print_compact();

    let top10_block = format_top10_opportunities(&opportunities, top10_mode_caption);
    print!("{}", top10_block);

    Ok((opportunity_count, top10_block))
}
/// 运行单个监控周期
async fn run_cycle(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    matcher: &mut MarketMatcher,
    arb_detector: &ArbitrageDetector,
    monitor_state: &mut MonitorState,
    live_log: Option<&LiveTradesLog>,
    live_trading: bool,
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
) -> Result<CycleStats> {
    let start_time = Local::now();
    println!("🔄 开始新周期 #{} - {}", monitor_state.current_cycle, start_time.format("%H:%M:%S"));

    monitor_state.prune_tracked_beyond_resolution_horizon(Utc::now());

    let (per_leg_cap, pair_cap) = arb_probe_caps();
    let trade_amount = per_leg_cap;
    let top10_caption = format!(
        "live · 参考资金 ${} · 每腿 ${:.2} · 每对 ${:.2}",
        TOTAL_CAPITAL_USD as i32,
        per_leg_cap,
        pair_cap
    );
    let is_full_match_cycle = monitor_state.should_full_match();

    // Top10 仅在终端打印；CSV 仅追加套利行。
    let (new_matches, opportunities, _top10_block) = if is_full_match_cycle {
        let (m, v, top10) = run_full_match_cycle(
            polymarket,
            kalshi,
            matcher,
            arb_detector,
            monitor_state,
            trade_amount,
            pair_cap,
            per_leg_cap,
            live_log,
            live_trading,
            ks_http,
            kalshi_cfg,
            pm_trader,
            &top10_caption,
        )
        .await?;
        (m, v, top10)
    } else {
        let (c, top10) = run_tracking_cycle(
            polymarket,
            kalshi,
            arb_detector,
            monitor_state,
            trade_amount,
            pair_cap,
            per_leg_cap,
            live_log,
            live_trading,
            ks_http,
            kalshi_cfg,
            pm_trader,
            &matcher.category_mapper,
            &top10_caption,
        )
        .await?;
        (0, c, top10)
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
        "      ✅ Polymarket: {} 个 (21d 窗口), Kalshi: {} 个 (21d 窗口)",
        polymarket_markets.len(),
        kalshi_markets.len()
    );

    Ok((kalshi_markets, polymarket_markets))
}

/// 周期统计
struct CycleStats {
    new_matches: usize,
    arbitrage_opportunities: usize,
}

/// 本周期利润 Top 10 文本（终端用；不含订单簿，簿档在每次 `validate_arbitrage_pair` 中已打印）
fn format_top10_opportunities(opportunities: &[OpportunityRow], mode_caption: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    writeln!(out).unwrap();
    if opportunities.is_empty() {
        writeln!(out, "🏆 本周期利润 Top 10: 无套利机会").unwrap();
        return out;
    }
    let mut sorted: Vec<_> = opportunities.iter().collect();
    sorted.sort_by(|a, b| {
        b.0.net_profit_100
            .partial_cmp(&a.0.net_profit_100)
            .unwrap_or(Ordering::Equal)
    });
    writeln!(
        out,
        "🏆 本周期利润 Top 10（{}，手续费美元计；Gas/缓冲为固定美元，括号内为占该行「成本」比例）",
        mode_caption
    )
    .unwrap();
    writeln!(
        out,
        "──────────────────────────────────────────────────────────────────────"
    )
    .unwrap();
    for (i, (opp, pm_title, kalshi_title, pm_dt, ks_dt)) in sorted.iter().take(10).enumerate() {
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
        writeln!(
            out,
            "\n   ┌─ #{} 净利润 ${:.2}  ROI {:.1}% ─────────────────────────────────",
            i + 1,
            opp.net_profit_100,
            opp.roi_100_percent
        )
        .unwrap();
        writeln!(out, "   │  PM:      {}", pm_title).unwrap();
        writeln!(out, "   │  Kalshi:  {}", kalshi_title).unwrap();
        writeln!(out, "   │  📅 {}", format_resolution_expiry("PM", *pm_dt)).unwrap();
        writeln!(out, "   │  📅 {}", format_resolution_expiry("Kalshi", *ks_dt)).unwrap();
        writeln!(out, "   │  ─────────────────────────────────────────────────────────────").unwrap();
        let inv = if opp.strategy.contains("颠倒") {
            " (Y/N颠倒)"
        } else {
            ""
        };
        writeln!(
            out,
            "   │  📊 策略: Polymarket 买 {}  +  Kalshi 买 {}{}",
            opp.polymarket_action.1, opp.kalshi_action.1, inv
        )
        .unwrap();
        writeln!(out, "   │  📊 对冲份数 n: {:.4}", opp.contracts).unwrap();
        writeln!(
            out,
            "   │  💵 最优Ask: PM {:.4}  Kalshi {:.4}  →  滑点后: PM {:.4}  Kalshi {:.4}",
            opp.pm_optimal,
            opp.kalshi_optimal,
            opp.pm_avg_slipped,
            opp.kalshi_avg_slipped
        )
        .unwrap();
        writeln!(
            out,
            "   │  📉 滑点%: PM {:+.2}%  |  Kalshi {:+.2}%",
            pm_slip, ks_slip
        )
        .unwrap();
        writeln!(
            out,
            "   │  💰 成本 ${:.2}  →  净利 ${:.2}",
            opp.capital_used, opp.net_profit_100
        )
        .unwrap();
        writeln!(
            out,
            "   │     交易手续费: Polymarket ${:.2}  |  Kalshi ${:.2}",
            opp.fee_polymarket_usd, opp.fee_kalshi_usd
        )
        .unwrap();
        writeln!(
            out,
            "   │     链上执行成本（估）: ${:.4}",
            opp.gas_assumed_usd
        )
        .unwrap();
        writeln!(
            out,
            "   │     摩擦成本（friction cost）: ${:.4}",
            opp.surplus_buffer_usd
        )
        .unwrap();
        writeln!(out, "   │  ROI: {:.1}%", opp.roi_100_percent).unwrap();
        writeln!(out, "   └────────────────────────────────────────────────────────────────").unwrap();
    }
    writeln!(out).unwrap();
    out
}