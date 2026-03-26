//! Kalshi Demo 环境：RSA-PSS-SHA256 签名与 `POST /portfolio/orders` 限价 IOC 下单。
//! 凭证仅来自环境变量，见 `system_params`；失败由调用方回退全本地模拟。

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::thread_rng;
use reqwest::Client;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pss::BlindedSigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use serde_json::{json, Value};

use crate::arbitrage_detector::ArbitrageOpportunity;
use crate::system_params::{
    CASH_UTILIZATION_MAX, KALSHI_DEMO_API_KEY_ID_ENV, KALSHI_DEMO_BASE_URL,
    KALSHI_DEMO_PRIVATE_KEY_PATH_ENV,
};

/// 与 Kalshi 文档一致：签名字符串中的路径（不含 query）。
pub const KALSHI_SIGN_PATH_ORDERS: &str = "/trade-api/v2/portfolio/orders";
pub const KALSHI_SIGN_PATH_BALANCE: &str = "/trade-api/v2/portfolio/balance";

#[derive(Clone)]
pub struct KalshiDemoConfig {
    pub api_key_id: String,
    private_key: RsaPrivateKey,
    pub base_url: String,
}

impl KalshiDemoConfig {
    /// 两个环境变量均非空时加载；否则 `None`（全本地模拟）。
    pub fn try_from_env() -> Result<Option<Self>> {
        let key_id = std::env::var(KALSHI_DEMO_API_KEY_ID_ENV).unwrap_or_default();
        let path = std::env::var(KALSHI_DEMO_PRIVATE_KEY_PATH_ENV).unwrap_or_default();
        if key_id.trim().is_empty() || path.trim().is_empty() {
            return Ok(None);
        }
        let mut pem = std::fs::read_to_string(path.trim())
            .with_context(|| format!("读取私钥文件 {}", path.trim()))?;
        if pem.starts_with('\u{FEFF}') {
            pem.remove(0);
        }
        let key =
            RsaPrivateKey::from_pkcs1_pem(&pem.trim()).context("解析 PEM 私钥（需 PKCS#1：`BEGIN RSA PRIVATE KEY`）")?;
        Ok(Some(Self {
            api_key_id: key_id.trim().to_string(),
            private_key: key,
            base_url: KALSHI_DEMO_BASE_URL.trim_end_matches('/').to_string(),
        }))
    }
}

fn sign_request(key: &RsaPrivateKey, timestamp_ms: &str, method: &str, path: &str) -> Result<String> {
    let msg = format!("{}{}{}", timestamp_ms, method, path);
    let sk = BlindedSigningKey::<sha2::Sha256>::new(key.clone());
    let sig = sk
        .sign_with_rng(&mut thread_rng(), msg.as_bytes())
        .to_bytes();
    Ok(BASE64.encode(sig))
}

/// `GET /portfolio/balance`，`balance` 为可用余额（美分）。
pub async fn get_demo_balance_cents(http: &Client, cfg: &KalshiDemoConfig) -> Result<i64> {
    let url = format!("{}/portfolio/balance", cfg.base_url);
    let ts = millis_timestamp();
    let sig = sign_request(&cfg.private_key, &ts, "GET", KALSHI_SIGN_PATH_BALANCE)?;

    let resp = http
        .get(&url)
        .header("KALSHI-ACCESS-KEY", &cfg.api_key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &ts)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .send()
        .await
        .context("Kalshi Demo GET /portfolio/balance")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "Kalshi Demo 余额查询 HTTP {}: {}",
            status.as_u16(),
            text.trim()
        );
    }
    let v: Value = serde_json::from_str(&text).with_context(|| format!("balance JSON: {}", text))?;
    v["balance"]
        .as_i64()
        .context("响应缺少 balance（美分）")
}

fn millis_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    ms.to_string()
}

/// 在 Demo 端对 Kalshi 腿下买单：限价 IOC，价格为滑点均价向上取整到分（1–99）。
pub async fn place_demo_buy_ioc(
    http: &Client,
    cfg: &KalshiDemoConfig,
    ticker: &str,
    kalshi_side_upper: &str,
    opp: &ArbitrageOpportunity,
    client_order_id: &str,
    per_leg_cap_usd: f64,
) -> Result<String> {
    let count_fp = opp.contracts.floor().max(1.0);
    let count = count_fp as i64;
    if count < 1 {
        anyhow::bail!("合约份数过小，跳过 Demo 下单");
    }

    let side = kalshi_side_upper.to_ascii_lowercase();
    if side != "yes" && side != "no" {
        anyhow::bail!("Kalshi 腿方向应为 YES/NO，当前: {}", kalshi_side_upper);
    }

    let raw_cents = (opp.kalshi_avg_slipped * 100.0).ceil() as i64;
    let price_cents = raw_cents.clamp(1, 99);

    // 按 API 余额 × 95%（`CASH_UTILIZATION_MAX`）与每腿名义上限收缩份数。
    let mut count = count;
    let max_by_leg_cap = if per_leg_cap_usd > 0.0 && price_cents > 0 {
        let leg_cents = (per_leg_cap_usd * 100.0).floor().max(1.0) as i64;
        (leg_cents / price_cents).max(0)
    } else {
        i64::MAX
    };
    if max_by_leg_cap >= 0 && count > max_by_leg_cap {
        println!(
            "   📘 [Kalshi Demo] 每腿上限 ${:.2}，份数由 {} 缩至 {}（限价 {}¢）",
            per_leg_cap_usd, count, max_by_leg_cap, price_cents
        );
        count = max_by_leg_cap;
    }
    if count < 1 {
        anyhow::bail!(
            "Kalshi Demo 每腿上限 ${:.2} 不足以在当前限价 {}¢ 下买 1 份",
            per_leg_cap_usd,
            price_cents
        );
    }

    match get_demo_balance_cents(http, cfg).await {
        Ok(balance) => {
            if balance <= 0 {
                anyhow::bail!(
                    "Kalshi Demo 可用余额为 0（{} 分）；请在 https://demo.kalshi.com 账户中确认虚拟资金",
                    balance
                );
            }
            let spendable = ((balance as f64) * CASH_UTILIZATION_MAX).floor() as i64;
            let reserve = price_cents.max(1);
            let max_by_price = spendable / reserve;
            let max_by_unit = spendable / 100;
            let max_affordable = max_by_price.min(max_by_unit).max(0);
            if max_affordable < 1 {
                anyhow::bail!(
                    "Kalshi Demo 余额不足以买 1 份：可用约 {:.2} USD（balance={}¢），限价 {}¢/份；请充值 Demo 或等纸面用更小 n",
                    balance as f64 / 100.0,
                    balance,
                    price_cents
                );
            }
            if count > max_affordable {
                println!(
                    "   📘 [Kalshi Demo] 可用余额约 {:.2} USD（按 {:.0}% 计），Kalshi 下单份数由 {} 缩至 {}（限价 {}¢）",
                    balance as f64 / 100.0,
                    CASH_UTILIZATION_MAX * 100.0,
                    count,
                    max_affordable,
                    price_cents
                );
                count = max_affordable;
            }
        }
        Err(e) => {
            anyhow::bail!(
                "Kalshi Demo 读取余额失败（须先查询余额再下单）: {}",
                e
            );
        }
    }

    let mut body = json!({
        "ticker": ticker,
        "action": "buy",
        "side": side,
        "count": count,
        "type": "limit",
        "time_in_force": "immediate_or_cancel",
        "client_order_id": client_order_id,
    });
    if side == "yes" {
        body["yes_price"] = json!(price_cents);
    } else {
        body["no_price"] = json!(price_cents);
    }

    let url = format!("{}/portfolio/orders", cfg.base_url);
    let ts = millis_timestamp();
    let sig = sign_request(&cfg.private_key, &ts, "POST", KALSHI_SIGN_PATH_ORDERS)?;

    let resp = http
        .post(&url)
        .header("KALSHI-ACCESS-KEY", &cfg.api_key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &ts)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Kalshi Demo POST /portfolio/orders")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "Kalshi Demo 下单 HTTP {}: {}",
            status.as_u16(),
            text.trim()
        );
    }
    let v: Value = serde_json::from_str(&text).with_context(|| format!("解析 JSON: {}", text))?;
    let order_id = v["order"]["order_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if order_id.is_empty() {
        anyhow::bail!("响应无 order_id: {}", text);
    }
    Ok(order_id)
}
