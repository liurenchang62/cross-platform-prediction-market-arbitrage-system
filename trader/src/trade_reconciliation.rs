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
    // BUY 订单：making=USDC支付，taking=token收到；SELL 订单：making=token卖出，taking=USDC收到。
    // pm_buy_tokens  = 各 BUY 订单 taking_amount 之和（token）
    // pm_sell_tokens = 各 SELL 订单 making_amount 之和（token）
    // pm_shares_total = trade API 返回的成交量（可能延迟，仅供参考）
    let mut pm_shares_total = 0.0_f64;
    let mut pm_fee_sum = 0.0_f64;
    let mut pm_buy_tokens = 0.0_f64;
    let mut pm_sell_tokens = 0.0_f64;
    let mut pm_buy_cost = 0.0_f64;
    let mut pm_sell_usdc = 0.0_f64;
    if pm_posts.is_empty() {
        crate::trade_out!("     📘 Polymarket：（本窗口无成功 post_order 返回）");
    } else {
        for (i, post) in pm_posts.iter().enumerate() {
            let taking = decimal_to_f64(&post.taking_amount);
            let making = decimal_to_f64(&post.making_amount);
            // PostOrderResponse 无 side 字段，通过 making < taking 判断方向：
            // BUY：making=USDC（如$4.44）< taking=tokens（如12）→ true
            // SELL：making=tokens（如4）> taking=USDC（如$0.80）→ false
            // 预测市场 token 价格永远 < $1，故此等式对任意 n≥1 恒成立，无需 `|| taking > 1.0`（该条件反而误判SELL为BUY）
            let is_buy = making < taking;
            if is_buy {
                pm_buy_tokens += taking;
                pm_buy_cost   += making;
            } else {
                pm_sell_tokens += making;
                pm_sell_usdc   += taking;
            }
            crate::trade_out!(
                "     📘 PM 第 {} 笔 orderID={} success={} status={:?} making={} taking={} trade_ids={}",
                i + 1,
                post.order_id,
                post.success,
                post.status,
                post.making_amount,
                post.taking_amount,
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
            "     📘 PM 合计：买入token≈{:.4} 支付USDC≈${:.4} | 卖出token≈{:.4} 回收USDC≈${:.4} | trade成交量≈{:.4} 份 手续费(估)≈${:.6}",
            pm_buy_tokens, pm_buy_cost, pm_sell_tokens, pm_sell_usdc, pm_shares_total, pm_fee_sum
        );
    }
    // 下方 OK 路径用 pm_buy_tokens 作为 pm_take_sum（BUY token 数），pm_buy_cost 作为成本
    let pm_take_sum = pm_buy_tokens;

    if status == "OK" || status == "RECONCILE_KS" || status == "RECONCILE_PM" {
        let matched = ks_buy_contracts.min(pm_take_sum);
        if matched > 1e-6 && pm_take_sum.is_finite() && ks_buy_cash.is_finite() {
            let payout = matched * 1.0;
            let cash_out_est = -ks_buy_cash + pm_buy_cost;
            let net_resolution_est = payout - cash_out_est;
            let denom = cash_out_est.abs().max(1e-9);
            let roi_pct = (net_resolution_est / denom) * 100.0;
            crate::trade_out!(
                "     💰 **对冲摘要**（按接口分项估算，结算时 $1/份）：配对≈{:.4} 份 | 到期毛兑付≈${:.4} | 现金流出合≈${:.4}（Kalshi 买现金流幅值 + PM USDC成本） | 预计至结算净利≈${:.4} | 估算 ROI≈{:.2}%",
                matched, payout, cash_out_est, net_resolution_est, roi_pct
            );
            crate::trade_out!("        说明：微利已含 fills 内手续费口径；PM 费另按 trade bps 估 ${:.6}；结算与汇率为理论值。", pm_fee_sum);
        }
    }

    crate::trade_out!("     ℹ️ 以上为 API 分项汇总；最终以账簿与结算为准。");
    crate::trade_out!("  ═══════════════════════════════════════════════════════\n");
}
