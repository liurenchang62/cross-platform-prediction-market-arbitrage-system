use std::str::FromStr as _;

use anyhow::{Context, Result};
use alloy_signer_local::PrivateKeySigner;
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::request::{BalanceAllowanceRequest, TradesRequest};
use polymarket_client_sdk::clob::types::{AssetType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client as PmClient, Config as PmConfig};
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::POLYGON;

use crate::system_params;

const PM_HOST: &str = "https://clob.polymarket.com";

fn sig_type() -> SignatureType {
    match std::env::var(system_params::POLYMARKET_SIGNATURE_TYPE_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0)
    {
        1 => SignatureType::Proxy,
        2 => SignatureType::GnosisSafe,
        _ => SignatureType::Eoa,
    }
}

pub async fn connect() -> Result<PmClient<Authenticated<Normal>>> {
    let hex = system_params::load_polymarket_private_key_hex().context("PM 私钥")?;
    let signer = PrivateKeySigner::from_str(&hex)
        .context("PM PrivateKeySigner")?
        .with_chain_id(Some(POLYGON));
    PmClient::new(PM_HOST, PmConfig::default())
        .map_err(|e| anyhow::anyhow!("{}", e))?
        .authentication_builder(&signer)
        .signature_type(sig_type())
        .authenticate()
        .await
        .map_err(|e| anyhow::anyhow!("PM auth: {}", e))
}

fn dec_f64(d: &Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap_or(f64::NAN)
}

pub async fn collateral_balance(client: &PmClient<Authenticated<Normal>>) -> Result<Decimal> {
    let req = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    let r = client.balance_allowance(req).await?;
    Ok(r.balance)
}

pub async fn conditional_shares(
    client: &PmClient<Authenticated<Normal>>,
    token_id: &str,
) -> Result<Decimal> {
    let u: U256 = token_id.parse().context("token_id U256")?;
    let req = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Conditional)
        .token_id(u)
        .build();
    let r = client.balance_allowance(req).await?;
    Ok(r.balance)
}

/// 拉取该 asset 的**全部**成交页（若 SDK 仅返一页则尽力汇总）。
pub async fn all_trades_for_token(
    client: &PmClient<Authenticated<Normal>>,
    token_id: &str,
) -> Result<Vec<polymarket_client_sdk::clob::types::response::TradeResponse>> {
    let u: U256 = token_id.parse().context("token")?;
    let mut out = Vec::new();
    let req = TradesRequest::builder().asset_id(u).build();
    let page = client.trades(&req, None).await?;
    out.extend(page.data);
    Ok(out)
}

pub fn trades_net_ledger(
    trades: &[polymarket_client_sdk::clob::types::response::TradeResponse],
) -> (f64, f64, f64, Option<String>) {
    let mut net_cash_out = 0.0_f64;
    let mut fees = 0.0_f64;
    let mut net_shares = 0.0_f64;
    let mut last: Option<String> = None;
    for t in trades {
        let sz = dec_f64(&t.size);
        let px = dec_f64(&t.price);
        let bps = dec_f64(&t.fee_rate_bps);
        let fee = if (sz * px * (bps / 10_000.0)).is_finite() {
            sz * px * (bps / 10_000.0)
        } else {
            0.0
        };
        fees += fee;
        let notional = sz * px;
        match t.side {
            Side::Buy => {
                net_shares += sz;
                net_cash_out += notional + fee;
            }
            Side::Sell => {
                net_shares -= sz;
                net_cash_out -= notional - fee;
            }
            _ => {}
        }
        last = Some(t.id.clone());
    }
    (net_cash_out, fees, net_shares, last)
}
