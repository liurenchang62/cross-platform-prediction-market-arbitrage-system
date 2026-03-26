// src/main.rs
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::time;
use chrono::{DateTime, Utc, Local};

mod market;
mod market_matcher;
mod text_vectorizer;
mod vector_index;
mod arbitrage_detector;
mod monitor_logger;
mod clients;
mod category_mapper;
// 在文件开头的 mod 声明中添加
mod validation;
mod unclassified_logger;
mod tracking;
mod system_params;
mod category_vectorizer;  // 新增
mod cycle_statistics;
mod market_filter;
mod paper_trading;
mod kalshi_demo;

use crate::category_mapper::CategoryMapper;
use crate::unclassified_logger::UnclassifiedLogger;
use crate::market::Market;
use clients::{PolymarketClient, KalshiClient};
use kalshi_demo::KalshiDemoConfig;
use reqwest::Client as HttpClient;
use market_matcher::{MarketMatcher, MarketMatcherConfig};
use arbitrage_detector::{ArbitrageDetector, ArbitrageOpportunity};
use monitor_logger::MonitorLogger;
use crate::tracking::{oriented_track_id, MonitorState};
use crate::system_params::{
    DEMO_REFERENCE_BUDGET_USD, FULL_FETCH_INTERVAL, KALSHI_DEMO_API_KEY_ID_ENV,
    KALSHI_DEMO_BASE_URL, KALSHI_DEMO_PRIVATE_KEY_PATH_ENV, LOCAL_TOTAL_USD, MAX_RETRIES,
    RETRY_INITIAL_DELAY_MS, SIMILARITY_THRESHOLD, paper_caps_demo, paper_caps_local,
};
use crate::market_filter::filter_markets_by_resolution_horizon;
use crate::arbitrage_detector::build_pair_orderbook_ladders;
use crate::paper_trading::{validate_opportunity_from_ladders, PaperEngine};

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

/// Top10 / 追踪共用的套利行（含双方解析日）
type OpportunityRow = (
    ArbitrageOpportunity,
    String,
    String,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

/// 进程内未形成 `KalshiDemoConfig` 时打印原因（环境变量与磁盘无关，只看**当前进程**是否可见）。
fn print_kalshi_demo_not_configured_hint() {
    let has_id = std::env::var(KALSHI_DEMO_API_KEY_ID_ENV)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_path = std::env::var(KALSHI_DEMO_PRIVATE_KEY_PATH_ENV)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    println!("   📘 Kalshi Demo 未启用：本进程未加载到可用凭证（将全部记 ks_exec=local）。");
    if !has_id && !has_path {
        println!(
            "      → 需要同时设置 {} 与 {}",
            KALSHI_DEMO_API_KEY_ID_ENV, KALSHI_DEMO_PRIVATE_KEY_PATH_ENV
        );
        println!("      → 环境变量与项目是否在「同一盘符」无关；须在**运行 cargo run 的终端/IDE**里可见。");
        println!("      → 若在「系统环境变量」里修改过，请**关掉并重新打开**终端或 Cursor 再运行。");
        println!("      → 可在项目根目录放 `.env`（已 gitignore 的常见做法），程序启动时会尝试自动加载。");
    } else {
        if !has_id {
            println!("      → 缺少或为空: {}", KALSHI_DEMO_API_KEY_ID_ENV);
        }
        if !has_path {
            println!("      → 缺少或为空: {}", KALSHI_DEMO_PRIVATE_KEY_PATH_ENV);
        }
    }
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

#[tokio::main]
async fn main() -> Result<()> {
    // 从项目根目录加载 `.env`（若存在）；`std::env::var` 本身不读 .env，仅读进程环境。
    let _ = dotenvy::dotenv();

    println!("🚀 启动跨平台套利监控系统");
    println!("📊 监控平台: Polymarket ↔ Kalshi");
    
    // 初始化日志
    let logger = MonitorLogger::new("logs".to_string())?;
    
    // 初始化未分类日志器
    let unclassified_logger = UnclassifiedLogger::new("logs/unclassified")?;
    
    // 初始化类别映射器
    println!("📚 加载类别配置...");
    let category_mapper = CategoryMapper::from_file("config/categories.toml")
        .context("加载类别配置文件失败")?;
    
    // 初始化客户端
    let polymarket = PolymarketClient::new();

    // Demo 凭证先于 KalshiClient：若启用 Demo 下单，行情/订单簿必须同一基址，否则生产 ticker 在 Demo 会 market_not_found。
    let kalshi_demo_cfg: Option<KalshiDemoConfig> = KalshiDemoConfig::try_from_env()?;
    // Kalshi Demo 限价下单是否仍允许（重试耗尽后为 false，纸面回退为纯本地新会话）。
    let mut demo_trading_active = kalshi_demo_cfg.is_some();
    let kalshi = if kalshi_demo_cfg.is_some() {
        println!("   📗 Kalshi 市场与订单簿使用 Demo API（与 Demo 下单 ticker 一致）");
        KalshiClient::new_with_base_url(KALSHI_DEMO_BASE_URL)
    } else {
        KalshiClient::new()
    };
    if kalshi_demo_cfg.is_some() {
        let (pl, pp) = paper_caps_demo();
        println!("   📗 Kalshi Demo 凭证已加载：IOC 限价单走官方 Demo API");
        println!(
            "   📗 标尺 [demo]：策略参考总盘子 ${} | 每腿探针 ${:.2} | 每对名义上限 ${:.2}",
            DEMO_REFERENCE_BUDGET_USD as i32,
            pl,
            pp
        );
    } else {
        print_kalshi_demo_not_configured_hint();
        let (pl, pp) = paper_caps_local();
        println!(
            "   📒 标尺 [local]：纸面总资金 ${} | 每腿探针 ${:.2} | 每对名义上限 ${:.2}",
            LOCAL_TOTAL_USD as i32,
            pl,
            pp
        );
    }

    // 初始化匹配器
    let matcher_config = MarketMatcherConfig {
        similarity_threshold: SIMILARITY_THRESHOLD,
        use_date_boost: true,
        use_category_boost: true,
        date_boost_factor: 0.05,
        category_boost_factor: 0.03,
        ..Default::default()
    };

    let mut matcher = MarketMatcher::new(matcher_config, category_mapper)
        .with_logger(unclassified_logger);

    // 初始化套利检测器
    let arb_detector = ArbitrageDetector::new(0.02);

    // 模拟盘（可选）：见 `system_params::PAPER_TRADING_ENABLED`
    let mut paper_engine: Option<PaperEngine> = PaperEngine::try_new()?;
    let demo_http = HttpClient::builder()
        .timeout(Duration::from_secs(25))
        .build()
        .context("构建 Kalshi Demo HTTP 客户端")?;
    
    // 初始化监控状态
    let mut monitor_state = MonitorState::new(FULL_FETCH_INTERVAL, 10000);  // 改为10000
    
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
    
    println!("\n✅ 初始化完成");
    println!("   📊 Kalshi 市场数: {}", kalshi_markets.len());
    println!("   📊 Polymarket 市场数: {}", polymarket_markets.len());
    println!("   📊 Kalshi 索引大小: {}", matcher.kalshi_index_size());
    println!("   📊 Polymarket 索引大小: {}", matcher.polymarket_index_size());
    println!("\n🔄 开始监控循环 (间隔: 30秒)...\n");
    
    // 主循环
    loop {
        match run_cycle(
            &polymarket,
            &kalshi,
            &mut matcher,
            &arb_detector,
            &logger,
            &mut monitor_state,
            &mut paper_engine,
            &demo_http,
            kalshi_demo_cfg.as_ref(),
            &mut demo_trading_active,
        ).await {
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

/// 验证单个套利对（带滑点检查）；本周期对 PM/Kalshi **各拉一次**订单簿，解析 ask+bid，供模拟平仓与套利共用。
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
    logger: &MonitorLogger,
    pair_label: &str,
    paper: &mut Option<PaperEngine>,
    demo_http: &HttpClient,
    kalshi_demo_cfg: Option<&KalshiDemoConfig>,
    demo_trading_active: &mut bool,
) -> Result<Option<(ArbitrageOpportunity, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
    let Some(tid) = pm_market.token_ids.first() else {
        return Ok(None);
    };
    let Some(pm_book) = polymarket.get_order_book(tid).await.ok().flatten() else {
        return Ok(None);
    };
    let Some(ks_book) = kalshi
        .get_order_book(&kalshi_market.market_id)
        .await
        .ok()
        .flatten()
    else {
        return Ok(None);
    };

    let Some(ladders) = build_pair_orderbook_ladders(&pm_book, &ks_book, pm_side, kalshi_side)
    else {
        return Ok(None);
    };

    let Some(opp) = validate_opportunity_from_ladders(
        arb_detector,
        &ladders,
        pm_side,
        kalshi_side,
        needs_inversion,
        capital_usdt,
        pair_cap_usdt,
    ) else {
        return Ok(None);
    };

    let pm_orderbook_vec = &ladders.pm_asks;
    let kalshi_orderbook_vec = &ladders.ks_asks;

    let inv = if needs_inversion { " (Y/N颠倒)" } else { "" };
    let pm_slip = if opp.pm_optimal > 0.0 { (opp.pm_avg_slipped - opp.pm_optimal) / opp.pm_optimal * 100.0 } else { 0.0 };
    let ks_slip = if opp.kalshi_optimal > 0.0 { (opp.kalshi_avg_slipped - opp.kalshi_optimal) / opp.kalshi_optimal * 100.0 } else { 0.0 };

    let pm_expiry = match pm_market.resolution_date {
        Some(d) => Some(d),
        None => polymarket.fetch_resolution_by_market_id(&pm_market.market_id).await,
    };
    let ks_expiry = match kalshi_market.resolution_date {
        Some(d) => Some(d),
        None => kalshi.fetch_resolution_by_ticker(&kalshi_market.market_id).await,
    };

    println!("\n  📌 验证通过 (相似度: {:.3}){}", similarity, inv);
    println!("     PM:      {}", pm_market.title);
    println!("     Kalshi:  {}", kalshi_market.title);
    println!("     📅 {}", format_resolution_expiry("PM", pm_expiry));
    println!("     📅 {}", format_resolution_expiry("Kalshi", ks_expiry));
    println!("     ─────────────────────────────────────────────────────────");
    println!("     📗 PM 订单簿(买{}) Top5:", pm_side);
    for (j, (p, s)) in pm_orderbook_vec.iter().take(5).enumerate() {
        println!("         #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if pm_orderbook_vec.is_empty() {
        println!("         (无订单簿)");
    }
    println!("     📗 Kalshi 订单簿(买{}) Top5:", kalshi_side);
    for (j, (p, s)) in kalshi_orderbook_vec.iter().take(5).enumerate() {
        println!("         #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if kalshi_orderbook_vec.is_empty() {
        println!("         (无订单簿)");
    }
    println!("     ─────────────────────────────────────────────────────────");
    println!("     📊 策略: Polymarket 买 {}  +  Kalshi 买 {}", pm_side, kalshi_side);
    println!("     ─────────────────────────────────────────────────────────");
    println!("     📊 对冲份数 n: {:.4}", opp.contracts);
    println!("     💵 最优 Ask:     PM {:.4}  |  Kalshi {:.4}", opp.pm_optimal, opp.kalshi_optimal);
    println!("     📉 滑点后均价:   PM {:.4} ({:+.2}%)  |  Kalshi {:.4} ({:+.2}%)", 
        opp.pm_avg_slipped, pm_slip, opp.kalshi_avg_slipped, ks_slip);
    println!("     ─────────────────────────────────────────────────────────");
    println!("     💰 投入 ${:.2}（每腿探针上限）利润拆解:", capital_usdt);
    println!("        毛利润(兑付): ${:.2}", opp.contracts);
    println!("        - 成本:        ${:.2}", opp.capital_used);
    println!("        - 手续费:      ${:.2}", opp.fees_amount);
    println!("        - Gas费:       ${:.2}", opp.gas_amount);
    println!("        = 净利润:      ${:.2}", opp.net_profit_100);
    println!("        ROI:           {:.1}%", opp.roi_100_percent);
    println!("     ─────────────────────────────────────────────────────────");

    if let Err(e) = logger.log_arbitrage_opportunity(
        cycle_id,
        cycle_phase,
        &opp,
        &pm_market.market_id,
        &kalshi_market.market_id,
        &pm_market.title,
        &kalshi_market.title,
        similarity,
        pm_side,
        kalshi_side,
        needs_inversion,
        pm_expiry,
        ks_expiry,
    ) {
        eprintln!("         ⚠️ 写入监控 CSV 失败: {}", e);
    }

    if let Some(engine) = paper.as_mut() {
        let mut ks_exec_notes = String::from("ks_exec=local");
        if *demo_trading_active {
            if let Some(cfg) = kalshi_demo_cfg {
                let mut placed: Option<(String, String)> = None;
                let mut last_err: Option<String> = None;
                for attempt in 0..MAX_RETRIES {
                    let client_order_id = uuid::Uuid::new_v4().to_string();
                    match kalshi_demo::place_demo_buy_ioc(
                        demo_http,
                        cfg,
                        &kalshi_market.market_id,
                        kalshi_side,
                        &opp,
                        &client_order_id,
                        per_leg_cap_usd,
                    )
                    .await
                    {
                        Ok(order_id) => {
                            placed = Some((order_id, client_order_id));
                            break;
                        }
                        Err(e) => {
                            last_err = Some(format!("{}", e));
                            eprintln!(
                                "   ⚠️ [Kalshi Demo] 下单失败 (尝试 {}/{}): {}",
                                attempt + 1,
                                MAX_RETRIES,
                                e
                            );
                            if attempt + 1 < MAX_RETRIES {
                                let wait_ms =
                                    RETRY_INITIAL_DELAY_MS.saturating_mul(attempt as u64 + 1);
                                time::sleep(Duration::from_millis(wait_ms)).await;
                            }
                        }
                    }
                }
                if let Some((order_id, client_order_id)) = placed {
                    ks_exec_notes = format!(
                        "ks_exec=demo kalshi_order_id={} kalshi_client_order_id={}",
                        order_id, client_order_id
                    );
                    println!(
                        "   📗 [Kalshi Demo] 已提交 IOC 限价买单 order_id={} ticker={}",
                        order_id, kalshi_market.market_id
                    );
                } else {
                    eprintln!(
                        "   ❌ [Kalshi Demo] 重试耗尽，停止 Demo 下单；纸面切换为纯本地新会话。最后错误: {:?}",
                        last_err
                    );
                    *demo_trading_active = false;
                    engine.reset_to_pure_local_after_demo_failure(
                        &format!(
                            "kalshi_demo_order_failed_after_retries {}",
                            last_err.unwrap_or_default()
                        ),
                    )?;
                }
            }
        }
        if let Err(e) = engine.try_open(
            pair_label,
            &opp,
            pm_side,
            kalshi_side,
            cycle_id,
            &pm_market.market_id,
            &kalshi_market.market_id,
            tid.as_str(),
            Utc::now(),
            &ks_exec_notes,
        ) {
            eprintln!("   ⚠️ [Paper] try_open 失败: {}", e);
        }
    }

    Ok(Some((opp, pm_expiry, ks_expiry)))
}

/// 周期开始：对已持仓逐对拉双边订单簿，按 bid 真实吃单评估平仓，写入同一交易 CSV（`CLOSE` / `NO_CLOSE`）。
async fn paper_cycle_start_close_checks(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    paper: &mut Option<PaperEngine>,
    cycle_id: usize,
) -> Result<()> {
    let Some(engine) = paper.as_mut() else {
        return Ok(());
    };
    let positions = engine.snapshot_open_positions();
    if positions.is_empty() {
        return Ok(());
    }
    let check_time = Utc::now();
    for pos in positions {
        let pm_book = match polymarket.get_order_book(&pos.pm_token_id).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                engine.log_no_close_book_error(
                    &pos,
                    cycle_id,
                    check_time,
                    "pm_orderbook_empty",
                )?;
                continue;
            }
            Err(_) => {
                engine.log_no_close_book_error(
                    &pos,
                    cycle_id,
                    check_time,
                    "pm_orderbook_fetch_failed",
                )?;
                continue;
            }
        };
        let ks_book = match kalshi.get_order_book(&pos.kalshi_market_id).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                engine.log_no_close_book_error(
                    &pos,
                    cycle_id,
                    check_time,
                    "kalshi_orderbook_empty",
                )?;
                continue;
            }
            Err(_) => {
                engine.log_no_close_book_error(
                    &pos,
                    cycle_id,
                    check_time,
                    "kalshi_orderbook_fetch_failed",
                )?;
                continue;
            }
        };
        let Some(ladders) =
            build_pair_orderbook_ladders(&pm_book, &ks_book, &pos.pm_side, &pos.kalshi_side)
        else {
            engine.log_no_close_book_error(
                &pos,
                cycle_id,
                check_time,
                "orderbook_parse_failed",
            )?;
            continue;
        };
        engine.check_early_close_at_cycle(&pos.pair_label, &ladders, cycle_id, check_time)?;
    }
    Ok(())
}






/// 执行全量匹配周期
/// 执行全量匹配周期
async fn run_full_match_cycle(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    matcher: &mut MarketMatcher,
    arb_detector: &ArbitrageDetector,
    logger: &MonitorLogger,
    monitor_state: &mut MonitorState,
    trade_amount: f64,
    pair_cap_usdt: f64,
    per_leg_cap_usd: f64,
    paper: &mut Option<PaperEngine>,
    demo_http: &HttpClient,
    kalshi_demo_cfg: Option<&KalshiDemoConfig>,
    demo_trading_active: &mut bool,
    top10_mode_caption: &str,
) -> Result<(usize, usize, String, String)> {
    println!("   📡 执行全量匹配...");
    
    // 获取全量市场
    let polymarket_raw: Vec<Market> = polymarket.fetch_all_markets().await?;
    let kalshi_raw: Vec<Market> = kalshi.fetch_all_markets().await?;
    let now = Utc::now();
    // 21d：`Market.resolution_date`（PM endDate 链 / Kalshi expected_expiration 优先），与终端「到期」一致
    let polymarket_markets = filter_markets_by_resolution_horizon(polymarket_raw, now);
    let kalshi_markets = filter_markets_by_resolution_horizon(kalshi_raw, now);

    println!(
        "      Polymarket: {} 个市场 (21d 窗口内), Kalshi: {} 个市场 (21d 窗口内)",
        polymarket_markets.len(),
        kalshi_markets.len()
    );
    
    println!("\n   🔄 重建索引...");
    matcher.fit_vectorizer(&kalshi_markets, &polymarket_markets)?;
    matcher.build_kalshi_index(&kalshi_markets)?;
    matcher.build_polymarket_index(&polymarket_markets)?;
    
    println!("   🔍 匹配市场...");
    let raw_matches = matcher.find_matches_bidirectional(&polymarket_markets, &kalshi_markets).await;
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
    
    // 验证每个匹配对，统计有套利机会的，并收集用于 Top 10 统计
    let mut verified_count = 0;
    let mut opportunities: Vec<OpportunityRow> = Vec::new();

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
            monitor_state.current_cycle,
            "full_match",
            logger,
            &pair_label,
            paper,
            demo_http,
            kalshi_demo_cfg,
            demo_trading_active,
        )
        .await?
        {
            verified_count += 1;
            crate::cycle_statistics::record_opportunity(&verified);
            opportunities.push((
                verified.clone(),
                pm_market.title.clone(),
                kalshi_market.title.clone(),
                pm_exp,
                ks_exp,
            ));
        }
    }

    // 全量匹配周期结束：输出 Top 10 利润（与文件报告同源字符串）
    let top10_block = format_top10_opportunities(&opportunities, top10_mode_caption);
    print!("{}", top10_block);
    let full_cycle_block = crate::cycle_statistics::on_full_cycle_completed(&opportunities);

    // 更新追踪列表（所有匹配对，带方向信息）
    monitor_state.update_tracked_pairs(all_matches);
    
    Ok((matches.len(), verified_count, top10_block, full_cycle_block))
}


/// 执行价格追踪周期
/// 执行价格追踪周期
async fn run_tracking_cycle(
    polymarket: &PolymarketClient,
    kalshi: &KalshiClient,
    arb_detector: &ArbitrageDetector,
    logger: &MonitorLogger,
    monitor_state: &mut MonitorState,
    trade_amount: f64,
    pair_cap_usdt: f64,
    per_leg_cap_usd: f64,
    paper: &mut Option<PaperEngine>,
    demo_http: &HttpClient,
    kalshi_demo_cfg: Option<&KalshiDemoConfig>,
    demo_trading_active: &mut bool,
    top10_mode_caption: &str,
) -> Result<(usize, String)> {
    println!("   📡 执行价格追踪...");
    println!("      追踪 {} 个匹配对", monitor_state.tracked_pairs.len());

    // 追踪周期：以周期为边界刷新价格/订单簿数据源（与真实时间 60s TTL 解耦；下一周期再清缓存并重新拉取）
    polymarket.clear_price_cache().await;
    kalshi.clear_price_cache().await;

    let mut opportunity_count = 0;
    let mut opportunities: Vec<OpportunityRow> = Vec::new();

    for pair in monitor_state.tracked_pairs.iter_mut() {
        if !pair.active {
            continue;
        }

        // 本周期内每个追踪对从 Gamma 拉取一次 PM 快照；周期内 validate 内不再重复拉 Gamma（仅走订单簿 HTTP）
        if let Ok(fresh_pm) = polymarket
            .fetch_market_snapshot_by_id(&pair.pm_market.market_id)
            .await
        {
            pair.pm_market.outcome_prices = fresh_pm.outcome_prices.or(pair.pm_market.outcome_prices);
            pair.pm_market.best_ask = fresh_pm.best_ask.or(pair.pm_market.best_ask);
            pair.pm_market.best_bid = fresh_pm.best_bid.or(pair.pm_market.best_bid);
            pair.pm_market.last_trade_price =
                fresh_pm.last_trade_price.or(pair.pm_market.last_trade_price);
            pair.pm_market.volume_24h = fresh_pm.volume_24h;
            if !fresh_pm.token_ids.is_empty() {
                pair.pm_market.token_ids = fresh_pm.token_ids;
            }
            if pair.pm_market.resolution_date.is_none() {
                pair.pm_market.resolution_date = fresh_pm.resolution_date;
            }
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
            logger,
            &pair.pair_id,
            paper,
            demo_http,
            kalshi_demo_cfg,
            demo_trading_active,
        )
        .await?
        {
            opportunity_count += 1;
            crate::cycle_statistics::record_opportunity(&verified);
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

    // 追踪周期结束：输出 Top 10 利润
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
    logger: &MonitorLogger,
    monitor_state: &mut MonitorState,
    paper: &mut Option<PaperEngine>,
    demo_http: &HttpClient,
    kalshi_demo_cfg: Option<&KalshiDemoConfig>,
    demo_trading_active: &mut bool,
) -> Result<CycleStats> {
    let start_time = Local::now();
    println!("🔄 开始新周期 #{} - {}", monitor_state.current_cycle, start_time.format("%H:%M:%S"));

    monitor_state.prune_tracked_beyond_resolution_horizon(Utc::now());

    if let Some(p) = paper.as_mut() {
        p.tick_cooldowns();
    }

    paper_cycle_start_close_checks(polymarket, kalshi, paper, monitor_state.current_cycle).await?;

    let (per_leg_cap, pair_cap) = if *demo_trading_active && kalshi_demo_cfg.is_some() {
        paper_caps_demo()
    } else {
        paper_caps_local()
    };
    let trade_amount = per_leg_cap;
    let top10_caption = if *demo_trading_active && kalshi_demo_cfg.is_some() {
        format!(
            "demo · 参考盘子 ${} · 每腿 ${:.2} · 每对 ${:.2}",
            DEMO_REFERENCE_BUDGET_USD as i32,
            per_leg_cap,
            pair_cap
        )
    } else {
        format!(
            "local · 纸面 ${} · 每腿 ${:.2} · 每对 ${:.2}",
            LOCAL_TOTAL_USD as i32,
            per_leg_cap,
            pair_cap
        )
    };
    let is_full_match_cycle = monitor_state.should_full_match();

    // 大周期边界：只重置累计器，不输出「上一大周期总绩效」（小周期结束时的报告仍保留）。
    if is_full_match_cycle && monitor_state.current_cycle > 0 {
        crate::cycle_statistics::reset_big_period_accumulator();
    }

    // top10 / full_roi 仅在终端打印；CSV 仅追加套利行（无周期汇总行）。
    let (new_matches, opportunities, _top10_block, _full_roi_block) = if is_full_match_cycle {
        // 全量匹配周期
        let (m, v, top10, full) = run_full_match_cycle(
            polymarket,
            kalshi,
            matcher,
            arb_detector,
            logger,
            monitor_state,
            trade_amount,
            pair_cap,
            per_leg_cap,
            paper,
            demo_http,
            kalshi_demo_cfg,
            demo_trading_active,
            &top10_caption,
        )
        .await?;
        (m, v, top10, Some(full))
    } else {
        // 价格追踪周期
        let (c, top10) = run_tracking_cycle(
            polymarket,
            kalshi,
            arb_detector,
            logger,
            monitor_state,
            trade_amount,
            pair_cap,
            per_leg_cap,
            paper,
            demo_http,
            kalshi_demo_cfg,
            demo_trading_active,
            &top10_caption,
        )
        .await?;
        (0, c, top10, None)
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
        "🏆 本周期利润 Top 10（{}，含滑点/手续费/Gas）",
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
            "   │  💰 成本${:.2}  手续费${:.2}  Gas${:.2}  →  净利${:.2}",
            opp.capital_used, opp.fees_amount, opp.gas_amount, opp.net_profit_100
        )
        .unwrap();
        writeln!(out, "   │  ROI: {:.1}%", opp.roi_100_percent).unwrap();
        writeln!(out, "   └────────────────────────────────────────────────────────────────").unwrap();
    }
    writeln!(out).unwrap();
    out
}