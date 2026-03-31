//! 四个子命令对应的业务逻辑（由 `src/bin/*.rs` 调用）。

use std::path::Path;

use anyhow::Result;
use reqwest::Client;

use crate::charts;
use crate::gamma;
use crate::kalshi;
use crate::mapping::load_mapping;
use crate::pm;

fn http() -> Result<Client> {
    Ok(Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

pub async fn run_overview(config: &Path) -> Result<()> {
    let http = http()?;
    let pairs = load_mapping(config)?;
    let ks = kalshi::load_kalshi()?;
    let pm = pm::connect().await?;
    let cents = kalshi::balance_cents(&http, &ks).await?;
    let col = pm::collateral_balance(&pm).await?;
    println!("══ 账户总览 ══");
    println!(
        "Kalshi  balance={}¢ (${:.2})",
        cents,
        cents as f64 / 100.0
    );
    println!("Polymarket CLOB Collateral balance={} (SDK Decimal)", col);

    let sign_p = "/trade-api/v2/portfolio/positions";
    let url = format!("{}/portfolio/positions?limit=1000", ks.base);
    let body = kalshi::signed_get(&http, &ks, &url, sign_p).await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let arr = v["market_positions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut ks_nz = 0_usize;
    for p in &arr {
        let fp = p["position_fp"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        if fp.abs() >= 1e-6 {
            ks_nz += 1;
        }
    }
    println!("Kalshi 非零仓位 markets: {}", ks_nz);

    let mut pm_tokens_nz = 0_usize;
    for pair in &pairs {
        let dec = pm::conditional_shares(&pm, &pair.pm_buy_token_id).await?;
        let x: f64 = dec.to_string().parse().unwrap_or(0.0);
        if x.abs() >= 1e-9 {
            pm_tokens_nz += 1;
        }
    }
    println!(
        "Polymarket 映射内 token 上有余额的条数: {} / {}",
        pm_tokens_nz,
        pairs.len()
    );

    let mut arb_open = 0_usize;
    for pair in &pairs {
        let pos = kalshi::positions_for_ticker(&http, &ks, &pair.kalshi_ticker).await?;
        let ks_fp = pos
            .as_ref()
            .and_then(|p| p["position_fp"].as_str()?.parse::<f64>().ok())
            .unwrap_or(0.0);
        let ks_open = ks_fp.abs();
        let pm_sh = pm::conditional_shares(&pm, &pair.pm_buy_token_id).await?;
        let pm_o: f64 = pm_sh.to_string().parse().unwrap_or(0.0);
        if ks_open >= 1e-6 && pm_o >= 1e-9 {
            arb_open += 1;
        }
    }
    println!(
        "映射内「Kalshi 与 PM 同时非零」的套利对估计: {}（以仓位快照为准）",
        arb_open
    );
    Ok(())
}

pub async fn run_pairs(config: &Path) -> Result<()> {
    let http = http()?;
    let pairs = load_mapping(config)?;
    let ks = kalshi::load_kalshi()?;
    let pm = pm::connect().await?;
    for pair in &pairs {
        println!("\n──────── 映射 id={} ────────", pair.id);
        let ks_title = kalshi::market_title(&http, &ks, &pair.kalshi_ticker)
            .await
            .unwrap_or_else(|_| "(Kalshi title 拉取失败)".into());
        let pm_title = gamma::pm_market_title(&http, &pair.pm_market_id)
            .await
            .unwrap_or_else(|_| "(Gamma title 拉取失败)".into());
        println!("Kalshi  [{}] {}", pair.kalshi_ticker, ks_title);
        println!("PM      [{}] {}", pair.pm_market_id, pm_title);
        println!(
            "策略腿: PM 买 {} | Kalshi 买 {}",
            pair.strategy.pm_buy_leg, pair.strategy.kalshi_buy_leg
        );

        let leg = pair.ks_side_lower();
        let fills = kalshi::all_fills_for_ticker(&http, &ks, &pair.kalshi_ticker).await?;
        let (ks_cash, ks_fee_sum, ks_net_c, ks_last) =
            kalshi::fills_net_leg_ledger(&fills, leg.as_str());

        let trades = pm::all_trades_for_token(&pm, &pair.pm_buy_token_id).await?;
        let (pm_cash, pm_fee_sum, pm_net_sh, pm_last) = pm::trades_net_ledger(&trades);

        let cond = pm::conditional_shares(&pm, &pair.pm_buy_token_id).await?;
        let pm_cond: f64 = cond.to_string().parse().unwrap_or(0.0);

        let pos = kalshi::positions_for_ticker(&http, &ks, &pair.kalshi_ticker).await?;
        let ks_fp = pos
            .as_ref()
            .and_then(|p| p["position_fp"].as_str()?.parse::<f64>().ok())
            .unwrap_or(0.0);
        println!(
            "Kalshi 持仓 position_fp={} exposure_dollars={:?}",
            pos.as_ref()
                .and_then(|p| p["position_fp"].as_str())
                .unwrap_or("0"),
            pos.as_ref()
                .and_then(|p| p["market_exposure_dollars"].as_str())
        );
        println!(
            "Kalshi  fills 净{}边: 净现金流支出(买−卖估算)≈${:.4} | 官方 fee 累加≈${:.4} | 净份数≈{:.4} | 最后成交 {:?}",
            leg.to_uppercase(),
            ks_cash,
            ks_fee_sum,
            ks_net_c,
            ks_last
        );
        println!(
            "PM token {} 条件份额(官方 balance-allowance)={}",
            pair.pm_buy_token_id, cond
        );
        println!(
            "PM trades 净: 现金流支出≈${:.4} | 费(bps 官方)估和≈${:.4} | 净份额≈{:.4} | 末笔 id {:?}",
            pm_cash, pm_fee_sum, pm_net_sh, pm_last
        );

        let matched = ks_fp.abs().min(pm_cond);
        let payout = matched * 1.0;
        let cost = ks_cash.max(0.0) + pm_cash.max(0.0);
        let exp_net = payout - cost;
        let roi = if cost > 1e-9 {
            (exp_net / cost) * 100.0
        } else {
            0.0
        };
        println!(
            "── 理论到期（若完全兑付 $1/份 & 映射事件一致）── 对冲份数≈min(|KS fp|,{:.4})={:.4} | 预期毛收入 ${:.4} | 以 fills/trades 净支出估成本 ${:.4} | 估净利 ${:.4} | 估 ROI {:.2}%",
            pm_cond,
            matched,
            payout,
            cost,
            exp_net,
            roi
        );
        println!("（未结算；PM 赎回若未出现在 trades 中则成本可能低估。）");
    }
    Ok(())
}

pub async fn run_settled(config: &Path) -> Result<()> {
    let http = http()?;
    let pairs = load_mapping(config)?;
    let ks = kalshi::load_kalshi()?;
    let pm = pm::connect().await?;
    for pair in &pairs {
        println!("\n──────── 结算核对 id={} ────────", pair.id);
        let set_rows =
            kalshi::all_settlements_for_ticker(&http, &ks, &pair.kalshi_ticker).await?;
        if set_rows.is_empty() {
            println!("Kalshi settlements: （无记录或未结算）");
        } else {
            for s in &set_rows {
                println!(
                    "Kalshi settlement: result={:?} revenue¢={} fee_cost={} settled_time={} yes_fp={} no_fp={}",
                    s["market_result"].as_str(),
                    s["revenue"].as_i64().unwrap_or(0),
                    s["fee_cost"].as_str().unwrap_or("?"),
                    s["settled_time"].as_str().unwrap_or("?"),
                    s["yes_count_fp"].as_str().unwrap_or("?"),
                    s["no_count_fp"].as_str().unwrap_or("?"),
                );
            }
            let k_pnl = kalshi::settlements_realized_usd(&set_rows);
            println!(
                "Kalshi 官方 settlements 合计（revenue/1$−fee_cost 口径）: ${:.4}",
                k_pnl
            );
        }
        let closed = gamma::pm_market_closed(&http, &pair.pm_market_id)
            .await
            .unwrap_or(false);
        println!("Gamma 市场 closed={}", closed);
        let trades = pm::all_trades_for_token(&pm, &pair.pm_buy_token_id).await?;
        let (pm_cash, pm_fees, net_sh, _) = pm::trades_net_ledger(&trades);
        println!(
            "PM trades 生命周期净现金流(买正卖负支出口径 Net)≈${:.4} | 费≈${:.4} | 净剩份额≈{:.4}",
            pm_cash, pm_fees, net_sh
        );
        let cond = pm::conditional_shares(&pm, &pair.pm_buy_token_id).await?;
        println!(
            "PM 当前条件余额: {}（非零则可能仍有头寸或赎回在链上未完成反映在 trades）",
            cond
        );

        let ks_real = kalshi::settlements_realized_usd(&set_rows);
        let pm_real = -pm_cash;
        println!(
            "── 粗.realized 合并（KS 用 settlements；PM 用 −trades 净现金流的相反数近似已实现）── 合计≈${:.4}",
            ks_real + pm_real
        );
    }
    Ok(())
}

pub async fn run_charts(config: &Path, events: &Path, out: &Path) -> Result<()> {
    let http = http()?;
    let pairs = load_mapping(config)?;
    let days = charts::trades_per_day_from_jsonl(events)?;
    let tickers: Vec<String> = pairs.iter().map(|p| p.kalshi_ticker.clone()).collect();
    let ks = kalshi::load_kalshi()?;
    let cum = charts::cumulative_kalshi_settlements(&http, &ks, &tickers).await?;
    std::fs::create_dir_all(out).ok();
    let p1 = out.join("trades_per_day.png");
    let p2 = out.join("kalshi_settlements_cumulative.png");
    charts::render_trades_chart(&days, &p1)?;
    charts::render_cumulative_realized(&cum, &p2)?;
    println!("已写入 {:?} 与 {:?}", p1, p2);
    Ok(())
}
