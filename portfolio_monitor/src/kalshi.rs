use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::thread_rng;
use reqwest::Client;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pss::BlindedSigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::RsaPrivateKey;
use serde_json::Value;

use crate::system_params;

const KS_FILLS_PATH: &str = "/trade-api/v2/portfolio/fills";

pub struct KalshiAuth {
    pub key_id: String,
    pub key: RsaPrivateKey,
    pub base: String,
}

pub fn load_kalshi() -> Result<KalshiAuth> {
    let key_id = std::env::var(system_params::KALSHI_API_KEY_ID_ENV)
        .context(system_params::KALSHI_API_KEY_ID_ENV)?;
    let path_raw = std::env::var(system_params::KALSHI_PRIVATE_KEY_PATH_ENV)
        .context(system_params::KALSHI_PRIVATE_KEY_PATH_ENV)?;
    let path = system_params::normalize_env_file_path(&path_raw);
    anyhow::ensure!(!path.is_empty(), "Kalshi PEM path empty");
    let mut pem = std::fs::read_to_string(&path).context("read Kalshi PEM")?;
    if pem.starts_with('\u{FEFF}') {
        pem.remove(0);
    }
    let key = RsaPrivateKey::from_pkcs1_pem(pem.trim()).context("Kalshi PKCS#1 PEM")?;
    Ok(KalshiAuth {
        key_id: key_id.trim().to_string(),
        key,
        base: system_params::KALSHI_TRADE_API_BASE
            .trim_end_matches('/')
            .to_string(),
    })
}

fn ts_ms() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn sign(key: &RsaPrivateKey, timestamp_ms: &str, method: &str, path: &str) -> Result<String> {
    let path = path.split('?').next().unwrap_or(path);
    let msg = format!("{}{}{}", timestamp_ms, method, path);
    let sk = BlindedSigningKey::<sha2::Sha256>::new(key.clone());
    let sig = sk
        .sign_with_rng(&mut thread_rng(), msg.as_bytes())
        .to_bytes();
    Ok(BASE64.encode(sig))
}

pub async fn signed_get(
    http: &Client,
    auth: &KalshiAuth,
    url: &str,
    sign_path: &str,
) -> Result<String> {
    let t = ts_ms();
    let sig = sign(&auth.key, &t, "GET", sign_path)?;
    let resp = http
        .get(url)
        .header("KALSHI-ACCESS-KEY", &auth.key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &t)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .send()
        .await
        .context("Kalshi GET")?;
    let code = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !code.is_success() {
        anyhow::bail!("Kalshi {} — {}", code, text.trim());
    }
    Ok(text)
}

/// 鉴权 `POST`（平仓 / 下单）。
pub async fn signed_post(
    http: &Client,
    auth: &KalshiAuth,
    url: &str,
    sign_path: &str,
    body: &Value,
) -> Result<String> {
    let t = ts_ms();
    let sig = sign(&auth.key, &t, "POST", sign_path)?;
    let resp = http
        .post(url)
        .header("KALSHI-ACCESS-KEY", &auth.key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &t)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .json(body)
        .send()
        .await
        .context("Kalshi POST")?;
    let code = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !code.is_success() {
        anyhow::bail!("Kalshi POST {} — {}", code, text.trim());
    }
    Ok(text)
}

/// IOC 限价卖单（平仓用）。`side` = "YES" 或 "NO"，`min_price_cents` 通常填 1。
pub async fn sell_ioc(
    http: &Client,
    auth: &KalshiAuth,
    ticker: &str,
    side: &str,
    count: i64,
    min_price_cents: i64,
) -> Result<Value> {
    let side_lower = side.to_ascii_lowercase();
    let price = min_price_cents.clamp(1, 99);
    let client_id = format!(
        "close_{}_{}_{}",
        side_lower,
        &ticker[..ticker.len().min(20)],
        ts_ms()
    );
    let mut body = serde_json::json!({
        "ticker": ticker,
        "action": "sell",
        "side": side_lower,
        "count": count,
        "type": "limit",
        "time_in_force": "immediate_or_cancel",
        "client_order_id": client_id,
        "reduce_only": true,
    });
    if side_lower == "yes" {
        body["yes_price"] = serde_json::json!(price);
    } else {
        body["no_price"] = serde_json::json!(price);
    }
    let url = format!("{}/portfolio/orders", auth.base);
    let text = signed_post(http, auth, &url, "/trade-api/v2/portfolio/orders", &body).await?;
    serde_json::from_str(&text).context("sell_ioc response json")
}

pub async fn balance_cents(http: &Client, auth: &KalshiAuth) -> Result<i64> {
    let sign_path = "/trade-api/v2/portfolio/balance";
    let url = format!("{}/portfolio/balance", auth.base);
    let text = signed_get(http, auth, &url, sign_path).await?;
    let v: Value = serde_json::from_str(&text).context("balance json")?;
    v["balance"]
        .as_i64()
        .context("balance cents missing")
}

/// 公开 `GET /markets/{ticker}` — 无需鉴权（与主程序拉取订单簿一致）。
pub async fn market_title(http: &Client, auth: &KalshiAuth, ticker: &str) -> Result<String> {
    let url = format!("{}/markets/{}", auth.base, ticker);
    let resp = http.get(&url).send().await.context("Kalshi market GET")?;
    let text = resp.text().await?;
    let v: Value = serde_json::from_str(&text).context("market json")?;
    Ok(v["market"]["title"]
        .as_str()
        .unwrap_or("(no title)")
        .to_string())
}

pub async fn positions_for_ticker(
    http: &Client,
    auth: &KalshiAuth,
    ticker: &str,
) -> Result<Option<Value>> {
    let sign_path = "/trade-api/v2/portfolio/positions";
    let url = format!("{}/portfolio/positions?ticker={}", auth.base, ticker);
    let text = signed_get(http, auth, &url, sign_path).await?;
    let v: Value = serde_json::from_str(&text)?;
    let arr = v["market_positions"].as_array().cloned().unwrap_or_default();
    Ok(arr.into_iter().next())
}

pub async fn all_fills_for_ticker(http: &Client, auth: &KalshiAuth, ticker: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..80 {
        let t = ts_ms();
        let sig = sign(&auth.key, &t, "GET", KS_FILLS_PATH)?;
        let url = format!("{}/portfolio/fills", auth.base);
        let mut req = http
            .get(&url)
            .query(&[("ticker", ticker), ("limit", "200")])
            .header("KALSHI-ACCESS-KEY", &auth.key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &t)
            .header("KALSHI-ACCESS-SIGNATURE", sig);
        if let Some(c) = cursor.as_deref() {
            if !c.is_empty() {
                req = req.query(&[("cursor", c)]);
            }
        }
        let resp = req.send().await.context("fills")?;
        if !resp.status().is_success() {
            anyhow::bail!("fills HTTP {}", resp.status());
        }
        let v: Value = resp.json().await?;
        if let Some(arr) = v["fills"].as_array() {
            for f in arr {
                all.push(f.clone());
            }
        }
        let next = v["cursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        match next {
            Some(c) if cursor.as_ref() != Some(&c) => cursor = Some(c),
            _ => break,
        }
    }
    Ok(all)
}

pub async fn all_settlements_for_ticker(
    http: &Client,
    auth: &KalshiAuth,
    ticker: &str,
) -> Result<Vec<Value>> {
    let sign_path = "/trade-api/v2/portfolio/settlements";
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..80 {
        let t = ts_ms();
        let sig = sign(&auth.key, &t, "GET", sign_path)?;
        let url = format!("{}/portfolio/settlements", auth.base);
        let mut req = http
            .get(&url)
            .query(&[("ticker", ticker), ("limit", "200")])
            .header("KALSHI-ACCESS-KEY", &auth.key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &t)
            .header("KALSHI-ACCESS-SIGNATURE", sig);
        if let Some(c) = cursor.as_deref() {
            if !c.is_empty() {
                req = req.query(&[("cursor", c)]);
            }
        }
        let resp = req.send().await.context("settlements")?;
        if !resp.status().is_success() {
            anyhow::bail!("settlements HTTP {}", resp.status());
        }
        let v: Value = resp.json().await?;
        if let Some(arr) = v["settlements"].as_array() {
            for s in arr {
                all.push(s.clone());
            }
        }
        let next = v["cursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        match next {
            Some(c) if cursor.as_ref() != Some(&c) => cursor = Some(c),
            _ => break,
        }
    }
    Ok(all)
}

pub fn fp_dollars(s: &str) -> f64 {
    s.trim().parse::<f64>().unwrap_or(0.0)
}

/// 仅统计 **买入** 且 `side` 与策略腿一致 的成交：官方 `fee_cost` + 价×量。
/// 净持仓（买 − 卖）与同边 **官方 fee_cost**、**现金流**（买为支出为正、卖为收入为负）。
pub fn fills_net_leg_ledger(fills: &[Value], leg_side_lower: &str) -> (f64, f64, f64, Option<String>) {
    let mut net_contracts = 0.0_f64;
    let mut fees = 0.0_f64;
    let mut net_cash_out = 0.0_f64;
    let mut last_ts: Option<String> = None;
    for f in fills {
        let action = f["action"].as_str().unwrap_or("");
        let side = f["side"].as_str().unwrap_or("");
        if side != leg_side_lower {
            continue;
        }
        let count = f["count_fp"].as_str().map(|s| fp_dollars(s)).unwrap_or(0.0);
        let yes_p = f["yes_price_dollars"].as_str().map(fp_dollars).unwrap_or(0.0);
        let no_p = f["no_price_dollars"].as_str().map(fp_dollars).unwrap_or(0.0);
        let unit = if side == "yes" { yes_p } else { no_p };
        let fee = f["fee_cost"].as_str().map(fp_dollars).unwrap_or(0.0);
        fees += fee;
        let leg = count * unit;
        match action {
            "buy" => {
                net_contracts += count;
                net_cash_out += leg + fee;
            }
            "sell" => {
                net_contracts -= count;
                net_cash_out -= leg - fee;
            }
            _ => {}
        }
        if let Some(ct) = f["created_time"].as_str() {
            last_ts = Some(ct.to_string());
        }
    }
    (net_cash_out, fees, net_contracts, last_ts)
}

pub fn settlements_realized_usd(rows: &[Value]) -> f64 {
    let mut sum = 0.0_f64;
    for s in rows {
        let rev_cents = s["revenue"].as_i64().unwrap_or(0);
        let fee = s["fee_cost"].as_str().map(fp_dollars).unwrap_or(0.0);
        sum += rev_cents as f64 / 100.0 - fee;
    }
    sum
}

fn ladder_level_ok(p: f64, s: f64) -> bool {
    p > 0.0 && p < 1.0 && s > 0.0
}

fn json_num_field(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

/// 公开 `GET /markets/{ticker}/orderbook`（无需鉴权）。
pub async fn fetch_orderbook_json_public(
    http: &Client,
    base: &str,
    ticker: &str,
) -> Result<Value> {
    let url = format!("{}/markets/{}/orderbook", base.trim_end_matches('/'), ticker);
    let resp = http.get(&url).send().await.context("Kalshi orderbook GET")?;
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).context("orderbook json")
}

/// `outcome_side`: "YES" 或 "NO"（大写）。
pub fn orderbook_mid_for_side(book: &Value, outcome_side: &str) -> Option<f64> {
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
        let p = json_num_field(entry.get(0)?)?;
        let s = json_num_field(entry.get(1)?)?;
        if ladder_level_ok(p, s) {
            best_bid = Some(best_bid.map_or(p, |b| b.max(p)));
        }
    }

    let mut best_ask: Option<f64> = None;
    for entry in ask_src_arr {
        let p = json_num_field(entry.get(0)?)?;
        let s = json_num_field(entry.get(1)?)?;
        let ask_p = 1.0 - p;
        if ladder_level_ok(ask_p, s) {
            best_ask = Some(best_ask.map_or(ask_p, |a| a.min(ask_p)));
        }
    }

    let b = best_bid?;
    let a = best_ask?;
    Some(0.5 * (b + a))
}

/// 跟踪用到期展示：`close_time` / `expected_expiration_time` 等字段尽力解析。
pub async fn market_expiry_display(http: &Client, base: &str, ticker: &str) -> String {
    let url = format!("{}/markets/{}", base.trim_end_matches('/'), ticker);
    let Ok(resp) = http.get(&url).send().await else {
        return "—".to_string();
    };
    let Ok(text) = resp.text().await else {
        return "—".to_string();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return "—".to_string();
    };
    let m = &v["market"];
    m["expected_expiration_time"]
        .as_str()
        .or_else(|| m["close_time"].as_str())
        .or_else(|| m["expiration_time"].as_str())
        .unwrap_or("—")
        .to_string()
}
