//! 套利检出时的终端订单簿展示（不落盘 CSV）。

use chrono::{DateTime, Utc};

use crate::arbitrage_detector::ArbitrageOpportunity;

/// 终端：仅跟踪模式下每次检出套利时打印簿前 5 档（实盘路径仍由 `validate_arbitrage_pair_post_success` 详打）。
pub fn print_arbitrage_detection_books(
    opp: &ArbitrageOpportunity,
    pm_market_title: &str,
    kalshi_market_title: &str,
    similarity: f64,
    needs_inversion: bool,
    pm_side: &str,
    kalshi_side: &str,
    pm_expiry: Option<DateTime<Utc>>,
    ks_expiry: Option<DateTime<Utc>>,
) {
    let inv = if needs_inversion { " (Y/N颠倒)" } else { "" };
    println!("\n  📌 检出套利 (similarity {:.3}){}", similarity, inv);
    println!("     PM:     {}", pm_market_title);
    println!("     Kalshi: {}", kalshi_market_title);
    if let Some(d) = pm_expiry {
        println!("     PM 到期: {} UTC", d.format("%Y-%m-%d %H:%M"));
    }
    if let Some(d) = ks_expiry {
        println!("     KS 到期: {} UTC", d.format("%Y-%m-%d %H:%M"));
    }
    println!("     📗 PM 买{} 簿 Top5:", pm_side);
    for (j, (p, s)) in opp.orderbook_pm_top5.iter().take(5).enumerate() {
        println!("        #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_pm_top5.is_empty() {
        println!("        (空)");
    }
    println!("     📗 Kalshi 买{} 簿 Top5:", kalshi_side);
    for (j, (p, s)) in opp.orderbook_kalshi_top5.iter().take(5).enumerate() {
        println!("        #{}. 价 {:.4} 量 {:.1}", j + 1, p, s);
    }
    if opp.orderbook_kalshi_top5.is_empty() {
        println!("        (空)");
    }
    println!(
        "     💰 模型净利 ${:.2}  ROI {:.1}%  n={:.4}",
        opp.net_profit_100, opp.roi_100_percent, opp.contracts
    );
}
