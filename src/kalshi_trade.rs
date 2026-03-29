//! Kalshi 生产环境：RSA-PSS-SHA256 鉴权与 `POST /portfolio/orders`（IOC 限价买），与官方 Trade API 一致。

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
    require_live_orders_allowed, KALSHI_API_KEY_ID_ENV, KALSHI_BALANCE_DEPLOYABLE_FRAC,
    KALSHI_PRIVATE_KEY_PATH_ENV, KALSHI_TRADE_API_BASE,
};

pub const KALSHI_SIGN_PATH_ORDERS: &str = "/trade-api/v2/portfolio/orders";
pub const KALSHI_SIGN_PATH_BALANCE: &str = "/trade-api/v2/portfolio/balance";
pub const KALSHI_SIGN_PATH_FEE_CHANGES: &str = "/trade-api/v2/series/fee_changes";

#[derive(Clone)]
pub struct KalshiTradeConfig {
    pub api_key_id: String,
    private_key: RsaPrivateKey,
    pub base_url: String,
}

impl KalshiTradeConfig {
    pub fn try_from_env() -> Result<Self> {
        let key_id = std::env::var(KALSHI_API_KEY_ID_ENV)
            .with_context(|| format!("环境变量 {} 未设置", KALSHI_API_KEY_ID_ENV))?;
        let path_raw = std::env::var(KALSHI_PRIVATE_KEY_PATH_ENV)
            .with_context(|| format!("环境变量 {} 未设置", KALSHI_PRIVATE_KEY_PATH_ENV))?;
        let path = crate::system_params::normalize_env_file_path(&path_raw);
        if key_id.trim().is_empty() || path.is_empty() {
            anyhow::bail!("Kalshi API Key 或私钥路径为空");
        }
        let mut pem = std::fs::read_to_string(&path)
            .with_context(|| format!("读取私钥文件（字符数 {}）", path.len()))?;
        if pem.starts_with('\u{FEFF}') {
            pem.remove(0);
        }
        let key = RsaPrivateKey::from_pkcs1_pem(&pem.trim())
            .context("解析 PEM 私钥（需 PKCS#1：`BEGIN RSA PRIVATE KEY`）")?;
        Ok(Self {
            api_key_id: key_id.trim().to_string(),
            private_key: key,
            base_url: KALSHI_TRADE_API_BASE.trim_end_matches('/').to_string(),
        })
    }
}

/// `GET /series/fee_changes`（全量）；由 [`crate::kalshi_fee_schedule`] 按 series 过滤解析。
pub async fn fetch_series_fee_changes_json(
    http: &Client,
    cfg: &KalshiTradeConfig,
) -> Result<Value> {
    let url = format!("{}/series/fee_changes", cfg.base_url);
    let ts = millis_timestamp();
    let sig = sign_request(&cfg.private_key, &ts, "GET", KALSHI_SIGN_PATH_FEE_CHANGES)?;

    let resp = http
        .get(&url)
        .header("KALSHI-ACCESS-KEY", &cfg.api_key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &ts)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .send()
        .await
        .context("Kalshi GET /series/fee_changes")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Kalshi fee_changes HTTP {}: {}", status.as_u16(), text.trim());
    }
    serde_json::from_str(&text).context("fee_changes JSON")
}

fn sign_request(key: &RsaPrivateKey, timestamp_ms: &str, method: &str, path: &str) -> Result<String> {
    let msg = format!("{}{}{}", timestamp_ms, method, path);
    let sk = BlindedSigningKey::<sha2::Sha256>::new(key.clone());
    let sig = sk
        .sign_with_rng(&mut thread_rng(), msg.as_bytes())
        .to_bytes();
    Ok(BASE64.encode(sig))
}

pub async fn get_balance_cents(http: &Client, cfg: &KalshiTradeConfig) -> Result<i64> {
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
        .context("Kalshi GET /portfolio/balance")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Kalshi 余额 HTTP {}: {}", status.as_u16(), text.trim());
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

fn json_order_i64(order: &Value, key: &str) -> Option<i64> {
    order.get(key).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|x| x as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

/// Kalshi IOC 单笔结果（用于第一腿 → 以 `fill_count` 锚定第二腿 n）。
#[derive(Debug, Clone)]
pub struct KalshiIocResult {
    pub order_id: String,
    pub requested_count: i64,
    pub fill_count: i64,
    /// 本次 IOC 限价（美分/份），用于估算已付成本上界。
    pub limit_price_cents: i64,
}

impl KalshiIocResult {
    /// 用限价估算已成交量总成本（美元）；成交价可能略优于限价。
    pub fn fill_cost_usd_upper(&self) -> f64 {
        self.fill_count as f64 * self.limit_price_cents as f64 / 100.0
    }
}

/// 限价 IOC 买 Kalshi 腿；名义与份数逻辑与原 Demo/纸面收缩一致。
pub async fn place_buy_ioc(
    http: &Client,
    cfg: &KalshiTradeConfig,
    ticker: &str,
    kalshi_side_upper: &str,
    opp: &ArbitrageOpportunity,
    client_order_id: &str,
    per_leg_cap_usd: f64,
) -> Result<KalshiIocResult> {
    require_live_orders_allowed().context("Kalshi place_buy_ioc")?;
    let count_fp = opp.contracts.floor().max(1.0);
    let mut count = count_fp as i64;
    if count < 1 {
        anyhow::bail!("合约份数过小，跳过 Kalshi 下单");
    }

    let side = kalshi_side_upper.to_ascii_lowercase();
    if side != "yes" && side != "no" {
        anyhow::bail!("Kalshi 腿方向应为 YES/NO，当前: {}", kalshi_side_upper);
    }

    let raw_cents = (opp.kalshi_avg_slipped * 100.0).ceil() as i64;
    let price_cents = raw_cents.clamp(1, 99);

    let max_by_leg_cap = if per_leg_cap_usd > 0.0 && price_cents > 0 {
        let leg_cents = (per_leg_cap_usd * 100.0).floor().max(1.0) as i64;
        (leg_cents / price_cents).max(0)
    } else {
        i64::MAX
    };
    if max_by_leg_cap >= 0 && count > max_by_leg_cap {
        println!(
            "   📘 [Kalshi] 每腿上限 ${:.2}，份数由 {} 缩至 {}（限价 {}¢）",
            per_leg_cap_usd, count, max_by_leg_cap, price_cents
        );
        count = max_by_leg_cap;
    }
    if count < 1 {
        anyhow::bail!(
            "Kalshi 每腿上限 ${:.2} 不足以在当前限价 {}¢ 下买 1 份",
            per_leg_cap_usd,
            price_cents
        );
    }

    let balance = get_balance_cents(http, cfg).await?;
    if balance <= 0 {
        anyhow::bail!("Kalshi 可用余额为 0（{} 分）", balance);
    }
    let spendable = ((balance as f64) * KALSHI_BALANCE_DEPLOYABLE_FRAC).floor() as i64;
    let reserve = price_cents.max(1);
    let max_by_price = spendable / reserve;
    let max_by_unit = spendable / 100;
    let max_affordable = max_by_price.min(max_by_unit).max(0);
    if max_affordable < 1 {
        anyhow::bail!(
            "Kalshi 余额不足以买 1 份：可用约 {:.2} USD（balance={}¢），限价 {}¢/份",
            balance as f64 / 100.0,
            balance,
            price_cents
        );
    }
    if count > max_affordable {
        println!(
            "   📘 [Kalshi] 可用余额约 {:.2} USD（动用 {:.0}%），份数由 {} 缩至 {}（限价 {}¢）",
            balance as f64 / 100.0,
            KALSHI_BALANCE_DEPLOYABLE_FRAC * 100.0,
            count,
            max_affordable,
            price_cents
        );
        count = max_affordable;
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
        .context("Kalshi POST /portfolio/orders")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Kalshi 下单 HTTP {}: {}", status.as_u16(), text.trim());
    }
    let v: Value = serde_json::from_str(&text).with_context(|| format!("解析 JSON: {}", text))?;
    let order = &v["order"];
    let order_id = order["order_id"].as_str().unwrap_or("").to_string();
    if order_id.is_empty() {
        anyhow::bail!("响应无 order_id: {}", text);
    }
    let fill_count = match json_order_i64(order, "fill_count") {
        Some(fc) if fc > 0 => fc,
        Some(_) => anyhow::bail!("Kalshi IOC 未成交（fill_count=0）: {}", text.trim()),
        None => count,
    };
    Ok(KalshiIocResult {
        order_id,
        requested_count: count,
        fill_count,
        limit_price_cents: price_cents,
    })
}

/// Kalshi IOC **卖出**减仓（用于止损卖平第一腿多仓）。`min_price_cents` 为该侧最低可接受限价（美分），越小越易立即成交。
pub async fn place_sell_ioc(
    http: &Client,
    cfg: &KalshiTradeConfig,
    ticker: &str,
    kalshi_side_upper: &str,
    count: i64,
    min_price_cents: i64,
    client_order_id: &str,
) -> Result<KalshiIocResult> {
    require_live_orders_allowed().context("Kalshi place_sell_ioc")?;
    let count = count.max(1);
    let side = kalshi_side_upper.to_ascii_lowercase();
    if side != "yes" && side != "no" {
        anyhow::bail!("Kalshi 腿方向应为 YES/NO，当前: {}", kalshi_side_upper);
    }
    let price_cents = min_price_cents.clamp(1, 99);

    let mut body = json!({
        "ticker": ticker,
        "action": "sell",
        "side": side,
        "count": count,
        "type": "limit",
        "time_in_force": "immediate_or_cancel",
        "client_order_id": client_order_id,
        "reduce_only": true,
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
        .context("Kalshi POST /portfolio/orders (sell IOC)")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Kalshi 卖单 HTTP {}: {}", status.as_u16(), text.trim());
    }
    let v: Value = serde_json::from_str(&text).with_context(|| format!("解析 JSON: {}", text))?;
    let order = &v["order"];
    let order_id = order["order_id"].as_str().unwrap_or("").to_string();
    if order_id.is_empty() {
        anyhow::bail!("响应无 order_id: {}", text);
    }
    let fill_count = match json_order_i64(order, "fill_count") {
        Some(fc) if fc > 0 => fc,
        Some(_) => anyhow::bail!("Kalshi 卖单 IOC 未成交（fill_count=0）: {}", text.trim()),
        None => count,
    };
    Ok(KalshiIocResult {
        order_id,
        requested_count: count,
        fill_count,
        limit_price_cents: price_cents,
    })
}
