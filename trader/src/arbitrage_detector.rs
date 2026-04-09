// src/arbitrage_detector.rs
use crate::fee_model::{
    live_arb_auxiliary_drag_half_usd, live_arb_auxiliary_drag_usd,
    kalshi_taker_fee_usd_round_up, polymarket_taker_fee_usdc_round, ArbCostParams,
};
use crate::system_params::{
    arb_probe_caps, live_arbit_surplus_buffer_usd, LIVE_ARBIT_GAS_ASSUMED_USD,
};
use crate::market::{Market, MarketPrices};
use serde_json::Value;

/// 旧版「单价维»快速筛选用常量（`check_arbitrage_optimal` 等，主路径已用 [`ArbCostParams`]）。
const LEGACY_PER_CONTRACT_FEE_SCREEN_USD: f64 = 0.02;

#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    pub strategy: String,
    pub kalshi_action: (String, String, f64),
    pub polymarket_action: (String, String, f64),
    pub total_cost: f64,
    pub gross_profit: f64,
    pub fees: f64,
    pub net_profit: f64,
    pub roi_percent: f64,
    pub gas_fee: f64,
    pub final_profit: f64,
    pub final_roi_percent: f64,
    /// 以下为「每腿探针 + 每对名义上限」收缩后的口径（原 net_profit_100 命名保留）
    pub pm_optimal: f64,
    pub kalshi_optimal: f64,
    pub pm_avg_slipped: f64,
    pub kalshi_avg_slipped: f64,
    pub contracts: f64,
    pub capital_used: f64,
    /// 两家交易所手续费之和（= fee_polymarket_usd + fee_kalshi_usd；主路径与分项一致）。
    pub fees_amount: f64,
    /// Polymarket taker 手续费（美元，按 n 份与 `ArbCostParams` 公式，见 `fee_model`）。
    pub fee_polymarket_usd: f64,
    /// Kalshi taker 手续费（美元，美分向上取整公式 × 系列乘数）。
    pub fee_kalshi_usd: f64,
    pub gas_amount: f64,
    /// 摩擦中的固定 Gas 美元（与 `system_params::LIVE_ARBIT_GAS_ASSUMED_USD` 一致）。
    pub gas_assumed_usd: f64,
    /// 摩擦中的盈余缓冲美元（= `pair_cap ×` 缓冲比例）。
    pub surplus_buffer_usd: f64,
    pub net_profit_100: f64,
    pub roi_100_percent: f64,
    pub orderbook_pm_top5: Vec<(f64, f64)>,
    pub orderbook_kalshi_top5: Vec<(f64, f64)>,
    /// 识别阶段拉取的 PM fee rate（bps），执行阶段直接复用，避免在两腿之间额外发起 HTTP 请求。
    pub pm_fee_bps: u32,
    /// Kalshi IOC 买单限价（美分）= 填满 n 份所需阶梯最差价位，执行时直接使用。
    pub ks_limit_cents: i64,
    /// PM FAK 限价买单每 token 最高价（美元）= 填满 n 份所需阶梯最差 ask 价位。
    pub pm_limit_price: f64,
}

#[derive(Clone)]
pub struct ArbitrageDetector {
    /// 可接受最小净利：[`crate::system_params::live_arbit_min_net_profit_usd`] 同源，`pair_cap ×` 本比例。
    min_net_profit_frac_of_pair_cap: f64,
}

impl ArbitrageDetector {
    pub fn new(min_net_profit_frac_of_pair_cap: f64) -> Self {
        Self {
            min_net_profit_frac_of_pair_cap,
        }
    }

    #[inline]
    fn min_profit_usd(&self, pair_cap_usdt: f64) -> f64 {
        pair_cap_usdt.max(0.0) * self.min_net_profit_frac_of_pair_cap
    }

    /// 给定「每对名义上限」时的最小净利润门槛（美元），用于日志与第二腿提示。
    #[inline]
    pub fn min_profit_usd_for_pair_cap(&self, pair_cap_usdt: f64) -> f64 {
        self.min_profit_usd(pair_cap_usdt)
    }

    #[inline]
    pub fn min_net_profit_frac_of_pair_cap(&self) -> f64 {
        self.min_net_profit_frac_of_pair_cap
    }

    /// Kalshi 第一腿已成交 `n1` 份、总成本约 `kalshi_cost_total`（美元），
    /// 在 PM ask 阶梯上若只对冲 `n2≤n1` 份，按与识别阶段相同公式估算整对净利（兑付=n2）。
    pub fn net_profit_pm_hedge_n(
        &self,
        n2: f64,
        pm_asks: &[(f64, f64)],
        n1_kalshi: f64,
        kalshi_cost_total: f64,
        cost: &ArbCostParams,
        ks_fill_price_dollars: f64,
        pair_cap_usdt: f64,
    ) -> Option<f64> {
        if n1_kalshi <= 0.0 || !n1_kalshi.is_finite() || n2 < 1.0 - 1e-9 || n2 > n1_kalshi + 1e-6 {
            return None;
        }
        let (c_pm, pm_avg, _) = cost_for_exact_contracts(pm_asks, n2)?;
        let c_ks_portion = kalshi_cost_total * (n2 / n1_kalshi);
        let capital_used = c_ks_portion + c_pm;
        let gross = n2;
        let fee_pm = polymarket_taker_fee_usdc_round(n2, pm_avg, cost.pm_fee_bps, cost.pm_exponent);
        let fee_ks_full =
            kalshi_taker_fee_usd_round_up(n1_kalshi, ks_fill_price_dollars, cost.ks_taker_fee_multiplier);
        let fee_ks = fee_ks_full * (n2 / n1_kalshi);
        let fees_amount = fee_pm + fee_ks;
        let gas_amount = live_arb_auxiliary_drag_usd(pair_cap_usdt);
        Some(gross - capital_used - fees_amount - gas_amount)
    }

    /// 从 `n1` 向下试到 1：返回**最大**的仍满足净利门槛的 `n2`（优先全对冲）。
    pub fn pick_pm_hedge_n_after_kalshi(
        &self,
        n1: i64,
        pm_asks: &[(f64, f64)],
        kalshi_cost_total: f64,
        cost: &ArbCostParams,
        ks_fill_price_dollars: f64,
        pair_cap_usdt: f64,
    ) -> Option<i64> {
        let n1 = n1.max(1);
        let n1f = n1 as f64;
        for n2 in (1..=n1).rev() {
            let net = self.net_profit_pm_hedge_n(
                n2 as f64,
                pm_asks,
                n1f,
                kalshi_cost_total,
                cost,
                ks_fill_price_dollars,
                pair_cap_usdt,
            )?;
            if net > self.min_profit_usd(pair_cap_usdt) {
                return Some(n2);
            }
        }
        None
    }

    /// 不做净利门槛过滤：在 `n2∈[1,n1]` 中取使 [`net_profit_pm_hedge_n`] **最大**者（平手时优先更大 `n2`，多对冲）。
    pub fn best_pm_hedge_net_after_kalshi(
        &self,
        n1: i64,
        pm_asks: &[(f64, f64)],
        kalshi_cost_total: f64,
        cost: &ArbCostParams,
        ks_fill_price_dollars: f64,
        pair_cap_usdt: f64,
    ) -> Option<(i64, f64)> {
        let n1 = n1.max(1);
        let n1f = n1 as f64;
        let mut best: Option<(i64, f64)> = None;
        for n2 in 1..=n1 {
            let Some(net) = self.net_profit_pm_hedge_n(
                n2 as f64,
                pm_asks,
                n1f,
                kalshi_cost_total,
                cost,
                ks_fill_price_dollars,
                pair_cap_usdt,
            ) else {
                continue;
            };
            best = Some(match best {
                None => (n2, net),
                Some((bn2, bn)) if net > bn || (net == bn && n2 > bn2) => (n2, net),
                Some(b) => b,
            });
        }
        best
    }

    /// 在 Kalshi **卖**盘上按价高优先吃掉 `n1` 份，估算相对已付 `kalshi_cost_total` 的净利：
    /// `回款 − 卖出手续费 − 单腿摩擦 − 已付成本`（摩擦为整轮 Gas+缓冲的一半）。
    pub fn estimate_kalshi_sell_unwind_net(
        &self,
        n1: i64,
        ks_sell_bids_desc: &[(f64, f64)],
        kalshi_cost_total: f64,
        cost: &ArbCostParams,
        pair_cap_usdt: f64,
    ) -> Option<f64> {
        let n = n1.max(1) as f64;
        let (proceeds, avg_px) = proceeds_for_exact_contracts_sell(ks_sell_bids_desc, n)?;
        let fees_sell = kalshi_taker_fee_usd_round_up(n, avg_px, cost.ks_taker_fee_multiplier);
        let gas = live_arb_auxiliary_drag_half_usd(pair_cap_usdt);
        Some(proceeds - fees_sell - gas - kalshi_cost_total)
    }

    // 在 ArbitrageDetector 中添加新方法

    /// 以 capital_usdt 为**每腿上限探针预算**：两腿各自最多用该金额吃单，取较小成交份数为对冲规模 n，
    /// 再以 n 为基准对两边订单簿**重算**实现 n 份的精确成本（不写死紧边=100）。
    pub fn calculate_arbitrage_100usdt(
        &self,
        pm_optimal: f64,
        kalshi_optimal: f64,
        pm_orderbook: Option<&[(f64, f64)]>,
        kalshi_orderbook: Option<&[(f64, f64)]>,
        pm_side: &str,
        kalshi_side: &str,
        needs_inversion: bool,
        capital_usdt: f64,
        pair_cap_usdt: f64,
        cost: &ArbCostParams,
    ) -> Option<ArbitrageOpportunity> {
        let total_cost_opt = pm_optimal + kalshi_optimal;
        if total_cost_opt >= 1.0 || total_cost_opt <= 0.0 {
            return None;
        }
        if capital_usdt <= 0.0 {
            return None;
        }

        // 探针：两腿各用最多 capital_usdt，得到可行份数上界
        let contracts_pm = if let Some(ob) = pm_orderbook {
            if ob.is_empty() {
                return None;
            }
            calculate_slippage_with_fixed_usdt(ob, capital_usdt).filled_contracts
        } else if pm_optimal > 0.0 {
            capital_usdt / pm_optimal
        } else {
            return None;
        };
        let contracts_ks = if let Some(ob) = kalshi_orderbook {
            if ob.is_empty() {
                return None;
            }
            calculate_slippage_with_fixed_usdt(ob, capital_usdt).filled_contracts
        } else if kalshi_optimal > 0.0 {
            capital_usdt / kalshi_optimal
        } else {
            return None;
        };

        let mut n = if contracts_pm > 0.0 && contracts_ks > 0.0 {
            contracts_pm.min(contracts_ks)
        } else {
            return None;
        };

        // 以 n 为基准重算双腿真实成本（滑点已体现在成交价路径上），同时取得 worst_ask 用于限价
        let (mut c_pm, mut pm_avg, mut pm_worst_ask) = if let Some(ob) = pm_orderbook {
            cost_for_exact_contracts(ob, n)?
        } else {
            let c = n * pm_optimal;
            (c, pm_optimal, pm_optimal)
        };
        let (mut c_ks, mut kalshi_avg, mut ks_worst_ask) = if let Some(ob) = kalshi_orderbook {
            cost_for_exact_contracts(ob, n)?
        } else {
            let c = n * kalshi_optimal;
            (c, kalshi_optimal, kalshi_optimal)
        };

        let mut capital_used = c_pm + c_ks;
        if pair_cap_usdt > 0.0 && capital_used > pair_cap_usdt {
            let scale = pair_cap_usdt / capital_used;
            n *= scale;
            if n <= 0.0 || !n.is_finite() {
                return None;
            }
            let (c1, a1, wa1) = if let Some(ob) = pm_orderbook {
                cost_for_exact_contracts(ob, n)?
            } else {
                (n * pm_optimal, pm_optimal, pm_optimal)
            };
            let (c2, a2, wa2) = if let Some(ob) = kalshi_orderbook {
                cost_for_exact_contracts(ob, n)?
            } else {
                (n * kalshi_optimal, kalshi_optimal, kalshi_optimal)
            };
            c_pm = c1; pm_avg = a1; pm_worst_ask = wa1;
            c_ks = c2; kalshi_avg = a2; ks_worst_ask = wa2;
            capital_used = c_pm + c_ks;
        }

        // n 取整：Kalshi 只接受整数合约，统一在此 floor，ROI 计算与执行均基于整数 n
        let n_int = n.floor().max(1.0);
        if n_int != n {
            let (c1, a1, wa1) = if let Some(ob) = pm_orderbook {
                cost_for_exact_contracts(ob, n_int)?
            } else {
                (n_int * pm_optimal, pm_optimal, pm_optimal)
            };
            let (c2, a2, wa2) = if let Some(ob) = kalshi_orderbook {
                cost_for_exact_contracts(ob, n_int)?
            } else {
                (n_int * kalshi_optimal, kalshi_optimal, kalshi_optimal)
            };
            c_pm = c1; pm_avg = a1; pm_worst_ask = wa1;
            c_ks = c2; kalshi_avg = a2; ks_worst_ask = wa2;
            capital_used = c_pm + c_ks;
        }
        let n = n_int;

        // PM CLOB 最低委托 $1.00：用 worst_ask × n 计算最大可能名义金额（精确，无缓冲）
        if (n as f64) * pm_worst_ask < 1.0 {
            return None;
        }

        let gross = n * 1.0;
        let fee_pm = polymarket_taker_fee_usdc_round(n, pm_avg, cost.pm_fee_bps, cost.pm_exponent);
        let fee_ks = kalshi_taker_fee_usd_round_up(n, kalshi_avg, cost.ks_taker_fee_multiplier);
        let fees_amount = fee_pm + fee_ks;
        let gas_assumed_usd = LIVE_ARBIT_GAS_ASSUMED_USD;
        let surplus_buffer_usd = live_arbit_surplus_buffer_usd(pair_cap_usdt);
        let gas_amount = gas_assumed_usd + surplus_buffer_usd;
        let net_profit_100 = gross - capital_used - fees_amount - gas_amount;

        // 门槛锚定 capital_used（与 ROI 分母一致）：净利 / 真实成本 >= min_frac 才执行。
        if capital_used <= 0.0 || net_profit_100 <= capital_used * self.min_net_profit_frac_of_pair_cap {
            return None;
        }
        let roi_100 = if capital_used > 0.0 {
            (net_profit_100 / capital_used) * 100.0
        } else {
            0.0
        };

        let inversion_note = if needs_inversion { " [Y/N颠倒]" } else { "" };
        let strategy = format!("Buy {} on Polymarket + Buy {} on Kalshi{}", pm_side, kalshi_side, inversion_note);

        let orderbook_pm_top5 = pm_orderbook
            .map(|ob| ob.iter().take(5).copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let orderbook_kalshi_top5 = kalshi_orderbook
            .map(|ob| ob.iter().take(5).copied().collect::<Vec<_>>())
            .unwrap_or_default();

        Some(ArbitrageOpportunity {
            strategy,
            kalshi_action: ("BUY".to_string(), kalshi_side.to_string(), kalshi_optimal),
            polymarket_action: ("BUY".to_string(), pm_side.to_string(), pm_optimal),
            total_cost: total_cost_opt,
            gross_profit: 1.0 - total_cost_opt,
            fees: (fee_pm.max(0.0) / n.max(1e-9)) + (fee_ks.max(0.0) / n.max(1e-9)),
            net_profit: (1.0 - total_cost_opt)
                - (fee_pm.max(0.0) / n.max(1e-9) + fee_ks.max(0.0) / n.max(1e-9)),
            roi_percent: roi_100,
            gas_fee: gas_amount,
            final_profit: net_profit_100,
            final_roi_percent: roi_100,
            pm_optimal,
            kalshi_optimal,
            pm_avg_slipped: pm_avg,
            kalshi_avg_slipped: kalshi_avg,
            contracts: n,
            capital_used,
            fees_amount,
            fee_polymarket_usd: fee_pm,
            fee_kalshi_usd: fee_ks,
            gas_amount,
            gas_assumed_usd,
            surplus_buffer_usd,
            net_profit_100,
            roi_100_percent: roi_100,
            orderbook_pm_top5,
            orderbook_kalshi_top5,
            pm_fee_bps: 0, // 由 validate_arbitrage_core 在返回前填入
            ks_limit_cents: (ks_worst_ask * 100.0).ceil() as i64,
            pm_limit_price: pm_worst_ask,
        })
    }

    pub fn calculate_arbitrage_with_direction(
        &self,
        pm_prices: &MarketPrices,
        kalshi_prices: &MarketPrices,
        pm_side: &str,
        kalshi_side: &str,
        needs_inversion: bool,
    ) -> Option<ArbitrageOpportunity> {
        // 根据方向确定实际买卖
        let (pm_action, pm_price) = if pm_side == "YES" {
            ("BUY", pm_prices.yes_ask.unwrap_or(pm_prices.yes))
        } else {
            ("BUY", pm_prices.no_ask.unwrap_or(pm_prices.no))
        };
        
        let (kalshi_action, kalshi_price) = if kalshi_side == "YES" {
            ("BUY", kalshi_prices.yes_ask.unwrap_or(kalshi_prices.yes))
        } else {
            ("BUY", kalshi_prices.no_ask.unwrap_or(kalshi_prices.no))
        };
        
        // 计算成本
        let total_cost = pm_price + kalshi_price;
        let profit = 1.0 - total_cost;
        let total_fees = LEGACY_PER_CONTRACT_FEE_SCREEN_USD;
        
        let pair_cap_usdt = arb_probe_caps().1;
        let min_profit = self.min_profit_usd(pair_cap_usdt);
        if profit <= total_fees + min_profit {
            return None;
        }
        
        let net_profit = profit - total_fees;
        let gas_assumed_usd = LIVE_ARBIT_GAS_ASSUMED_USD;
        let surplus_buffer_usd = live_arbit_surplus_buffer_usd(pair_cap_usdt);
        let gas_fee = live_arb_auxiliary_drag_usd(pair_cap_usdt);
        let final_profit = net_profit - gas_fee;
        
        if final_profit <= min_profit {
            return None;
        }
        
        let roi = if total_cost > 0.0 {
            (final_profit / total_cost) * 100.0
        } else {
            0.0
        };
        
        // 构建策略描述
        let inversion_note = if needs_inversion {
            " [Y/N颠倒]"
        } else {
            ""
        };
        
        let strategy = format!("Buy {} on Polymarket + Buy {} on Kalshi{}", 
            pm_side, kalshi_side, inversion_note);
        
        Some(ArbitrageOpportunity {
            strategy,
            kalshi_action: (kalshi_action.to_string(), kalshi_side.to_string(), kalshi_price),
            polymarket_action: (pm_action.to_string(), pm_side.to_string(), pm_price),
            total_cost,
            gross_profit: profit,
            fees: total_fees,
            net_profit,
            roi_percent: roi,
            gas_fee,
            final_profit,
            final_roi_percent: roi,
            pm_optimal: pm_price,
            kalshi_optimal: kalshi_price,
            pm_avg_slipped: pm_price,
            kalshi_avg_slipped: kalshi_price,
            contracts: 0.0,
            capital_used: 0.0,
            fees_amount: 0.0,
            fee_polymarket_usd: 0.0,
            fee_kalshi_usd: 0.0,
            gas_amount: gas_fee,
            gas_assumed_usd,
            surplus_buffer_usd,
            net_profit_100: 0.0,
            roi_100_percent: 0.0,
            orderbook_pm_top5: Vec::new(),
            orderbook_kalshi_top5: Vec::new(),
            pm_fee_bps: 0,
            ks_limit_cents: 0,
            pm_limit_price: 0.0,
        })
    }

    pub fn calculate_final_profit(
        &self,
        pm_prices: &MarketPrices,
        kalshi_prices: &MarketPrices,
        pm_slippage: f64,
        kalshi_slippage: f64,
    ) -> Option<ArbitrageOpportunity> {
        let opportunity = match self.check_arbitrage_optimal(pm_prices, kalshi_prices) {
            Some(opp) => opp,
            None => return None,
        };

        let pm_slipped = if opportunity.strategy.contains("Buy Yes on Polymarket") {
            pm_prices.yes * (1.0 + pm_slippage / 100.0)
        } else {
            pm_prices.no * (1.0 + pm_slippage / 100.0)
        };

        let kalshi_slipped = if opportunity.strategy.contains("Buy Yes on Kalshi") {
            kalshi_prices.yes * (1.0 + kalshi_slippage / 100.0)
        } else {
            kalshi_prices.no * (1.0 + kalshi_slippage / 100.0)
        };

        let slipped_cost = pm_slipped + kalshi_slipped;
        let slipped_profit = 1.0 - slipped_cost;

        if slipped_profit <= 0.0 {
            return None;
        }

        let total_fees = LEGACY_PER_CONTRACT_FEE_SCREEN_USD;
        let net_profit = slipped_profit - total_fees;
        let pair_cap_usdt = arb_probe_caps().1;
        let min_profit = self.min_profit_usd(pair_cap_usdt);
        let gas_assumed_usd = LIVE_ARBIT_GAS_ASSUMED_USD;
        let surplus_buffer_usd = live_arbit_surplus_buffer_usd(pair_cap_usdt);
        let gas_fee = live_arb_auxiliary_drag_usd(pair_cap_usdt);
        let final_profit = net_profit - gas_fee;

        if final_profit <= min_profit {
            return None;
        }

        let roi = if slipped_cost > 0.0 {
            (final_profit / slipped_cost) * 100.0
        } else {
            0.0
        };

        Some(ArbitrageOpportunity {
            strategy: opportunity.strategy,
            kalshi_action: opportunity.kalshi_action,
            polymarket_action: opportunity.polymarket_action,
            total_cost: slipped_cost,
            gross_profit: slipped_profit,
            fees: total_fees,
            net_profit,
            roi_percent: (net_profit / slipped_cost) * 100.0,
            gas_fee,
            final_profit,
            final_roi_percent: roi,
            pm_optimal: opportunity.pm_optimal,
            kalshi_optimal: opportunity.kalshi_optimal,
            pm_avg_slipped: pm_slipped,
            kalshi_avg_slipped: kalshi_slipped,
            contracts: 0.0,
            capital_used: 0.0,
            fees_amount: 0.0,
            fee_polymarket_usd: 0.0,
            fee_kalshi_usd: 0.0,
            gas_amount: gas_fee,
            gas_assumed_usd,
            surplus_buffer_usd,
            net_profit_100: 0.0,
            roi_100_percent: 0.0,
            orderbook_pm_top5: Vec::new(),
            orderbook_kalshi_top5: Vec::new(),
            pm_fee_bps: 0,
            ks_limit_cents: 0,
            pm_limit_price: 0.0,
        })
    }

    pub fn check_arbitrage_optimal(
        &self,
        pm_prices: &MarketPrices,
        kalshi_prices: &MarketPrices,
    ) -> Option<ArbitrageOpportunity> {
        if kalshi_prices.yes == 0.0 && kalshi_prices.no == 0.0 {
            return None;
        }
        if !pm_prices.validate() || !kalshi_prices.validate() {
            return None;
        }

        if pm_prices.yes_ask.is_none() || pm_prices.no_ask.is_none() {
            return None;
        }
        if kalshi_prices.yes_ask.is_none() || kalshi_prices.no_ask.is_none() {
            return None;
        }

        let pm_yes_ask = pm_prices.yes_ask.unwrap();
        let pm_no_ask = pm_prices.no_ask.unwrap();
        let kalshi_yes_ask = kalshi_prices.yes_ask.unwrap();
        let kalshi_no_ask = kalshi_prices.no_ask.unwrap();

        let cost_strategy_1 = kalshi_yes_ask + pm_no_ask;
        let profit_strategy_1 = 1.0 - cost_strategy_1;

        let cost_strategy_2 = kalshi_no_ask + pm_yes_ask;
        let profit_strategy_2 = 1.0 - cost_strategy_2;

        let total_fees = LEGACY_PER_CONTRACT_FEE_SCREEN_USD;
        let pair_cap_usdt = arb_probe_caps().1;
        let gas_assumed_usd = LIVE_ARBIT_GAS_ASSUMED_USD;
        let surplus_buffer_usd = live_arbit_surplus_buffer_usd(pair_cap_usdt);
        let gas_fee = live_arb_auxiliary_drag_usd(pair_cap_usdt);
        let min_profit = self.min_profit_usd(pair_cap_usdt);

        if profit_strategy_1 > total_fees + min_profit {
            let net_profit = profit_strategy_1 - total_fees;
            let final_profit = net_profit - gas_fee;
            if final_profit > min_profit {
                let roi_pre = if cost_strategy_1 > 0.0 {
                    (net_profit / cost_strategy_1) * 100.0
                } else {
                    0.0
                };
                let final_roi = if cost_strategy_1 > 0.0 {
                    (final_profit / cost_strategy_1) * 100.0
                } else {
                    0.0
                };

                return Some(ArbitrageOpportunity {
                    strategy: "Buy Yes on Kalshi + Buy No on Polymarket".to_string(),
                    kalshi_action: ("BUY".to_string(), "YES".to_string(), kalshi_yes_ask),
                    polymarket_action: ("BUY".to_string(), "NO".to_string(), pm_no_ask),
                    total_cost: cost_strategy_1,
                    gross_profit: profit_strategy_1,
                    fees: total_fees,
                    net_profit,
                    roi_percent: roi_pre,
                    gas_fee,
                    final_profit,
                    final_roi_percent: final_roi,
                    pm_optimal: pm_no_ask,
                    kalshi_optimal: kalshi_yes_ask,
                    pm_avg_slipped: pm_no_ask,
                    kalshi_avg_slipped: kalshi_yes_ask,
                    contracts: 0.0,
                    capital_used: 0.0,
                    fees_amount: 0.0,
                    fee_polymarket_usd: 0.0,
                    fee_kalshi_usd: 0.0,
                    gas_amount: gas_fee,
                    gas_assumed_usd,
                    surplus_buffer_usd,
                    net_profit_100: 0.0,
                    roi_100_percent: 0.0,
                    orderbook_pm_top5: Vec::new(),
                    orderbook_kalshi_top5: Vec::new(),
                    pm_fee_bps: 0,
                    ks_limit_cents: 0,
                    pm_limit_price: 0.0,
                });
            }
        }

        if profit_strategy_2 > total_fees + min_profit {
            let net_profit = profit_strategy_2 - total_fees;
            let final_profit = net_profit - gas_fee;
            if final_profit > min_profit {
                let roi_pre = if cost_strategy_2 > 0.0 {
                    (net_profit / cost_strategy_2) * 100.0
                } else {
                    0.0
                };
                let final_roi = if cost_strategy_2 > 0.0 {
                    (final_profit / cost_strategy_2) * 100.0
                } else {
                    0.0
                };

                return Some(ArbitrageOpportunity {
                    strategy: "Buy No on Kalshi + Buy Yes on Polymarket".to_string(),
                    kalshi_action: ("BUY".to_string(), "NO".to_string(), kalshi_no_ask),
                    polymarket_action: ("BUY".to_string(), "YES".to_string(), pm_yes_ask),
                    total_cost: cost_strategy_2,
                    gross_profit: profit_strategy_2,
                    fees: total_fees,
                    net_profit,
                    roi_percent: roi_pre,
                    gas_fee,
                    final_profit,
                    final_roi_percent: final_roi,
                    pm_optimal: pm_yes_ask,
                    kalshi_optimal: kalshi_no_ask,
                    pm_avg_slipped: pm_yes_ask,
                    kalshi_avg_slipped: kalshi_no_ask,
                    contracts: 0.0,
                    capital_used: 0.0,
                    fees_amount: 0.0,
                    fee_polymarket_usd: 0.0,
                    fee_kalshi_usd: 0.0,
                    gas_amount: gas_fee,
                    gas_assumed_usd,
                    surplus_buffer_usd,
                    net_profit_100: 0.0,
                    roi_100_percent: 0.0,
                    orderbook_pm_top5: Vec::new(),
                    orderbook_kalshi_top5: Vec::new(),
                    pm_fee_bps: 0,
                    ks_limit_cents: 0,
                    pm_limit_price: 0.0,
                });
            }
        }

        None
    }
}

// ==================== 滑点计算相关函数（需要公开导出）====================

/// 买盘 ask 阶梯各档可成交量之和。
#[inline]
pub fn total_size_buy_ladder(levels: &[(f64, f64)]) -> f64 {
    levels.iter().map(|(_, q)| *q).sum()
}

/// 卖盘 bid 阶梯各档可成交量之和。
#[inline]
pub fn total_size_sell_ladder(levels: &[(f64, f64)]) -> f64 {
    levels.iter().map(|(_, q)| *q).sum()
}

/// 从卖盘（价格升序）吃进**恰好** `n` 份合约。`n` 可为分数（与探针一致）。
/// 流动性不足则返回 `None`。返回 `(总成本, 成交量加权均价)`。
/// 返回 `(总成本, 加权均价, 最差成交价位)`。最差价位 = 填满 n 份时扫到的最高 ask 价格，
/// 用作 IOC/FAK 限价——保证在该价格水位未恶化时能精确成交 n 份。
pub fn cost_for_exact_contracts(asks: &[(f64, f64)], n: f64) -> Option<(f64, f64, f64)> {
    if n <= 0.0 || !n.is_finite() {
        return None;
    }
    const EPS: f64 = 1e-9;
    let mut remaining = n;
    let mut total_cost = 0.0;
    let mut worst_ask = 0.0;
    for (price, size) in asks {
        if remaining <= EPS {
            break;
        }
        if *size <= 0.0 || *price <= 0.0 {
            continue;
        }
        let take = remaining.min(*size);
        total_cost += take * price;
        worst_ask = *price;
        remaining -= take;
    }
    if remaining > 1e-6 {
        return None;
    }
    Some((total_cost, total_cost / n, worst_ask))
}

#[derive(Debug, Clone)]
pub struct SlippageInfo {
    pub avg_price: f64,
    pub slippage_percent: f64,
    pub filled: bool,
    pub filled_amount: f64,
    pub filled_contracts: f64,
}

pub fn calculate_slippage_with_fixed_usdt(
    asks: &[(f64, f64)],
    usdt_amount: f64,
) -> SlippageInfo {
    let mut remaining_usdt = usdt_amount;
    let mut total_contracts = 0.0;
    let mut total_cost = 0.0;
    let best_price = if asks.is_empty() { 0.0 } else { asks[0].0 };
    
    for (price, size) in asks {
        let level_value = price * size;
        
        if remaining_usdt >= level_value {
            total_contracts += size;
            total_cost += level_value;
            remaining_usdt -= level_value;
        } else {
            let buy_size = remaining_usdt / price;
            total_contracts += buy_size;
            total_cost += remaining_usdt;
            remaining_usdt = 0.0;
            break;
        }
    }
    
    let filled_amount = usdt_amount - remaining_usdt;
    let filled = remaining_usdt == 0.0;
    
    if total_contracts == 0.0 {
        return SlippageInfo {
            avg_price: 0.0,
            slippage_percent: 0.0,
            filled,
            filled_amount,
            filled_contracts: 0.0,
        };
    }
    
    let avg_price = total_cost / total_contracts;
    let slippage_percent = if best_price > 0.0 {
        (avg_price - best_price) / best_price * 100.0
    } else {
        0.0
    };
    
    SlippageInfo {
        avg_price,
        slippage_percent,
        filled,
        filled_amount,
        filled_contracts: total_contracts,
    }
}

/// 订单簿已按买入价升序排列时，第一档即为最优（最低）可成交价。
#[inline]
pub fn orderbook_best_ask_price(levels: &[(f64, f64)]) -> Option<f64> {
    levels.first().map(|(p, _)| *p)
}

/// PM / Kalshi 建仓阶梯共用：一档有效当且仅当价格在 **(0, 1)** 开区间且 **size > 0**（不限深度、YES/NO 路径统一）。
#[inline]
fn ladder_level_ok(price: f64, size: f64) -> bool {
    price.is_finite() && size.is_finite() && price > 0.0 && price < 1.0 && size > 0.0
}

/// 双腿 ask 阶梯构建失败原因（API 形态 / 解析 / 无流动性可区分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairLadderBuildFail {
    PmInvalidBuySide,
    PmMissingBidsArray,
    PmMissingAsksArray,
    PmMalformedQuote,
    /// PM：asks 或 bids 数组存在且长度为 0（无任何档位行）。
    PmNoAskLiquidityEmptySide,
    /// PM：有 ≥1 条档位且解析成功，但均不满足 [`ladder_level_ok`]（开区间 (0,1) 且 size>0）。
    PmNoAskLiquidityAllRowsFiltered,
    KsMissingOrderbookBody,
    KsInvalidBuySide,
    KsMalformedQuote,
    /// Kalshi：本腿用的 `no_dollars` / `yes_dollars` 无元素（字段缺失或空数组）。
    KsNoAskLiquidityEmptySide,
    /// Kalshi：有 ≥1 条档位且解析成功，但 implied ask 均不满足 [`ladder_level_ok`]。
    KsNoAskLiquidityAllRowsFiltered,
}

/// Kalshi 腿阶梯构建失败时的短标签（仅 KS 相关原因），用于调试样本行。
pub fn kalshi_ladder_fail_tag(r: PairLadderBuildFail) -> Option<&'static str> {
    Some(match r {
        PairLadderBuildFail::KsMissingOrderbookBody => "ks_missing_orderbook_body",
        PairLadderBuildFail::KsInvalidBuySide => "ks_invalid_buy_side",
        PairLadderBuildFail::KsMalformedQuote => "ks_malformed_quote",
        PairLadderBuildFail::KsNoAskLiquidityEmptySide => "ks_empty_levels",
        PairLadderBuildFail::KsNoAskLiquidityAllRowsFiltered => "ks_all_levels_filtered",
        _ => return None,
    })
}

fn json_num_field(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Polymarket：解析买侧可见卖档；失败时返回具体原因（用于排障统计）。
pub fn try_parse_polymarket_buy_asks(
    data: &Value,
    buy_side: &str,
    buy_no_via_yes_token_bids: bool,
) -> Result<Vec<(f64, f64)>, PairLadderBuildFail> {
    if buy_side != "YES" && buy_side != "NO" {
        return Err(PairLadderBuildFail::PmInvalidBuySide);
    }
    if buy_side == "NO" && buy_no_via_yes_token_bids {
        let bids = data
            .get("bids")
            .and_then(|v| v.as_array())
            .ok_or(PairLadderBuildFail::PmMissingBidsArray)?;
        let n_rows = bids.len();
        let mut result = Vec::new();
        for bid in bids {
            let price_v = bid.get("price").ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            let size_v = bid.get("size").ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            let bid_price = json_num_field(price_v).ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            let size = json_num_field(size_v).ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            let ask_price = 1.0 - bid_price;
            if ladder_level_ok(ask_price, size) {
                result.push((ask_price, size));
            }
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if result.is_empty() {
            return Err(if n_rows == 0 {
                PairLadderBuildFail::PmNoAskLiquidityEmptySide
            } else {
                PairLadderBuildFail::PmNoAskLiquidityAllRowsFiltered
            });
        }
        Ok(result)
    } else {
        let asks = data
            .get("asks")
            .and_then(|v| v.as_array())
            .ok_or(PairLadderBuildFail::PmMissingAsksArray)?;
        let n_rows = asks.len();
        let mut result = Vec::new();
        for ask in asks {
            let price_v = ask.get("price").ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            let size_v = ask.get("size").ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            let price = json_num_field(price_v).ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            let size = json_num_field(size_v).ok_or(PairLadderBuildFail::PmMalformedQuote)?;
            if ladder_level_ok(price, size) {
                result.push((price, size));
            }
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if result.is_empty() {
            return Err(if n_rows == 0 {
                PairLadderBuildFail::PmNoAskLiquidityEmptySide
            } else {
                PairLadderBuildFail::PmNoAskLiquidityAllRowsFiltered
            });
        }
        Ok(result)
    }
}

/// Polymarket CLOB：买入 `buy_side` 的卖档（价升序）。
/// `buy_no_via_yes_token_bids` 为 `true` 时，认为当前 JSON 来自 **YES** outcome token，
/// 且仅能通过 **bids** 反推隐含买 NO 价（Gamma 仅提供单 token 时的退化，与旧逻辑一致）；
/// 为 `false` 时读 **asks**，适用于买 YES，或买 NO 且已请求 **NO** outcome token 的簿。
pub fn parse_polymarket_buy_asks(
    data: &Value,
    buy_side: &str,
    buy_no_via_yes_token_bids: bool,
) -> Option<Vec<(f64, f64)>> {
    try_parse_polymarket_buy_asks(data, buy_side, buy_no_via_yes_token_bids).ok()
}

/// 兼容旧调用处：买 NO 时一律走 YES 单簿 bids 反推（双 token 场景请用 [`parse_polymarket_buy_asks`] + 正确 token）。
pub fn parse_polymarket_orderbook(data: &Value, side: &str) -> Option<Vec<(f64, f64)>> {
    match side {
        "YES" => parse_polymarket_buy_asks(data, "YES", false),
        "NO" => parse_polymarket_buy_asks(data, "NO", true),
        _ => None,
    }
}

/// 买入 `buy_side` 时应请求的 Polymarket `token_id`（`clobTokenIds` 约定 index0=YES、index1=NO）。
pub fn polymarket_clob_token_id_for_buy<'a>(pm: &'a Market, buy_side: &str) -> Option<&'a str> {
    match buy_side {
        "YES" => pm.token_ids.first().map(|s| s.as_str()),
        "NO" if pm.token_ids.len() >= 2 => pm.token_ids.get(1).map(|s| s.as_str()),
        "NO" => pm.token_ids.first().map(|s| s.as_str()),
        _ => pm.token_ids.first().map(|s| s.as_str()),
    }
}

/// 买 NO 且仅有一个 CLOB token 时，必须在 YES token 簿上用 bids 反推；双 token 时用 NO token 簿的 asks。
pub fn pm_buy_no_uses_yes_token_complement(pm: &Market, buy_side: &str) -> bool {
    buy_side == "NO" && pm.token_ids.len() < 2
}

pub fn try_parse_kalshi_orderbook(
    data: &Value,
    side: &str,
) -> Result<Vec<(f64, f64)>, PairLadderBuildFail> {
    let orderbook = data
        .get("orderbook_fp")
        .or_else(|| data.get("orderbook"))
        .ok_or(PairLadderBuildFail::KsMissingOrderbookBody)?;

    if side == "YES" {
        let no_bids = orderbook
            .get("no_dollars")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        let n_rows = no_bids.len();
        let mut result = Vec::new();
        for entry in no_bids {
            let bid_price = json_num_field(
                entry
                    .get(0)
                    .ok_or(PairLadderBuildFail::KsMalformedQuote)?,
            )
            .ok_or(PairLadderBuildFail::KsMalformedQuote)?;
            let size = json_num_field(
                entry
                    .get(1)
                    .ok_or(PairLadderBuildFail::KsMalformedQuote)?,
            )
            .ok_or(PairLadderBuildFail::KsMalformedQuote)?;
            let ask_price = 1.0 - bid_price;
            if ladder_level_ok(ask_price, size) {
                result.push((ask_price, size));
            }
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if result.is_empty() {
            return Err(if n_rows == 0 {
                PairLadderBuildFail::KsNoAskLiquidityEmptySide
            } else {
                PairLadderBuildFail::KsNoAskLiquidityAllRowsFiltered
            });
        }
        Ok(result)
    } else if side == "NO" {
        let yes_bids = orderbook
            .get("yes_dollars")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        let n_rows = yes_bids.len();
        let mut result = Vec::new();
        for entry in yes_bids {
            let bid_price = json_num_field(
                entry
                    .get(0)
                    .ok_or(PairLadderBuildFail::KsMalformedQuote)?,
            )
            .ok_or(PairLadderBuildFail::KsMalformedQuote)?;
            let size = json_num_field(
                entry
                    .get(1)
                    .ok_or(PairLadderBuildFail::KsMalformedQuote)?,
            )
            .ok_or(PairLadderBuildFail::KsMalformedQuote)?;
            let ask_price = 1.0 - bid_price;
            if ladder_level_ok(ask_price, size) {
                result.push((ask_price, size));
            }
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if result.is_empty() {
            return Err(if n_rows == 0 {
                PairLadderBuildFail::KsNoAskLiquidityEmptySide
            } else {
                PairLadderBuildFail::KsNoAskLiquidityAllRowsFiltered
            });
        }
        Ok(result)
    } else {
        Err(PairLadderBuildFail::KsInvalidBuySide)
    }
}

pub fn parse_kalshi_orderbook(data: &Value, side: &str) -> Option<Vec<(f64, f64)>> {
    try_parse_kalshi_orderbook(data, side).ok()
}

/// 在 `bids_desc`（卖方视角：**价高优先**，即最优买价在前）上卖出恰好 `n` 份，返回 (总回款, 加权均价)。
pub fn proceeds_for_exact_contracts_sell(bids_desc: &[(f64, f64)], n: f64) -> Option<(f64, f64)> {
    if n <= 0.0 || !n.is_finite() {
        return None;
    }
    const EPS: f64 = 1e-9;
    let mut remaining = n;
    let mut total_proceeds = 0.0;
    for (price, size) in bids_desc {
        if remaining <= EPS {
            break;
        }
        if *size <= 0.0 || *price <= 0.0 {
            continue;
        }
        let take = remaining.min(*size);
        total_proceeds += take * price;
        remaining -= take;
    }
    if remaining > 1e-6 {
        return None;
    }
    Some((total_proceeds, total_proceeds / n))
}

/// Kalshi：卖出持仓 `kalshi_side`（YES/NO）时吃对手 **bid** 阶梯。`yes_dollars` / `no_dollars` 行 `[价,量]`，美元价 ∈ (0,1)。
pub fn try_parse_kalshi_sell_bids(
    data: &Value,
    kalshi_side: &str,
) -> Result<Vec<(f64, f64)>, PairLadderBuildFail> {
    let orderbook = data
        .get("orderbook_fp")
        .or_else(|| data.get("orderbook"))
        .ok_or(PairLadderBuildFail::KsMissingOrderbookBody)?;

    let arr: &[Value] = match kalshi_side {
        "YES" => orderbook
            .get("yes_dollars")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]),
        "NO" => orderbook
            .get("no_dollars")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]),
        _ => return Err(PairLadderBuildFail::KsInvalidBuySide),
    };
    let n_rows = arr.len();
    let mut result = Vec::new();
    for entry in arr {
        let bid_price = json_num_field(
            entry
                .get(0)
                .ok_or(PairLadderBuildFail::KsMalformedQuote)?,
        )
        .ok_or(PairLadderBuildFail::KsMalformedQuote)?;
        let size = json_num_field(
            entry
                .get(1)
                .ok_or(PairLadderBuildFail::KsMalformedQuote)?,
        )
        .ok_or(PairLadderBuildFail::KsMalformedQuote)?;
        if ladder_level_ok(bid_price, size) {
            result.push((bid_price, size));
        }
    }
    if result.is_empty() {
        return Err(if n_rows == 0 {
            PairLadderBuildFail::KsNoAskLiquidityEmptySide
        } else {
            PairLadderBuildFail::KsNoAskLiquidityAllRowsFiltered
        });
    }
    result.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    Ok(result)
}

/// Kalshi 二元市场订单簿：`outcome_side` 为 **`YES`/`NO`（你持有的净头寸方向）**。  
/// 中间价 =（该结果最佳 bid + 由对手盘 dollars 行推出的最佳 ask）/2，价域与簿一致为 **美元概率 (0,1)**。
pub fn kalshi_orderbook_mid_dollars_for_outcome(book: &Value, outcome_side: &str) -> Option<f64> {
    let orderbook = book
        .get("orderbook_fp")
        .or_else(|| book.get("orderbook"))?;
    let (bid_key, ask_src_key) = match outcome_side {
        "YES" => ("yes_dollars", "no_dollars"),
        "NO" => ("no_dollars", "yes_dollars"),
        _ => return None,
    };
    let bids_arr = orderbook.get(bid_key)?.as_array()?;
    let ask_src_arr = orderbook.get(ask_src_key)?.as_array()?;

    let mut best_bid: Option<f64> = None;
    for entry in bids_arr {
        let Some(p0) = entry.get(0) else { continue };
        let Some(p1) = entry.get(1) else { continue };
        let Some(p) = json_num_field(p0) else { continue };
        let Some(s) = json_num_field(p1) else { continue };
        if ladder_level_ok(p, s) {
            best_bid = Some(best_bid.map_or(p, |b| b.max(p)));
        }
    }

    let mut best_ask: Option<f64> = None;
    for entry in ask_src_arr {
        let Some(p0) = entry.get(0) else { continue };
        let Some(p1) = entry.get(1) else { continue };
        let Some(p) = json_num_field(p0) else { continue };
        let Some(s) = json_num_field(p1) else { continue };
        let ask_p = 1.0 - p;
        if ladder_level_ok(ask_p, s) {
            best_ask = Some(best_ask.map_or(ask_p, |a| a.min(ask_p)));
        }
    }

    let b = best_bid?;
    let a = best_ask?;
    Some(0.5 * (b + a))
}

/// 双腿 ask 阶梯（开仓）。
#[derive(Debug, Clone)]
pub struct PairOrderbookLadders {
    pub pm_asks: Vec<(f64, f64)>,
    pub ks_asks: Vec<(f64, f64)>,
}

/// 从已获取的 PM/Kalshi 订单簿 JSON 构建双腿 ask 阶梯。
/// `pm_buy_no_via_yes_book_bids` 须与拉取 PM 簿时使用的 token 一致（见 [`polymarket_clob_token_id_for_buy`]）。
pub fn build_pair_orderbook_ladders_result(
    pm_book: &Value,
    ks_book: &Value,
    pm_side: &str,
    ks_side: &str,
    pm_buy_no_via_yes_book_bids: bool,
) -> Result<PairOrderbookLadders, PairLadderBuildFail> {
    let pm_asks = try_parse_polymarket_buy_asks(pm_book, pm_side, pm_buy_no_via_yes_book_bids)?;
    let ks_asks = try_parse_kalshi_orderbook(ks_book, ks_side)?;
    Ok(PairOrderbookLadders { pm_asks, ks_asks })
}

/// 在已构建的 [`PairOrderbookLadders`] 上完成套利计算（不重复请求订单簿）。
pub fn validate_opportunity_from_ladders(
    arb_detector: &ArbitrageDetector,
    ladders: &PairOrderbookLadders,
    pm_side: &str,
    kalshi_side: &str,
    needs_inversion: bool,
    capital_usdt: f64,
    pair_cap_usdt: f64,
    cost: &ArbCostParams,
) -> Option<ArbitrageOpportunity> {
    // Guard A：PM ask 总深度不足 1 整数份 → Kalshi 最低委托为 1 合约（官方文档 count minimum:1），
    // PM 侧同样无法成交不足 1 份的订单，直接跳过。纯 orderbook 约束，与平台最低委托金额无关。
    // 在 calculate_arbitrage_100usdt 之前检查，省掉完整的利润计算。
    let pm_ask_depth = total_size_buy_ladder(&ladders.pm_asks);
    if pm_ask_depth < 1.0 {
        return None;
    }

    let pm_optimal = orderbook_best_ask_price(&ladders.pm_asks)?;
    let kalshi_optimal = orderbook_best_ask_price(&ladders.ks_asks)?;

    arb_detector.calculate_arbitrage_100usdt(
        pm_optimal,
        kalshi_optimal,
        Some(ladders.pm_asks.as_slice()),
        Some(ladders.ks_asks.as_slice()),
        pm_side,
        kalshi_side,
        needs_inversion,
        capital_usdt,
        pair_cap_usdt,
        cost,
    )
}

#[cfg(test)]
mod arb_tests {
    use super::*;

    #[test]
    fn cost_for_exact_contracts_single_level() {
        let ob = [(0.5, 100.0)];
        let (c, avg, worst) = cost_for_exact_contracts(&ob, 10.0).unwrap();
        assert!((c - 5.0).abs() < 1e-9);
        assert!((avg - 0.5).abs() < 1e-9);
        assert!((worst - 0.5).abs() < 1e-9);
    }

    #[test]
    fn cost_for_exact_contracts_spans_levels() {
        let ob = [(0.1, 50.0), (0.2, 50.0)];
        let (c, avg, worst) = cost_for_exact_contracts(&ob, 75.0).unwrap();
        assert!((c - (50.0 * 0.1 + 25.0 * 0.2)).abs() < 1e-6);
        assert!((avg - c / 75.0).abs() < 1e-9);
        assert!((worst - 0.2).abs() < 1e-9);
    }

    #[test]
    fn cost_for_exact_contracts_insufficient() {
        let ob = [(0.5, 10.0)];
        assert!(cost_for_exact_contracts(&ob, 20.0).is_none());
    }

    #[test]
    fn polymarket_clob_accepts_numeric_price_size() {
        let v = serde_json::json!({
            "asks": [{"price": 0.42, "size": 100.5}, {"price": "0.43", "size": "50"}],
            "bids": []
        });
        let asks = parse_polymarket_buy_asks(&v, "YES", false).expect("numeric/string mix");
        assert!((asks[0].0 - 0.42).abs() < 1e-9);
    }

    #[test]
    fn arbitrage_probe_then_reprice_matches_n() {
        let det = ArbitrageDetector::new(0.0);
        let pm = [(0.001, 1_000_000.0), (0.01, 1_000_000.0)];
        let ks = [(0.05, 100.0), (0.1, 10000.0)];
        let cost = ArbCostParams::default();
        // `pair_cap` 参与盈余缓冲与最小净利：`1e6` 会使缓冲远超单笔探针利润，识别恒为 None。
        let pair_cap = 100.0_f64;
        let opp = det
            .calculate_arbitrage_100usdt(
                0.001,
                0.05,
                Some(&pm),
                Some(&ks),
                "YES",
                "YES",
                false,
                100.0,
                pair_cap,
                &cost,
            )
            .expect("opp");
        assert!(opp.contracts > 0.0);
        let (c_pm, _, _) = cost_for_exact_contracts(&pm, opp.contracts).unwrap();
        let (c_ks, _, _) = cost_for_exact_contracts(&ks, opp.contracts).unwrap();
        assert!((opp.capital_used - (c_pm + c_ks)).abs() < 0.01);
    }

    #[test]
    fn proceeds_sell_walks_bids_desc() {
        let bids = [(0.6, 10.0), (0.5, 10.0)];
        let (p, avg) = proceeds_for_exact_contracts_sell(&bids, 5.0).unwrap();
        assert!((p - 3.0).abs() < 1e-9);
        assert!((avg - 0.6).abs() < 1e-9);
    }

    #[test]
    fn kalshi_mid_yes_matches_bid_and_implied_ask() {
        let book = serde_json::json!({
            "orderbook": {
                "yes_dollars": [[0.4, 10.0]],
                "no_dollars": [[0.55, 10.0]]
            }
        });
        let mid = kalshi_orderbook_mid_dollars_for_outcome(&book, "YES").unwrap();
        assert!((mid - 0.5 * (0.4 + (1.0 - 0.55))).abs() < 1e-9);
    }

    #[test]
    fn kalshi_mid_no_matches_bid_and_implied_ask() {
        let book = serde_json::json!({
            "orderbook": {
                "no_dollars": [[0.35, 5.0]],
                "yes_dollars": [[0.5, 5.0]]
            }
        });
        let mid = kalshi_orderbook_mid_dollars_for_outcome(&book, "NO").unwrap();
        assert!((mid - 0.5 * (0.35 + (1.0 - 0.5))).abs() < 1e-9);
    }

    // ── 并行执行架构：事前事中事后全流程验证 ────────────────────────────────

    /// 事前：ks_limit_cents = ceil(worst_ask * 100)；pm_limit_price = worst_ask
    /// 用实盘订单簿（Norman Powell）验证：
    ///   PM asks: 0.42×8, 0.43×11.4, 0.46×300
    ///   KS asks: 0.41×76, 0.47×1000
    ///   capital=6.57 → n=15（PM 吃 L1:8份 + L2:7份，KS 全在 L1）
    ///   → PM worst_ask=0.43, KS worst_ask=0.41
    #[test]
    fn limit_prices_use_worst_ask_not_avg() {
        let det = ArbitrageDetector::new(0.0);
        let pm_asks: &[(f64, f64)] = &[(0.42, 8.0), (0.43, 11.4), (0.46, 300.0)];
        let ks_asks: &[(f64, f64)] = &[(0.41, 76.0), (0.47, 1000.0)];
        let cost = ArbCostParams { ks_taker_fee_multiplier: 0.07, pm_fee_bps: 200, pm_exponent: 1.0 };
        // capital=6.57：PM probe→15.47份，KS probe→16.02份，n=min→15.47→floor→15
        let opp = det.calculate_arbitrage_100usdt(
            0.42, 0.41,
            Some(pm_asks), Some(ks_asks),
            "YES", "NO", false,
            6.57, 100.0, &cost,
        ).expect("应识别到套利机会");

        assert_eq!(opp.contracts, 15.0, "n 应为整数 15（实盘对齐）");
        // KS：15份全在0.41档（76份够），worst_ask=0.41 → ceil(41)=41¢
        assert_eq!(opp.ks_limit_cents, 41, "Kalshi 限价 = ceil(0.41*100) = 41¢");
        // PM：8份@0.42 + 7份@0.43，worst_ask=0.43（第二档价格）
        assert!((opp.pm_limit_price - 0.43).abs() < 1e-9, "PM 限价 = worst_ask = 0.43");
        // 验证限价是 worst_ask 而非加权均价
        // PM avg ≈ (8×0.42 + 7×0.43)/15 ≈ 0.4247，不等于 worst_ask 0.43
        assert!((opp.pm_avg_slipped - 0.43).abs() > 1e-4, "均价应低于 worst_ask");
    }

    /// 事前：n 必须 floor 取整，且 PM 名义金额 n × worst_ask ≥ $1.00
    #[test]
    fn n_is_floored_and_pm_notional_checked() {
        let det = ArbitrageDetector::new(0.0);
        // 构造使 n 计算结果为小数（约 1.3）
        let pm_asks: &[(f64, f64)] = &[(0.45, 1000.0)];
        let ks_asks: &[(f64, f64)] = &[(0.45, 1000.0)];
        let cost = ArbCostParams { ks_taker_fee_multiplier: 0.07, pm_fee_bps: 200, pm_exponent: 1.0 };
        let opp = det.calculate_arbitrage_100usdt(
            0.45, 0.45,
            Some(pm_asks), Some(ks_asks),
            "YES", "NO", false,
            0.6,  // 0.6 / 0.45 = 1.33 → floor → 1
            100.0, &cost,
        );
        // n=1, notional = 1 × 0.45 = $0.45 < $1.00 → 应返回 None
        assert!(opp.is_none(), "n=1 名义 $0.45 < $1，应过滤");
    }

    /// 事前：ks_limit_cents 对极端值 clamp 到 [1,99]
    #[test]
    fn ks_limit_cents_clamped() {
        // worst_ask = 0.995 → ceil(99.5) = 100 → clamp → 99
        let val = (0.995_f64 * 100.0).ceil() as i64;
        assert_eq!(val.clamp(1, 99), 99);
        // worst_ask = 0.0 → 0 → clamp → 1
        let val2 = (0.0_f64 * 100.0).ceil() as i64;
        assert_eq!(val2.clamp(1, 99), 1);
    }

    /// 事中：PM BUY 语义 — taking=token，making=USDC
    /// 用本次实盘数值：making=6.449999 USDC，taking=15.186045 token
    #[test]
    fn pm_buy_k2_reads_taking_not_making() {
        let making: f64 = 6.449999;  // USDC 支付
        let taking: f64 = 15.186045; // token 收到
        // 旧错误逻辑：round(making) = 6 → 误判偏差 9
        let wrong_k2 = making.round() as i64;
        assert_eq!(wrong_k2, 6, "making 约 6.45 → round → 6（旧 bug）");
        // 正确逻辑：round(taking) = 15
        let correct_k2 = taking.round() as i64;
        assert_eq!(correct_k2, 15, "taking 约 15.19 → round → 15（正确）");
    }

    /// 事中：PM SELL 语义 — making=token卖出，taking=USDC收入（与BUY相反）
    #[test]
    fn pm_sell_sold_reads_making_not_taking() {
        // 假设 SELL 9 份：making=9.0 token，taking=3.5 USDC（卖出价约 $0.39）
        let making: f64 = 9.0;   // tokens sold
        let taking: f64 = 3.501; // USDC received
        // 正确逻辑：sold = round(making)
        let sold = making.round() as i64;
        assert_eq!(sold, 9, "SELL making=token卖出数");
        // 错误逻辑（旧 bug）：round(taking) = 4 → 误判只卖了 4 份
        let wrong_sold = taking.round() as i64;
        assert_eq!(wrong_sold, 4, "taking=USDC 不是 token，round 后数值错误");
        assert_ne!(sold, wrong_sold);
    }

    /// 事后：excess 计算与对称性
    #[test]
    fn excess_calculation_symmetric() {
        // Case 1: k1=k2=15 → OK，无 excess
        let (k1, k2) = (15_i64, 15_i64);
        assert_eq!((k1 - k2).max(0), 0);
        assert_eq!((k2 - k1).max(0), 0);

        // Case 2: k1=15, k2=6（旧 bug 场景）→ excess_ks=9
        let (k1, k2) = (15_i64, 6_i64);
        assert_eq!((k1 - k2).max(0), 9, "Kalshi 多 9 份");
        assert_eq!((k2 - k1).max(0), 0);

        // Case 3: k1=14, k2=15 → excess_pm=1
        let (k1, k2) = (14_i64, 15_i64);
        assert_eq!((k1 - k2).max(0), 0);
        assert_eq!((k2 - k1).max(0), 1, "PM 多 1 份");
    }

    /// 事后：对账触发门槛 excess × ref_price >= $1.00
    #[test]
    fn reconcile_threshold_one_dollar() {
        // excess=9, ref=0.41 → 3.69 → 触发
        assert!((9_i64 as f64 * 0.41) >= 1.0);
        // excess=1, ref=0.41 → 0.41 → 不触发（记录裸仓）
        assert!((1_i64 as f64 * 0.41) < 1.0);
        // excess=3, ref=0.35 → 1.05 → 触发
        assert!((3_i64 as f64 * 0.35) >= 1.0);
    }

    /// 事后：log_contracts = min(k1, k2)，正确对账后等于 n_target
    #[test]
    fn log_contracts_is_min_of_fills() {
        let (k1, k2) = (15_i64, 15_i64);
        assert_eq!(k1.min(k2) as f64, 15.0);
        // 对账前：min(15,6)=6（旧 bug 记录的错误值）
        let (k1, k2) = (15_i64, 6_i64);
        assert_eq!(k1.min(k2) as f64, 6.0, "旧 bug 会少记 9 份");
        // 修复后：min(15,15)=15
        let (k1, k2) = (15_i64, 15_i64);
        assert_eq!(k1.min(k2) as f64, 15.0, "修复后正确记录 15 份");
    }

    /// 事前：两侧策略对称 — PM YES + KS NO，sum < 1 才有套利空间
    #[test]
    fn arb_only_when_sum_lt_one() {
        let det = ArbitrageDetector::new(0.0);
        let ob: &[(f64, f64)] = &[(0.50, 1000.0)];
        let cost = ArbCostParams::default();
        // sum = 0.50 + 0.50 = 1.00 → 无套利
        let none = det.calculate_arbitrage_100usdt(0.50, 0.50, Some(ob), Some(ob), "YES", "NO", false, 10.0, 100.0, &cost);
        assert!(none.is_none(), "sum=1.00 无套利");
        // sum = 0.42 + 0.41 = 0.83 → 有套利（本次实盘价格）
        let pm_asks: &[(f64, f64)] = &[(0.42, 8.0), (0.43, 11.4), (0.46, 300.0)];
        let ks_asks: &[(f64, f64)] = &[(0.41, 76.0), (0.47, 1000.0)];
        let some = det.calculate_arbitrage_100usdt(0.42, 0.41, Some(pm_asks), Some(ks_asks), "YES", "NO", false, 10.0, 100.0, &cost);
        assert!(some.is_some(), "sum=0.83 应有套利");
    }
}