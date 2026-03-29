//! 只读查看实盘账户侧栏：**Kalshi** 余额与仓位；**Polymarket CLOB** 抵押余额、挂单与成交摘要（不下单）。
//!
//! ```text
//! cargo run --bin live_account_positions
//! ```
//!
//! ## 环境变量
//! - Kalshi: `KALSHI_API_KEY_ID`, `KALSHI_PRIVATE_KEY_PATH`
//! - Polymarket: `POLYMARKET_PRIVATE_KEY_PATH` 或 `POLYMARKET_PRIVATE_KEY`，可选 `POLYMARKET_SIGNATURE_TYPE`
//! - `SKIP_KALSHI=1` / `SKIP_PM=1` 可跳过一侧
//! - `KALSHI_POSITIONS_TICKER`：仅打印该 `ticker` 的 Kalshi 仓位（否则拉取分页中的前若干条非零仓位）

#![allow(dead_code)]

#[path = "../system_params.rs"]
mod system_params;

use std::str::FromStr as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::thread_rng;
use reqwest::Client;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pss::BlindedSigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use serde_json::Value;

use alloy_signer_local::PrivateKeySigner;
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::request::{
    BalanceAllowanceRequest, OrdersRequest, TradesRequest,
};
use polymarket_client_sdk::clob::types::{AssetType, SignatureType};
use polymarket_client_sdk::clob::{Client as PmClient, Config as PmConfig};
use polymarket_client_sdk::POLYGON;

const KALSHI_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
const KALSHI_KEY_ID: &str = "KALSHI_API_KEY_ID";
const KALSHI_KEY_PATH: &str = "KALSHI_PRIVATE_KEY_PATH";
const PM_HOST: &str = "https://clob.polymarket.com";

fn ts_ms() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn kalshi_path_for_sign(full_path: &str) -> &str {
    full_path.split('?').next().unwrap_or(full_path)
}

fn kalshi_sign(key: &RsaPrivateKey, timestamp_ms: &str, method: &str, path: &str) -> Result<String> {
    let path = kalshi_path_for_sign(path);
    let msg = format!("{}{}{}", timestamp_ms, method, path);
    let sk = BlindedSigningKey::<sha2::Sha256>::new(key.clone());
    let sig = sk
        .sign_with_rng(&mut thread_rng(), msg.as_bytes())
        .to_bytes();
    Ok(BASE64.encode(sig))
}

fn load_kalshi_key() -> Result<(String, RsaPrivateKey)> {
    let key_id = std::env::var(KALSHI_KEY_ID).context(KALSHI_KEY_ID)?;
    let path_raw = std::env::var(KALSHI_KEY_PATH).context(KALSHI_KEY_PATH)?;
    let path = system_params::normalize_env_file_path(&path_raw);
    if path.is_empty() {
        anyhow::bail!("{KALSHI_KEY_PATH} 去掉空白与引号后为空");
    }
    let mut pem = std::fs::read_to_string(&path).context("read Kalshi PEM")?;
    if pem.starts_with('\u{FEFF}') {
        pem.remove(0);
    }
    let pk = RsaPrivateKey::from_pkcs1_pem(pem.trim()).context("Kalshi PEM PKCS#1")?;
    Ok((key_id.trim().to_string(), pk))
}

async fn kalshi_signed_get(
    http: &Client,
    key_id: &str,
    key: &RsaPrivateKey,
    url: &str,
    sign_path: &str,
) -> Result<String> {
    let t = ts_ms();
    let sig = kalshi_sign(key, &t, "GET", sign_path)?;
    let resp = http
        .get(url)
        .header("KALSHI-ACCESS-KEY", key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &t)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .send()
        .await
        .context("Kalshi GET")?;
    let code = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !code.is_success() {
        anyhow::bail!("Kalshi GET {} — {}", code, text.trim());
    }
    Ok(text)
}

fn pm_signature_type() -> SignatureType {
    let raw = std::env::var(system_params::POLYMARKET_SIGNATURE_TYPE_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0);
    match raw {
        1 => SignatureType::Proxy,
        2 => SignatureType::GnosisSafe,
        _ => SignatureType::Eoa,
    }
}

async fn pm_connect() -> Result<PmClient<Authenticated<Normal>>> {
    let hex = system_params::load_polymarket_private_key_hex().context("Polymarket 私钥")?;
    let signer = PrivateKeySigner::from_str(&hex)
        .context("PrivateKeySigner")?
        .with_chain_id(Some(POLYGON));
    PmClient::new(PM_HOST, PmConfig::default())
        .map_err(|e| anyhow::anyhow!("{}", e))?
        .authentication_builder(&signer)
        .signature_type(pm_signature_type())
        .authenticate()
        .await
        .map_err(|e| anyhow::anyhow!("CLOB 鉴权: {}", e))
}

fn json_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .or_else(|| v.as_i64().map(|i| i as f64))
}

async fn run_kalshi(http: &Client) -> Result<()> {
    println!("\n======== Kalshi（余额 + 仓位）========");
    let (key_id, key) = load_kalshi_key()?;

    let bal_sign = "/trade-api/v2/portfolio/balance";
    let bal_url = format!("{}/portfolio/balance", KALSHI_BASE);
    let bal_text = kalshi_signed_get(http, &key_id, &key, &bal_url, bal_sign).await?;
    let bal_v: Value = serde_json::from_str(&bal_text).context("balance json")?;
    let cents = bal_v["balance"].as_i64().unwrap_or(0);
    let pv = bal_v["portfolio_value"].as_i64().unwrap_or(0);
    println!(
        "GET /portfolio/balance — balance={}¢ (${:.2}), portfolio_value={}¢ (${:.2})",
        cents,
        cents as f64 / 100.0,
        pv,
        pv as f64 / 100.0
    );

    let filter_ticker = std::env::var("KALSHI_POSITIONS_TICKER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let pos_sign_path = "/trade-api/v2/portfolio/positions";
    let pos_url = if let Some(ref t) = filter_ticker {
        format!("{KALSHI_BASE}/portfolio/positions?ticker={t}")
    } else {
        format!("{KALSHI_BASE}/portfolio/positions?limit=200")
    };
    let pos_text = kalshi_signed_get(http, &key_id, &key, &pos_url, pos_sign_path).await?;
    let pos_v: Value = serde_json::from_str(&pos_text)?;
    let arr = pos_v["market_positions"].as_array().cloned().unwrap_or_default();
    println!(
        "GET /portfolio/positions — 本页条数 {}（有 filter 时由 API 过滤）",
        arr.len()
    );
    let mut shown = 0usize;
    for p in &arr {
        let pos_fp = json_f64(&p["position_fp"]).unwrap_or(0.0);
        if filter_ticker.is_none() && pos_fp.abs() < 1e-9 {
            continue;
        }
        println!(
            "  ticker={} position_fp={} market_exposure_dollars={}",
            p["ticker"].as_str().unwrap_or("?"),
            p["position_fp"].as_str().unwrap_or("?"),
            p["market_exposure_dollars"].as_str().unwrap_or("?")
        );
        shown += 1;
        if shown >= 30 {
            println!("  … 最多展示 30 条非零仓位，其余略");
            break;
        }
    }
    if shown == 0 {
        println!("  （本页无仓位或非零仓位）");
    }
    Ok(())
}

async fn run_polymarket() -> Result<()> {
    println!("\n======== Polymarket CLOB（抵押 + 挂单 + 成交摘要）========");
    let client = pm_connect().await?;
    let col = client
        .balance_allowance(
            BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Collateral)
                .build(),
        )
        .await?;
    println!(
        "balance_allowance COLLATERAL — balance={} allowances(keys)={}",
        col.balance,
        col.allowances.len()
    );

    let ord_req = OrdersRequest::default();
    let orders = client.orders(&ord_req, None).await?;
    let n_show = orders.data.len().min(15);
    println!(
        "open orders 本页 {} 条（展示前 {} 条）",
        orders.data.len(),
        n_show
    );
    for o in orders.data.iter().take(15) {
        println!(
            "  id={} status={:?} side={:?} size_matched={} original_size={} price={}",
            o.id, o.status, o.side, o.size_matched, o.original_size, o.price
        );
    }

    let tr_req = TradesRequest::default();
    let trades = client.trades(&tr_req, None).await?;
    let nt = trades.data.len().min(10);
    println!(
        "trades 本页 {} 条（展示前 {} 条）",
        trades.data.len(),
        nt
    );
    for tr in trades.data.iter().take(10) {
        println!(
            "  id={} side={:?} size={} price={} asset_id={}",
            tr.id, tr.side, tr.size, tr.price, tr.asset_id
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("live_account_positions — 只读账户信息（不下单）\n");
    println!("{}", system_params::polymarket_private_key_env_diag());

    let http = Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?;

    if std::env::var("SKIP_KALSHI").unwrap_or_default() != "1" {
        if let Err(e) = run_kalshi(&http).await {
            println!("\n❌ Kalshi: {}", e);
            std::process::exit(1);
        }
    } else {
        println!("跳过 Kalshi（SKIP_KALSHI=1）");
    }

    if std::env::var("SKIP_PM").unwrap_or_default() != "1" {
        if let Err(e) = run_polymarket().await {
            println!("\n❌ Polymarket: {}", e);
            println!("   {}", system_params::polymarket_private_key_env_diag());
            std::process::exit(1);
        }
    } else {
        println!("跳过 Polymarket（SKIP_PM=1）");
    }

    println!("\n✅ 完成。");
    Ok(())
}
