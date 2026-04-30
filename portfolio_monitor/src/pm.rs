use std::str::FromStr as _;

use anyhow::{Context, Result};
use alloy_signer_local::PrivateKeySigner;
use polymarket_client_sdk_v2::auth::Signer as _;
use polymarket_client_sdk_v2::auth::Normal;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::clob::types::request::{
    BalanceAllowanceRequest, MidpointRequest, TradesRequest,
};
use polymarket_client_sdk_v2::clob::types::{AssetType, Side, SignatureType};
use polymarket_client_sdk_v2::clob::{Client as PmClient, Config as PmConfig};
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::{derive_proxy_wallet, derive_safe_wallet, POLYGON};

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

/// Polymarket **Data API**（`data-api.polymarket.com`）按 `user=` 查询的地址。
/// 与 CLOB 认证一致：EOA 用私钥地址；Proxy / Safe 用 SDK 推导的钱包地址。
/// 供 `pm_relayer_automation` / 组合工具读取与 CLOB `connect` 相同的私钥配置。
pub fn read_polymarket_private_key_hex() -> Result<String> {
    system_params::load_polymarket_private_key_hex()
}

pub fn polymarket_data_api_user_address() -> Result<Address> {
    let hex = system_params::load_polymarket_private_key_hex().context("PM 私钥")?;
    let signer = PrivateKeySigner::from_str(&hex)
        .context("PM PrivateKeySigner")?
        .with_chain_id(Some(POLYGON));
    let eoa = signer.address();
    Ok(match sig_type() {
        SignatureType::Eoa => eoa,
        SignatureType::Proxy => derive_proxy_wallet(eoa, POLYGON)
            .context("derive_proxy_wallet：请确认链为 Polygon 且环境 POLYMARKET_SIGNATURE_TYPE=1")?,
        SignatureType::GnosisSafe => derive_safe_wallet(eoa, POLYGON)
            .context("derive_safe_wallet：请确认链为 Polygon 且环境 POLYMARKET_SIGNATURE_TYPE=2")?,
        _ => anyhow::bail!("不支持的 Polymarket SignatureType（请检查 SDK / POLYMARKET_SIGNATURE_TYPE）"),
    })
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

/// CLOB `balance-allowance` 对 Collateral / Conditional 均为 **micro 计值**（1 USDC 或 1 份份额 = 1e6），与主程序 `polymarket_trade` 一致。
pub fn clob_micro_to_usd(balance: &Decimal) -> f64 {
    let div = Decimal::from(1_000_000u64);
    dec_f64(&(balance / div))
}

/// Polymarket CLOB `token_id` / `asset_id`（Gamma `clobTokenIds`）应为**非空十进制数字**字符串。
/// 占位符如 `PASTE_...` 会令 `U256` 解析失败，调用方应先校验或跳过。
pub fn pm_clob_token_id_valid(token_id: &str) -> bool {
    let s = token_id.trim();
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn token_id_to_u256(token_id: &str) -> Result<U256> {
    let s = token_id.trim();
    if !pm_clob_token_id_valid(s) {
        anyhow::bail!(
            "pm_buy_token_id 须为非空纯十进制数字（desk_events 或链上数据源须为真实 CLOB token_id）"
        );
    }
    s.parse().context("token_id U256")
}

/// FAK 市价卖出 outcome token（平仓用）。
pub async fn sell_token_fak(
    client: &PmClient<Authenticated<Normal>>,
    token_id: &str,
    n: i64,
) -> Result<polymarket_client_sdk_v2::clob::types::response::PostOrderResponse> {
    use polymarket_client_sdk_v2::clob::types::{Amount, OrderType};
    let token_u256: U256 = token_id_to_u256(token_id)?;
    let size_dec = Decimal::from(n as u64);
    let amount = Amount::shares(size_dec).map_err(|e| anyhow::anyhow!("Amount::shares: {}", e))?;
    let hex = system_params::load_polymarket_private_key_hex().context("PM 私钥")?;
    let signer = PrivateKeySigner::from_str(&hex)
        .context("PrivateKeySigner")?
        .with_chain_id(Some(POLYGON));
    let order = client
        .market_order()
        .token_id(token_u256)
        .amount(amount)
        .side(Side::Sell)
        .order_type(OrderType::FAK)
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("build market_order (sell): {}", e))?;
    let signed = client
        .sign(&signer, order)
        .await
        .map_err(|e| anyhow::anyhow!("sign: {}", e))?;
    client
        .post_order(signed)
        .await
        .map_err(|e| anyhow::anyhow!("post_order (sell): {}", e))
}

pub async fn collateral_balance(client: &PmClient<Authenticated<Normal>>) -> Result<Decimal> {
    let req = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    let r = client.balance_allowance(req).await?;
    Ok(r.balance)
}

pub async fn token_midpoint_price(
    client: &PmClient<Authenticated<Normal>>,
    token_id: &str,
) -> Result<Option<f64>> {
    let u: U256 = token_id_to_u256(token_id)?;
    let req = MidpointRequest::builder().token_id(u).build();
    let r = client.midpoint(&req).await.map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(Some(dec_f64(&r.mid)))
}

pub async fn conditional_shares(
    client: &PmClient<Authenticated<Normal>>,
    token_id: &str,
) -> Result<Decimal> {
    let u: U256 = token_id_to_u256(token_id)?;
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
) -> Result<Vec<polymarket_client_sdk_v2::clob::types::response::TradeResponse>> {
    let u: U256 = token_id_to_u256(token_id)?;
    let mut out = Vec::new();
    let req = TradesRequest::builder().asset_id(u).build();
    let page = client.trades(&req, None).await?;
    out.extend(page.data);
    Ok(out)
}

pub fn trades_net_ledger(
    trades: &[polymarket_client_sdk_v2::clob::types::response::TradeResponse],
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
