// src/main.rs
use anyhow::{Context, Result};
use std::cmp::Ordering;
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

use crate::category_mapper::CategoryMapper;
use crate::unclassified_logger::UnclassifiedLogger;
use crate::market::Market;
use clients::{PolymarketClient, KalshiClient};
use market_matcher::{MarketMatcher, MarketMatcherConfig};
use arbitrage_detector::{ArbitrageDetector, ArbitrageOpportunity};
use monitor_logger::MonitorLogger;
use crate::tracking::{MonitorState};
use crate::system_params::{
    FULL_FETCH_INTERVAL, PAPER_PER_LEG_CAP_USDT, SIMILARITY_THRESHOLD,
};
use crate::market_filter::filter_markets_by_resolution_horizon;
use crate::arbitrage_detector::build_pair_orderbook_ladders;
use crate::paper_trading::{validate_opportunity_from_ladders, PaperEngine};

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

#[tokio::main]
async fn main() -> Result<()> {
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
    let kalshi = KalshiClient::new();
    
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
    cycle_id: usize,
    cycle_phase: &str,
    logger: &MonitorLogger,
    pair_label: &str,
    paper: &mut Option<PaperEngine>,
) -> Option<(ArbitrageOpportunity, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> {
    let tid = pm_market.token_ids.first()?;
    let pm_book = polymarket.get_order_book(tid).await.ok()??;
    let ks_book = kalshi
        .get_order_book(&kalshi_market.market_id)
        .await
        .ok()??;

    let ladders = build_pair_orderbook_ladders(&pm_book, &ks_book, pm_side, kalshi_side)?;

    let opp = validate_opportunity_from_ladders(
        arb_detector,
        &ladders,
        pm_side,
        kalshi_side,
        needs_inversion,
        capital_usdt,
    )?;

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
    println!("     💰 投入 {} USDT 利润拆解:", capital_usdt as i32);
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
        ) {
            eprintln!("   ⚠️ [Paper] try_open 失败: {}", e);
        }
    }

    Some((opp, pm_expiry, ks_expiry))
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
    paper: &mut Option<PaperEngine>,
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
    let matches = matcher.find_matches_bidirectional(&polymarket_markets, &kalshi_markets).await;
    println!("      ✅ 找到 {} 个匹配对", matches.len());
    
    // 所有匹配对都加入追踪列表（带方向信息）
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
        let pair_label = format!("{}:{}", pm_market.market_id, kalshi_market.market_id);
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
            monitor_state.current_cycle,
            "full_match",
            logger,
            &pair_label,
            paper,
        )
        .await
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
    let top10_block = format_top10_opportunities(&opportunities);
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
    paper: &mut Option<PaperEngine>,
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
            monitor_state.current_cycle,
            "price_track",
            logger,
            &pair.pair_id,
            paper,
        )
        .await
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
    let top10_block = format_top10_opportunities(&opportunities);
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
) -> Result<CycleStats> {
    let start_time = Local::now();
    println!("🔄 开始新周期 #{} - {}", monitor_state.current_cycle, start_time.format("%H:%M:%S"));

    monitor_state.prune_tracked_beyond_resolution_horizon(Utc::now());

    if let Some(p) = paper.as_mut() {
        p.tick_cooldowns();
    }

    paper_cycle_start_close_checks(polymarket, kalshi, paper, monitor_state.current_cycle).await?;

    let trade_amount = PAPER_PER_LEG_CAP_USDT;
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
            paper,
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
            paper,
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
fn format_top10_opportunities(opportunities: &[OpportunityRow]) -> String {
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
        "╔══════════════════════════════════════════════════════════════════════╗"
    )
    .unwrap();
    writeln!(
        out,
        "║  🏆 本周期利润 Top 10（100 USDT 本金，含滑点/手续费/Gas）                 ║"
    )
    .unwrap();
    writeln!(
        out,
        "╚══════════════════════════════════════════════════════════════════════╝"
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