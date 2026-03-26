// src/arbitrage_detector.rs
use crate::market::MarketPrices;
use serde_json::Value;

/// Gas 费配置（固定值，单位 USDT）
pub const GAS_FEE_PER_TX: f64 = 0.02;  // 每笔交易 $0.02，两腿共 $0.04

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
    pub fees_amount: f64,
    pub gas_amount: f64,
    pub net_profit_100: f64,
    pub roi_100_percent: f64,
    pub orderbook_pm_top5: Vec<(f64, f64)>,
    pub orderbook_kalshi_top5: Vec<(f64, f64)>,
}

#[derive(Debug, Clone)]
pub struct Fees {
    pub polymarket: f64,
    pub kalshi: f64,
}

impl Default for Fees {
    fn default() -> Self {
        Self {
            polymarket: 0.01,
            kalshi: 0.01,
        }
    }
}

pub struct ArbitrageDetector {
    min_profit_threshold: f64,
    fees: Fees,
}

impl ArbitrageDetector {
    pub fn new(min_profit_threshold: f64) -> Self {
        Self {
            min_profit_threshold,
            fees: Fees::default(),
        }
    }

    pub fn with_fees(mut self, fees: Fees) -> Self {
        self.fees = fees;
        self
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

        // 以 n 为基准重算双腿真实成本（滑点已体现在成交价路径上）
        let (mut c_pm, mut pm_avg) = if let Some(ob) = pm_orderbook {
            cost_for_exact_contracts(ob, n)?
        } else {
            let c = n * pm_optimal;
            (c, pm_optimal)
        };
        let (mut c_ks, mut kalshi_avg) = if let Some(ob) = kalshi_orderbook {
            cost_for_exact_contracts(ob, n)?
        } else {
            let c = n * kalshi_optimal;
            (c, kalshi_optimal)
        };

        let mut capital_used = c_pm + c_ks;
        if pair_cap_usdt > 0.0 && capital_used > pair_cap_usdt {
            let scale = pair_cap_usdt / capital_used;
            n *= scale;
            if n <= 0.0 || !n.is_finite() {
                return None;
            }
            let (c1, a1) = if let Some(ob) = pm_orderbook {
                cost_for_exact_contracts(ob, n)?
            } else {
                (n * pm_optimal, pm_optimal)
            };
            let (c2, a2) = if let Some(ob) = kalshi_orderbook {
                cost_for_exact_contracts(ob, n)?
            } else {
                (n * kalshi_optimal, kalshi_optimal)
            };
            c_pm = c1;
            pm_avg = a1;
            c_ks = c2;
            kalshi_avg = a2;
            capital_used = c_pm + c_ks;
        }
        let gross = n * 1.0;
        let fees_amount =
            c_pm * self.fees.polymarket + c_ks * self.fees.kalshi;
        let gas_amount = GAS_FEE_PER_TX * 2.0;
        let net_profit_100 = gross - capital_used - fees_amount - gas_amount;

        if net_profit_100 <= self.min_profit_threshold {
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
            fees: self.fees.polymarket + self.fees.kalshi,
            net_profit: (1.0 - total_cost_opt) - (self.fees.polymarket + self.fees.kalshi),
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
            gas_amount,
            net_profit_100,
            roi_100_percent: roi_100,
            orderbook_pm_top5,
            orderbook_kalshi_top5,
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
        let total_fees = self.fees.polymarket + self.fees.kalshi;
        
        if profit <= total_fees + self.min_profit_threshold {
            return None;
        }
        
        let net_profit = profit - total_fees;
        let final_profit = net_profit - GAS_FEE_PER_TX * 2.0;
        
        if final_profit <= self.min_profit_threshold {
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
            gas_fee: GAS_FEE_PER_TX * 2.0,
            final_profit,
            final_roi_percent: roi,
            pm_optimal: pm_price,
            kalshi_optimal: kalshi_price,
            pm_avg_slipped: pm_price,
            kalshi_avg_slipped: kalshi_price,
            contracts: 0.0,
            capital_used: 0.0,
            fees_amount: 0.0,
            gas_amount: 0.0,
            net_profit_100: 0.0,
            roi_100_percent: 0.0,
            orderbook_pm_top5: Vec::new(),
            orderbook_kalshi_top5: Vec::new(),
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

        let total_fees = self.fees.polymarket + self.fees.kalshi;
        let net_profit = slipped_profit - total_fees;
        let final_profit = net_profit - GAS_FEE_PER_TX * 2.0;

        if final_profit <= self.min_profit_threshold {
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
            gas_fee: GAS_FEE_PER_TX * 2.0,
            final_profit,
            final_roi_percent: roi,
            pm_optimal: opportunity.pm_optimal,
            kalshi_optimal: opportunity.kalshi_optimal,
            pm_avg_slipped: pm_slipped,
            kalshi_avg_slipped: kalshi_slipped,
            contracts: 0.0,
            capital_used: 0.0,
            fees_amount: 0.0,
            gas_amount: 0.0,
            net_profit_100: 0.0,
            roi_100_percent: 0.0,
            orderbook_pm_top5: Vec::new(),
            orderbook_kalshi_top5: Vec::new(),
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

        let total_fees = self.fees.polymarket + self.fees.kalshi;

        if profit_strategy_1 > total_fees + self.min_profit_threshold {
            let net_profit = profit_strategy_1 - total_fees;
            let roi = if cost_strategy_1 > 0.0 {
                (net_profit / cost_strategy_1) * 100.0
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
                roi_percent: roi,
                gas_fee: 0.0,
                final_profit: 0.0,
                final_roi_percent: 0.0,
                pm_optimal: pm_no_ask,
                kalshi_optimal: kalshi_yes_ask,
                pm_avg_slipped: pm_no_ask,
                kalshi_avg_slipped: kalshi_yes_ask,
                contracts: 0.0,
                capital_used: 0.0,
                fees_amount: 0.0,
                gas_amount: 0.0,
                net_profit_100: 0.0,
                roi_100_percent: 0.0,
                orderbook_pm_top5: Vec::new(),
                orderbook_kalshi_top5: Vec::new(),
            });
        }

        if profit_strategy_2 > total_fees + self.min_profit_threshold {
            let net_profit = profit_strategy_2 - total_fees;
            let roi = if cost_strategy_2 > 0.0 {
                (net_profit / cost_strategy_2) * 100.0
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
                roi_percent: roi,
                gas_fee: 0.0,
                final_profit: 0.0,
                final_roi_percent: 0.0,
                pm_optimal: pm_yes_ask,
                kalshi_optimal: kalshi_no_ask,
                pm_avg_slipped: pm_yes_ask,
                kalshi_avg_slipped: kalshi_no_ask,
                contracts: 0.0,
                capital_used: 0.0,
                fees_amount: 0.0,
                gas_amount: 0.0,
                net_profit_100: 0.0,
                roi_100_percent: 0.0,
                orderbook_pm_top5: Vec::new(),
                orderbook_kalshi_top5: Vec::new(),
            });
        }

        None
    }
}

// ==================== 滑点计算相关函数（需要公开导出）====================

/// 从卖盘（价格升序）吃进**恰好** `n` 份合约。`n` 可为分数（与探针一致）。
/// 流动性不足则返回 `None`。返回 `(总成本, 成交量加权均价)`。
pub fn cost_for_exact_contracts(asks: &[(f64, f64)], n: f64) -> Option<(f64, f64)> {
    if n <= 0.0 || !n.is_finite() {
        return None;
    }
    const EPS: f64 = 1e-9;
    let mut remaining = n;
    let mut total_cost = 0.0;
    for (price, size) in asks {
        if remaining <= EPS {
            break;
        }
        if *size <= 0.0 || *price <= 0.0 {
            continue;
        }
        let take = remaining.min(*size);
        total_cost += take * price;
        remaining -= take;
    }
    if remaining > 1e-6 {
        return None;
    }
    Some((total_cost, total_cost / n))
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

pub fn parse_polymarket_orderbook(data: &Value, side: &str) -> Option<Vec<(f64, f64)>> {
    if side == "YES" {
        let asks = data.get("asks")?.as_array()?;
        let mut result = Vec::new();
        for ask in asks {
            let price = ask.get("price")?.as_str()?.parse::<f64>().ok()?;
            let size = ask.get("size")?.as_str()?.parse::<f64>().ok()?;
            result.push((price, size));
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        Some(result)
    } else if side == "NO" {
        let bids = data.get("bids")?.as_array()?;
        let mut result = Vec::new();
        for bid in bids {
            let bid_price = bid.get("price")?.as_str()?.parse::<f64>().ok()?;
            let size = bid.get("size")?.as_str()?.parse::<f64>().ok()?;
            let ask_price = 1.0 - bid_price;
            if ask_price > 0.01 && ask_price < 1.0 {
                result.push((ask_price, size));
            }
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        Some(result)
    } else {
        None
    }
}

pub fn parse_kalshi_orderbook(data: &Value, side: &str) -> Option<Vec<(f64, f64)>> {
    let orderbook = data.get("orderbook_fp")?;
    
    if side == "YES" {
        let no_bids = orderbook.get("no_dollars")?.as_array()?;
        let mut result = Vec::new();
        for entry in no_bids {
            let bid_price = entry.get(0)?.as_str()?.parse::<f64>().ok()?;
            let size = entry.get(1)?.as_str()?.parse::<f64>().ok()?;
            let ask_price = 1.0 - bid_price;
            if ask_price > 0.01 && ask_price < 1.0 {
                result.push((ask_price, size));
            }
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        Some(result)
    } else if side == "NO" {
        let yes_bids = orderbook.get("yes_dollars")?.as_array()?;
        let mut result = Vec::new();
        for entry in yes_bids {
            let bid_price = entry.get(0)?.as_str()?.parse::<f64>().ok()?;
            let size = entry.get(1)?.as_str()?.parse::<f64>().ok()?;
            let ask_price = 1.0 - bid_price;
            if ask_price > 0.01 && ask_price < 1.0 {
                result.push((ask_price, size));
            }
        }
        result.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        Some(result)
    } else {
        None
    }
}

// ==================== 卖出侧（bid 阶梯）用于模拟平仓 ====================

/// 本周期同一 HTTP 响应解析出的双腿 ask/bid，避免重复请求。
#[derive(Debug, Clone)]
pub struct PairOrderbookLadders {
    pub pm_asks: Vec<(f64, f64)>,
    pub ks_asks: Vec<(f64, f64)>,
    /// 卖出 pm_side 时吃 bid：已按价格**从高到低**（先最优 bid）
    pub pm_bids_desc: Vec<(f64, f64)>,
    pub ks_bids_desc: Vec<(f64, f64)>,
}

fn json_num_field(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Polymarket CLOB：卖出 `side`（YES/NO）时取对应 token 的 **bids**，按价格降序。
pub fn parse_polymarket_bids_desc(data: &Value, side: &str) -> Option<Vec<(f64, f64)>> {
    let _ = side;
    let bids = data.get("bids")?.as_array()?;
    let mut result = Vec::new();
    for bid in bids {
        let price = json_num_field(bid.get("price")?)?;
        let size = json_num_field(bid.get("size")?)?;
        if price > 0.0 && size > 0.0 {
            result.push((price, size));
        }
    }
    if result.is_empty() {
        return None;
    }
    result.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    Some(result)
}

/// Kalshi `orderbook_fp`：卖出 YES 用 `yes_dollars` bid；卖出 NO 用 `no_dollars` bid。价格降序。
pub fn parse_kalshi_bids_desc(data: &Value, side: &str) -> Option<Vec<(f64, f64)>> {
    let orderbook = data.get("orderbook_fp")?;
    let arr = if side == "YES" {
        orderbook.get("yes_dollars")?.as_array()?
    } else if side == "NO" {
        orderbook.get("no_dollars")?.as_array()?
    } else {
        return None;
    };
    let mut result = Vec::new();
    for entry in arr {
        let bid_price = json_num_field(entry.get(0)?)?;
        let size = json_num_field(entry.get(1)?)?;
        if bid_price > 0.01 && bid_price < 1.0 && size > 0.0 {
            result.push((bid_price, size));
        }
    }
    if result.is_empty() {
        return None;
    }
    result.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    Some(result)
}

/// 从已获取的 PM/Kalshi 订单簿 JSON 构建双腿 ask + 卖出用 bid 阶梯。
pub fn build_pair_orderbook_ladders(
    pm_book: &Value,
    ks_book: &Value,
    pm_side: &str,
    ks_side: &str,
) -> Option<PairOrderbookLadders> {
    let pm_asks = parse_polymarket_orderbook(pm_book, pm_side)?;
    let ks_asks = parse_kalshi_orderbook(ks_book, ks_side)?;
    let pm_bids_desc = parse_polymarket_bids_desc(pm_book, pm_side)?;
    let ks_bids_desc = parse_kalshi_bids_desc(ks_book, ks_side)?;
    Some(PairOrderbookLadders {
        pm_asks,
        ks_asks,
        pm_bids_desc,
        ks_bids_desc,
    })
}

/// 在 `bids_desc`（价高优先）上卖出恰好 `n` 份，返回 (总回款 USDT, 加权均价)。
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

#[cfg(test)]
mod arb_tests {
    use super::*;

    #[test]
    fn cost_for_exact_contracts_single_level() {
        let ob = [(0.5, 100.0)];
        let (c, avg) = cost_for_exact_contracts(&ob, 10.0).unwrap();
        assert!((c - 5.0).abs() < 1e-9);
        assert!((avg - 0.5).abs() < 1e-9);
    }

    #[test]
    fn cost_for_exact_contracts_spans_levels() {
        let ob = [(0.1, 50.0), (0.2, 50.0)];
        let (c, avg) = cost_for_exact_contracts(&ob, 75.0).unwrap();
        assert!((c - (50.0 * 0.1 + 25.0 * 0.2)).abs() < 1e-6);
        assert!((avg - c / 75.0).abs() < 1e-9);
    }

    #[test]
    fn cost_for_exact_contracts_insufficient() {
        let ob = [(0.5, 10.0)];
        assert!(cost_for_exact_contracts(&ob, 20.0).is_none());
    }

    #[test]
    fn arbitrage_probe_then_reprice_matches_n() {
        let det = ArbitrageDetector::new(0.0);
        let pm = [(0.001, 1_000_000.0), (0.01, 1_000_000.0)];
        let ks = [(0.05, 100.0), (0.1, 10000.0)];
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
                1_000_000.0,
            )
            .expect("opp");
        assert!(opp.contracts > 0.0);
        let (c_pm, _) = cost_for_exact_contracts(&pm, opp.contracts).unwrap();
        let (c_ks, _) = cost_for_exact_contracts(&ks, opp.contracts).unwrap();
        assert!((opp.capital_used - (c_pm + c_ks)).abs() < 0.01);
    }

    #[test]
    fn proceeds_sell_walks_bids_desc() {
        let bids = [(0.6, 10.0), (0.5, 10.0)];
        let (p, avg) = proceeds_for_exact_contracts_sell(&bids, 5.0).unwrap();
        assert!((p - 3.0).abs() < 1e-9);
        assert!((avg - 0.6).abs() < 1e-9);
    }
}