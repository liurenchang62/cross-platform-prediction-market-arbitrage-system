//! Polymarket CLOB 实盘：官方 `polymarket-client-sdk`（鉴权 + 发单）。
//! 环境变量：`POLYMARKET_PRIVATE_KEY_PATH`（txt 内 hex，优先）或 `POLYMARKET_PRIVATE_KEY`；可选 `POLYMARKET_SIGNATURE_TYPE`（0/1/2）。

use std::collections::HashSet;
use std::str::FromStr as _;
use std::time::Duration;

use anyhow::{Context, Result};
use alloy_signer_local::PrivateKeySigner;
use rust_decimal::prelude::ToPrimitive;
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::request::{BalanceAllowanceRequest, TradesRequest};
pub use polymarket_client_sdk::clob::types::response::{PostOrderResponse, TradeResponse};
use polymarket_client_sdk::clob::types::{Amount, AssetType, OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::POLYGON;
use serde_json::Value;

use crate::clients::PolymarketClient;
use crate::system_params::{
    load_polymarket_private_key_hex, require_live_orders_allowed, POLYMARKET_SIGNATURE_TYPE_ENV,
    REQUEST_INTERVAL_MS,
};

pub const CLOB_HOST: &str = "https://clob.polymarket.com";

/// CLOB `Collateral` 的 `balance`：与 py-clob 一致，为 USDC **micro 单位**（1 USDC = 1e6），换算为美元。
pub fn clob_collateral_micro_usdc_to_usd(balance: &Decimal) -> f64 {
    let div = Decimal::from(1_000_000u64);
    (balance / div).to_f64().unwrap_or(0.0)
}

/// CLOB `Conditional` 的 `balance`：与 Collateral **同为 micro 计值**（**1 份 outcome 份额 = 1e6**）。  
/// 盯市须 **先 ÷1e6 得份数** 再乘订单簿 mid（≈美元/份），否则市值会放大约 1e6 倍。
pub fn clob_conditional_micro_to_shares(balance: &Decimal) -> f64 {
    clob_collateral_micro_usdc_to_usd(balance)
}

fn json_level_price(level: &Value) -> Option<f64> {
    let p = level.get("price")?;
    p.as_f64()
        .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
}

/// CLOB `book` JSON：用最优买 + 最优卖的中间价作 **估算** MTM（无单侧时退化为单侧价）。
pub fn clob_book_mid_price_estimate(book: &Value) -> Option<f64> {
    let bids = book.get("bids")?.as_array()?;
    let asks = book.get("asks")?.as_array()?;
    let best_bid = bids
        .iter()
        .filter_map(json_level_price)
        .fold(None::<f64>, |acc, p| Some(acc.map_or(p, |a| a.max(p))));
    let best_ask = asks
        .iter()
        .filter_map(json_level_price)
        .fold(None::<f64>, |acc, p| Some(acc.map_or(p, |a| a.min(p))));
    match (best_bid, best_ask) {
        (Some(b), Some(a)) => Some((a + b) / 2.0),
        (Some(b), None) | (None, Some(b)) => Some(b),
        _ => None,
    }
}

fn pm_balance_allowance_wants_sync_from_env() -> bool {
    matches!(
        std::env::var("PM_CLOB_BALANCE_SYNC")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Ok("1" | "true" | "yes")
    )
}

fn signature_type_from_env() -> SignatureType {
    let raw = std::env::var(POLYMARKET_SIGNATURE_TYPE_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0);
    match raw {
        1 => SignatureType::Proxy,
        2 => SignatureType::GnosisSafe,
        _ => SignatureType::Eoa,
    }
}

#[derive(Clone)]
pub struct PolymarketClobTrader {
    client: Client<Authenticated<Normal>>,
    /// 私钥保留在内存中，仅用于每笔 `sign`；进程须运行在受控环境。
    pk_hex: String,
}

impl PolymarketClobTrader {
    /// 与官方文档一致：新机会开始 PM 腿前清空 SDK 内 tick / fee / neg_risk 缓存，避免串用旧 `fee_rate`。
    pub fn invalidate_clob_caches(&self) {
        self.client.invalidate_internal_caches();
    }

    /// `GET /fee-rate` → `base_fee`（bps）；计入 SDK 缓存。
    /// 按 `POST /order` 返回的 `trade_ids` 拉取 [`TradeResponse`]（官方 `GET /data/trades?id=` + asset）。
    pub async fn fetch_trades_for_post_order(
        &self,
        token_id: &str,
        post: &PostOrderResponse,
    ) -> Result<Vec<TradeResponse>> {
        let token_u256: U256 = token_id
            .parse()
            .map_err(|e| anyhow::anyhow!("token_id → U256: {}", e))?;
        let mut out = Vec::new();
        for tid in &post.trade_ids {
            let req = TradesRequest::builder()
                .id(tid.as_str())
                .asset_id(token_u256)
                .build();
            let page = self
                .client
                .trades(&req, None)
                .await
                .map_err(|e| anyhow::anyhow!("CLOB trades id={}: {}", tid, e))?;
            for t in page.data {
                if t.id == *tid {
                    out.push(t);
                    break;
                }
            }
        }
        Ok(out)
    }

    pub async fn get_fee_bps_for_token(&self, token_id: &str) -> Result<u32> {
        let token_u256: U256 = token_id
            .parse()
            .map_err(|e| anyhow::anyhow!("token_id → U256: {}", e))?;
        let r = self
            .client
            .fee_rate_bps(token_u256)
            .await
            .map_err(|e| anyhow::anyhow!("fee_rate_bps: {}", e))?;
        Ok(r.base_fee)
    }

    pub async fn connect() -> Result<Self> {
        let pk_hex = load_polymarket_private_key_hex().context("加载 Polymarket 私钥")?;
        let signer = PrivateKeySigner::from_str(&pk_hex)
            .with_context(|| "解析 Polymarket 私钥 hex")?
            .with_chain_id(Some(POLYGON));

        let client = Client::new(CLOB_HOST, Config::default())
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .authentication_builder(&signer)
            .signature_type(signature_type_from_env())
            .authenticate()
            .await
            .map_err(|e| anyhow::anyhow!("Polymarket CLOB 鉴权失败: {}", e))?;

        Ok(Self {
            client,
            pk_hex,
        })
    }

    /// `GET /balance-allowance`（Collateral）。可选 `PM_CLOB_BALANCE_SYNC=1` 先刷新缓存。
    pub async fn fetch_clob_collateral_balance_for_guard(&self) -> Result<Decimal> {
        let collateral_req = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Collateral)
            .build();
        if pm_balance_allowance_wants_sync_from_env() {
            let _ = self
                .client
                .update_balance_allowance(collateral_req.clone())
                .await;
        }
        let col = self
            .client
            .balance_allowance(collateral_req)
            .await
            .map_err(|e| anyhow::anyhow!("CLOB balance-allowance (Collateral): {}", e))?;
        Ok(col.balance)
    }

    /// 分页 `GET data/trades` 收集出现过的 `asset_id`（十进制字符串）。**不**保证覆盖无成交历史的持仓 token。
    ///
    /// 官方 OpenAPI 将 `maker_address`（SDK 序列化为查询键 `maker`）标为 **必填**；此前 `TradesRequest::default()` 会触发
    /// `400 {"error":"Invalid trade params payload"}`。
    pub async fn list_distinct_asset_ids_from_trade_history(&self) -> Result<Vec<String>> {
        let maker = self.client.address();
        let req = TradesRequest::builder()
            .maker_address(maker)
            .build();
        let mut out: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..512 {
            let page = self
                .client
                .trades(&req, cursor.clone())
                .await
                .map_err(|e| anyhow::anyhow!("CLOB trades 分页: {}", e))?;
            for t in page.data {
                out.insert(format!("{}", t.asset_id));
            }
            let next = page.next_cursor;
            if next.is_empty() || next.trim() == "LTE=" {
                break;
            }
            cursor = Some(next);
        }
        Ok(out.into_iter().collect())
    }

    /// 返回官方 **micro** 原始值（与 Collateral 同量级，1 份 = 1e6）。会话风控盯市见 [`clob_conditional_micro_to_shares`]。
    pub async fn fetch_conditional_balance_shares(&self, token_id: &str) -> Result<Decimal> {
        let token_u256: U256 = token_id
            .parse()
            .map_err(|e| anyhow::anyhow!("token_id → U256: {}", e))?;
        let bal_req = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Conditional)
            .token_id(token_u256)
            .build();
        if pm_balance_allowance_wants_sync_from_env() {
            let _ = self
                .client
                .update_balance_allowance(bal_req.clone())
                .await;
        }
        let row = self
            .client
            .balance_allowance(bal_req)
            .await
            .map_err(|e| anyhow::anyhow!("CLOB balance-allowance (Conditional): {}", e))?;
        Ok(row.balance)
    }

    /// 条件仓：**市场**数（Gamma `id` 去重）、**token** 数（`balance>0` 的 outcome 条数）、MTM 美元（mid×份额，micro 先 ÷1e6）、成功估出 mid 的 token 数。
    pub async fn estimate_conditional_mtm_usd_from_trades_and_books(
        &self,
        pm_public: &PolymarketClient,
    ) -> Result<(f64, usize, usize, usize)> {
        let mut token_ids = self.list_distinct_asset_ids_from_trade_history().await?;
        token_ids.sort();
        let mut mtm = 0.0_f64;
        let mut tokens_n = 0usize;
        let mut valued = 0usize;
        let mut gamma_markets: HashSet<String> = HashSet::new();
        for tid in token_ids {
            let bal = self.fetch_conditional_balance_shares(&tid).await?;
            if bal <= Decimal::ZERO {
                continue;
            }
            tokens_n += 1;
            if let Some(gmid) = pm_public.try_gamma_market_id_for_clob_token(&tid).await {
                gamma_markets.insert(gmid);
            }
            let Some(book) = pm_public.get_order_book(&tid).await? else {
                tokio::time::sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await;
                continue;
            };
            if let Some(mid) = clob_book_mid_price_estimate(&book) {
                let qty = clob_conditional_micro_to_shares(&bal);
                mtm += qty * mid;
                valued += 1;
            }
            tokio::time::sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await;
        }
        let mut markets_n = gamma_markets.len();
        if markets_n == 0 && tokens_n > 0 {
            markets_n = tokens_n;
        }
        Ok((mtm, markets_n, tokens_n, valued))
    }

    fn local_signer(&self) -> Result<PrivateKeySigner> {
        PrivateKeySigner::from_str(&self.pk_hex)
            .with_context(|| "PrivateKeySigner::from_str")
            .map(|s| s.with_chain_id(Some(POLYGON)))
    }

    /// 第二腿：按目标份数 `n_contracts` 与当前 PM 侧加权均价 `pm_avg_slipped` 估 USDC 花费（含 15% 缓冲，与 smoke 一致），
    /// 并满足 CLOB 市价买 ≥$1、2 位小数。
    pub async fn buy_outcome_token_fok_n(
        &self,
        token_id: &str,
        n_contracts: f64,
        pm_avg_slipped: f64,
        per_leg_cap_usd: f64,
    ) -> Result<PostOrderResponse> {
        require_live_orders_allowed().context("Polymarket buy_outcome_token_fok_n")?;
        let signer = self.local_signer()?;

        let cap = per_leg_cap_usd.max(0.0);
        let slip_cost = (n_contracts * pm_avg_slipped * 1.15_f64).max(0.01);
        let spend_usd = cap.min(slip_cost).max(0.01);

        let spend = Decimal::try_from(spend_usd)
            .ok()
            .context("Decimal::try_from spend_usd")?;
        let spend = spend.round_dp(2);
        if spend < Decimal::ONE {
            anyhow::bail!(
                "PM CLOB 最低委托 $1.00，计算金额 ${:.4}；调用方须保证传入金额 ≥ $1",
                spend
            );
        }

        let amount = Amount::usdc(spend).map_err(|e| anyhow::anyhow!("Amount::usdc: {}", e))?;

        let token_u256: U256 = token_id
            .parse()
            .map_err(|e| anyhow::anyhow!("token_id → U256: {}", e))?;

        let order = self
            .client
            .market_order()
            .token_id(token_u256)
            .amount(amount)
            .side(Side::Buy)
            .order_type(OrderType::FOK)
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("build market_order: {}", e))?;

        let signed = self
            .client
            .sign(&signer, order)
            .await
            .map_err(|e| anyhow::anyhow!("sign: {}", e))?;

        self.client
            .post_order(signed)
            .await
            .map_err(|e| anyhow::anyhow!("post_order: {}", e))
    }
}
