//! Polymarket CLOB 实盘：官方 `polymarket-client-sdk`（鉴权 + 发单）。
//! 环境变量：`POLYMARKET_PRIVATE_KEY_PATH`（txt 内 hex，优先）或 `POLYMARKET_PRIVATE_KEY`；可选 `POLYMARKET_SIGNATURE_TYPE`（0/1/2）。

use std::str::FromStr as _;

use anyhow::{Context, Result};
use alloy_signer_local::PrivateKeySigner;
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::POLYGON;

use crate::system_params::{
    load_polymarket_private_key_hex, require_live_orders_allowed, POLYMARKET_SIGNATURE_TYPE_ENV,
};

pub const CLOB_HOST: &str = "https://clob.polymarket.com";

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
    ) -> Result<String> {
        require_live_orders_allowed().context("Polymarket buy_outcome_token_fok_n")?;
        let signer = self.local_signer()?;

        let cap = per_leg_cap_usd.max(0.0);
        let slip_cost = (n_contracts * pm_avg_slipped * 1.15_f64).max(0.01);
        let spend_usd = cap.min(slip_cost).max(0.01);

        let spend = Decimal::try_from(spend_usd)
            .ok()
            .context("Decimal::try_from spend_usd")?;
        let spend = spend.max(Decimal::ONE).round_dp(2);

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

        let resp = self
            .client
            .post_order(signed)
            .await
            .map_err(|e| anyhow::anyhow!("post_order: {}", e))?;

        Ok(format!("{:?}", resp))
    }
}
