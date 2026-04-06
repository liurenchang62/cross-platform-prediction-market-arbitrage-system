//! 单笔套利对在窗口结束后的**成交核对**：拉 Kalshi order/fills 与 Polymarket trades，终端输出真实摘要。

use crate::polymarket_trade::PostOrderResponse;
use reqwest::Client as HttpClient;
use serde_json::Value;

use crate::kalshi_trade::{get_fills_for_order, get_order_json, KalshiTradeConfig};
use crate::polymarket_trade::PolymarketClobTrader;

fn fp_str_f64(s: &str) -> f64 {
    s.trim().parse::<f64>().unwrap_or(0.0)
}

fn kalshi_order_fp_usd(order: &Value, key: &str) -> f64 {
    order
        .get(key)
        .and_then(|v| v.as_str())
        .map(fp_str_f64)
        .unwrap_or(0.0)
}

/// 自 fills 汇总：现金流入为正、流出为负；手续费单独累加（美元）。
fn kalshi_fills_signed_cash_and_fees(fills: &[Value]) -> (f64, f64, f64, f64) {
    let mut cash = 0.0_f64;
    let mut fees = 0.0_f64;
    let mut buy_contracts = 0.0_f64;
    let mut sell_contracts = 0.0_f64;
    for f in fills {
        let count = f
            .get("count_fp")
            .and_then(|v| v.as_str())
            .map(fp_str_f64)
            .unwrap_or(0.0);
        let side = f.get("side").and_then(|v| v.as_str()).unwrap_or("");
        let yes_px = f
            .get("yes_price_dollars")
            .and_then(|v| v.as_str())
            .map(fp_str_f64)
            .unwrap_or(0.0);
        let no_px = f
            .get("no_price_dollars")
            .and_then(|v| v.as_str())
            .map(fp_str_f64)
            .unwrap_or(0.0);
        let unit = if side == "yes" { yes_px } else { no_px };
        let fee = f
            .get("fee_cost")
            .and_then(|v| v.as_str())
            .map(fp_str_f64)
            .unwrap_or(0.0);
        fees += fee;
        let action = f.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let leg = count * unit;
        if action == "buy" {
            buy_contracts += count;
            cash -= leg + fee;
        } else if action == "sell" {
            sell_contracts += count;
            cash += leg - fee;
        }
    }
    (cash, fees, buy_contracts, sell_contracts)
}

fn decimal_to_f64(d: &polymarket_client_sdk::types::Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap_or(f64::NAN)
}

#[allow(clippy::too_many_arguments)]
pub async fn print_live_pair_reconciliation(
    ks_http: &HttpClient,
    kalshi_cfg: &KalshiTradeConfig,
    pm_trader: &PolymarketClobTrader,
    pm_token_id: &str,
    pair_label: &str,
    status: &str,
    ks_buy_order_id: &str,
    ks_unwind_order_ids: &[String],
    pm_posts: &[PostOrderResponse],
) {
    crate::trade_out!("\n  ══ 实盘核对：{} （status={}） ══", pair_label, status);

    if ks_buy_order_id.is_empty() || status == "KS_FAIL" || status == "INIT" {
        crate::trade_out!("     （无完整 Kalshi 买入订单可核对；跳过接口拉取）");
        crate::trade_out!("  ═══════════════════════════════════════════════════════\n");
        return;
    }

    // —— Kalshi 买 ——
    match get_order_json(ks_http, kalshi_cfg, ks_buy_order_id).await {
        Ok(v) => {
            if let Some(o) = v.get("order") {
                let fp = o
                    .get("fill_count_fp")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?");
                let taker_cost = kalshi_order_fp_usd(o, "taker_fill_cost_dollars");
                let maker_cost = kalshi_order_fp_usd(o, "maker_fill_cost_dollars");
                let taker_fee = kalshi_order_fp_usd(o, "taker_fees_dollars");
                let maker_fee = kalshi_order_fp_usd(o, "maker_fees_dollars");
                crate::trade_out!(
                    "     📗 Kalshi 买单 order_id={} fill_count_fp={} taker_fill_cost≈${:.4} maker_fill_cost≈${:.4} taker_fees≈${:.4} maker_fees≈${:.4}",
                    ks_buy_order_id, fp, taker_cost, maker_cost, taker_fee, maker_fee
                );
            }
        }
        Err(e) => eprintln!("     ⚠️ Kalshi GET 买单失败: {}", e),
    }
    let mut ks_buy_contracts = 0.0_f64;
    let mut ks_buy_cash = 0.0_f64;
    match get_fills_for_order(ks_http, kalshi_cfg, ks_buy_order_id).await {
        Ok(fills) => {
            let (cash, fees, bc, _sc) = kalshi_fills_signed_cash_and_fees(&fills);
            ks_buy_contracts = bc;
            ks_buy_cash = cash;
            crate::trade_out!(
                "     📗 Kalshi 买 fills：{} 笔 汇总现金流(估)≈${:.4} 手续费(估)≈${:.4} 买量≈{:.2} 份",
                fills.len(),
                cash,
                fees,
                bc
            );
        }
        Err(e) => eprintln!("     ⚠️ Kalshi fills 买单失败: {}", e),
    }

    // —— Kalshi 卖平（若有）——
    let mut ks_unwind_cash = 0.0_f64;
    for oid in ks_unwind_order_ids {
        match get_order_json(ks_http, kalshi_cfg, oid).await {
            Ok(v) => {
                if let Some(o) = v.get("order") {
                    let fp = o
                        .get("fill_count_fp")
                        .and_then(|x| x.as_str())
                        .unwrap_or("?");
                    crate::trade_out!(
                        "     📙 Kalshi 平仓 order_id={} action={:?} fill_count_fp={}",
                        oid,
                        o.get("action").and_then(|x| x.as_str()),
                        fp
                    );
                }
            }
            Err(e) => eprintln!("     ⚠️ Kalshi GET 卖单 {} 失败: {}", oid, e),
        }
        match get_fills_for_order(ks_http, kalshi_cfg, oid).await {
            Ok(fills) => {
                let (cash, fees, _bc, sc) = kalshi_fills_signed_cash_and_fees(&fills);
                ks_unwind_cash += cash;
                crate::trade_out!(
                    "      └ fills：{} 笔 汇总现金流≈${:.4} 手续费≈${:.4} 卖量≈{:.2} 份",
                    fills.len(),
                    cash,
                    fees,
                    sc
                );
            }
            Err(e) => eprintln!("     ⚠️ Kalshi fills 卖单 {} 失败: {}", oid, e),
        }
    }

    // —— Polymarket ——
    let mut pm_shares_total = 0.0_f64;
    let mut pm_fee_sum = 0.0_f64;
    let mut pm_take_sum = 0.0_f64;
    if pm_posts.is_empty() {
        crate::trade_out!("     📘 Polymarket：（本窗口无成功 post_order 返回）");
    } else {
        for (i, post) in pm_posts.iter().enumerate() {
            pm_take_sum += decimal_to_f64(&post.taking_amount);
            crate::trade_out!(
                "     📘 Polymarket 第 {} 笔 orderID={} success={} status={:?} taking_amt={} making_amt={} trade_ids={}",
                i + 1,
                post.order_id,
                post.success,
                post.status,
                post.taking_amount,
                post.making_amount,
                post.trade_ids.len()
            );
            match pm_trader.fetch_trades_for_post_order(pm_token_id, post).await {
                Ok(trs) => {
                    for t in &trs {
                        let sz = decimal_to_f64(&t.size);
                        let px = decimal_to_f64(&t.price);
                        let bps = decimal_to_f64(&t.fee_rate_bps);
                        let fee_est = sz * px * (bps / 10_000.0);
                        pm_shares_total += sz;
                        if fee_est.is_finite() {
                            pm_fee_sum += fee_est;
                        }
                        crate::trade_out!(
                            "        └ trade id={} side={:?} size={} price={} fee_rate_bps={} 费用(估)≈${:.6}",
                            t.id, t.side, t.size, t.price, t.fee_rate_bps, fee_est
                        );
                    }
                }
                Err(e) => eprintln!("        ⚠️ 拉 trade 失败: {}", e),
            }
        }
        crate::trade_out!(
            "     📘 PM 合计：taking_amount 合≈{:.6} 成交量≈{:.4} 份 手续费(按 bps 估)≈${:.6}",
            pm_take_sum, pm_shares_total, pm_fee_sum
        );
    }

    if matches!(
        status,
        "UNWIND_COMPARE" | "UNWIND_TIMEOUT" | "UNWIND_TIMEOUT_PARTIAL" | "UNWIND_TIMEOUT_FAIL"
    ) {
        let round = ks_buy_cash + ks_unwind_cash;
        crate::trade_out!("     📌 **平仓摘要**：Kalshi 买+卖 fills 现金流(估)合计≈${:.4}（负为净支出）；PM 侧 taking 合≈${:.4}。", round, pm_take_sum);
        crate::trade_out!("        若仅卖平未对冲 PM，盈亏主要在 Kalshi 价差；请与交易所对账。");
    } else if status == "OK" || status == "PARTIAL_PM_PROGRESS" {
        let matched = ks_buy_contracts.min(pm_shares_total);
        if matched > 1e-6 && pm_take_sum.is_finite() && ks_buy_cash.is_finite() {
            let payout = matched * 1.0;
            let cash_out_est = -ks_buy_cash + pm_take_sum;
            let net_resolution_est = payout - cash_out_est;
            let denom = cash_out_est.abs().max(1e-9);
            let roi_pct = (net_resolution_est / denom) * 100.0;
            crate::trade_out!(
                "     💰 **对冲摘要**（按接口分项估算，结算时 $1/份）：配对≈{:.4} 份 | 到期毛兑付≈${:.4} | 现金流出合≈${:.4}（Kalshi 买现金流幅值 + PM taking） | 预计至结算净利≈${:.4} | 估算 ROI≈{:.2}%",
                matched, payout, cash_out_est, net_resolution_est, roi_pct
            );
            crate::trade_out!("        说明：微利已含 fills 内手续费口径；PM 费另按 trade bps 估 ${:.6}；结算与汇率为理论值。", pm_fee_sum);
        }
    }

    crate::trade_out!("     ℹ️ 以上为 API 分项汇总；最终以账簿与结算为准。");
    crate::trade_out!("  ═══════════════════════════════════════════════════════\n");
}
