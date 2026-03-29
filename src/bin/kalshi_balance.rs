//! 查询 Kalshi 生产环境 `GET /portfolio/balance`（凭证仅来自系统环境变量）。
//! `KALSHI_API_KEY_ID`、`KALSHI_PRIVATE_KEY_PATH`；运行：`cargo run --bin kalshi_balance`

#[path = "../system_params.rs"]
mod system_params;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::thread_rng;
use reqwest::Client;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pss::BlindedSigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// 与 `system_params::KALSHI_*` 一致
const KEY_ID_ENV: &str = "KALSHI_API_KEY_ID";
const KEY_PATH_ENV: &str = "KALSHI_PRIVATE_KEY_PATH";
const PROD_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
const SIGN_PATH: &str = "/trade-api/v2/portfolio/balance";

fn sign(key: &RsaPrivateKey, ts: &str) -> Result<String> {
    let msg = format!("{}GET{}", ts, SIGN_PATH);
    let sk = BlindedSigningKey::<sha2::Sha256>::new(key.clone());
    let sig = sk.sign_with_rng(&mut thread_rng(), msg.as_bytes()).to_bytes();
    Ok(BASE64.encode(sig))
}

fn ts_ms() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    let key_id = std::env::var(KEY_ID_ENV).context(KEY_ID_ENV)?;
    let pem_path_raw = std::env::var(KEY_PATH_ENV).context(KEY_PATH_ENV)?;
    let pem_path = system_params::normalize_env_file_path(&pem_path_raw);
    if pem_path.is_empty() {
        anyhow::bail!("{KEY_PATH_ENV} 去掉空白与引号后为空");
    }
    let mut pem = std::fs::read_to_string(&pem_path).context("read pem")?;
    if pem.starts_with('\u{FEFF}') {
        pem.remove(0);
    }
    let pk = RsaPrivateKey::from_pkcs1_pem(pem.trim()).context("PEM PKCS#1")?;

    let url = format!("{}/portfolio/balance", PROD_BASE.trim_end_matches('/'));
    let t = ts_ms();
    let sig = sign(&pk, &t)?;

    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let resp = http
        .get(&url)
        .header("KALSHI-ACCESS-KEY", key_id.trim())
        .header("KALSHI-ACCESS-TIMESTAMP", &t)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .send()
        .await
        .context("GET balance")?;

    let code = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    println!("HTTP {}", code);
    println!("{}", body);
    if code != 200 {
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body).context("json")?;
    let bal = v["balance"].as_i64().unwrap_or(0);
    let pv = v["portfolio_value"].as_i64().unwrap_or(0);
    println!(
        "---\nbalance (可用, 美分) = {}  => ${:.2}\nportfolio_value (美分) = {}  => ${:.2}",
        bal,
        bal as f64 / 100.0,
        pv,
        pv as f64 / 100.0
    );
    Ok(())
}
