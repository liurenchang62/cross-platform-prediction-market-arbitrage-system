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
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::sleep;

use crate::arbitrage_detector::{kalshi_orderbook_mid_dollars_for_outcome, ArbitrageOpportunity};
use crate::system_params::{
    require_live_orders_allowed, KALSHI_API_KEY_ID_ENV, KALSHI_BALANCE_DEPLOYABLE_FRAC,
    KALSHI_PRIVATE_KEY_PATH_ENV, KALSHI_TRADE_API_BASE, REQUEST_INTERVAL_MS,
};

pub const KALSHI_SIGN_PATH_ORDERS: &str = "/trade-api/v2/portfolio/orders";
pub const KALSHI_SIGN_PATH_BALANCE: &str = "/trade-api/v2/portfolio/balance";
pub const KALSHI_SIGN_PATH_FEE_CHANGES: &str = "/trade-api/v2/series/fee_changes";
pub const KALSHI_SIGN_PATH_FILLS: &str = "/trade-api/v2/portfolio/fills";
pub const KALSHI_SIGN_PATH_POSITIONS: &str = "/trade-api/v2/portfolio/positions";

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
    // 官方：签名字符串中的 path 不得包含 `?` 及查询参数。
    let path = path.split('?').next().unwrap_or(path);
    let msg = format!("{}{}{}", timestamp_ms, method, path);
    let sk = BlindedSigningKey::<sha2::Sha256>::new(key.clone());
    let sig = sk
        .sign_with_rng(&mut thread_rng(), msg.as_bytes())
        .to_bytes();
    Ok(BASE64.encode(sig))
}

/// `GET /portfolio/orders/{order_id}` — 单笔订单详情（含成交价口径 `*_fill_cost_dollars`、手续费等）。
pub async fn get_order_json(http: &Client, cfg: &KalshiTradeConfig, order_id: &str) -> Result<Value> {
    let sign_path = format!("/trade-api/v2/portfolio/orders/{order_id}");
    let url = format!("{}/portfolio/orders/{order_id}", cfg.base_url);
    let ts = millis_timestamp();
    let sig = sign_request(&cfg.private_key, &ts, "GET", &sign_path)?;

    let resp = http
        .get(&url)
        .header("KALSHI-ACCESS-KEY", &cfg.api_key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &ts)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .send()
        .await
        .context("Kalshi GET /portfolio/orders/{id}")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Kalshi GET order HTTP {}: {}", status.as_u16(), text.trim());
    }
    serde_json::from_str(&text).with_context(|| format!("order JSON: {}", text.trim()))
}

/// 拉取与该 `order_id` 关联的**全部**成交回报（分页游标直到结束）。官方：`GET /portfolio/fills?order_id=`.
pub async fn get_fills_for_order(http: &Client, cfg: &KalshiTradeConfig, order_id: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _page in 0..50 {
        let url = format!("{}/portfolio/fills", cfg.base_url);
        let ts = millis_timestamp();
        let sig = sign_request(&cfg.private_key, &ts, "GET", KALSHI_SIGN_PATH_FILLS)?;

        let mut req = http
            .get(&url)
            .query(&[("order_id", order_id), ("limit", "200")])
            .header("KALSHI-ACCESS-KEY", &cfg.api_key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &ts)
            .header("KALSHI-ACCESS-SIGNATURE", sig);
        if let Some(c) = cursor.as_deref() {
            if !c.is_empty() {
                req = req.query(&[("cursor", c)]);
            }
        }

        let resp = req.send().await.context("Kalshi GET /portfolio/fills")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Kalshi GET fills HTTP {}: {}", status.as_u16(), text.trim());
        }
        let v: Value = serde_json::from_str(&text).context("fills JSON")?;
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
            Some(c) if cursor.as_ref() != Some(&c) => {
                cursor = Some(c);
            }
            _ => break,
        }
    }
    Ok(all)
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

fn position_row_has_open_position(pos: &Value) -> bool {
    let exposure = pos
        .get("market_exposure_dollars")
        .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()))
        .or_else(|| pos.get("market_exposure_dollars").and_then(|x| x.as_f64()));
    if let Some(x) = exposure {
        if x.abs() > 1e-9 {
            return true;
        }
    }
    if let Some(fp) = pos.get("position_fp").and_then(|x| x.as_str()) {
        if let Ok(n) = fp.parse::<f64>() {
            if n.abs() > 1e-9 {
                return true;
            }
        }
    }
    false
}

fn event_position_row_has_open(pos: &Value) -> bool {
    let exposure = pos
        .get("event_exposure_dollars")
        .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()))
        .or_else(|| pos.get("event_exposure_dollars").and_then(|x| x.as_f64()));
    if let Some(x) = exposure {
        if x.abs() > 1e-9 {
            return true;
        }
    }
    if let Some(fp) = pos.get("total_cost_shares_fp").and_then(|x| x.as_str()) {
        if let Ok(n) = fp.parse::<f64>() {
            if n.abs() > 1e-9 {
                return true;
            }
        }
    }
    false
}

/// `position_fp`：正为 YES 净张数，负为 NO 净张数（见 Kalshi `MarketPosition` 文档）。
fn kalshi_position_fp_contracts_and_side(pos: &Value) -> Option<(f64, &'static str)> {
    let fp = pos
        .get("position_fp")?
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| pos.get("position_fp").and_then(|x| x.as_f64()))?;
    let c = fp.abs();
    if !c.is_finite() || c <= 1e-9 {
        return None;
    }
    let side = if fp > 0.0 { "YES" } else { "NO" };
    Some((c, side))
}

/// 公开 `GET /markets/{ticker}/orderbook`（与 [`crate::clients::KalshiClient::get_order_book`] 同路径）。
pub async fn fetch_market_orderbook_json(
    http: &Client,
    cfg: &KalshiTradeConfig,
    ticker: &str,
) -> Result<Option<Value>> {
    if ticker.is_empty() {
        return Ok(None);
    }
    let url = format!("{}/markets/{}/orderbook", cfg.base_url, ticker);
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Kalshi GET orderbook {}", ticker))?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let text = resp.text().await.unwrap_or_default();
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("orderbook JSON for {}", ticker))?;
    Ok(Some(v))
}

/// 分页 `GET /portfolio/positions`：每条有敞口的市场拉簿，**份数 × YES/NO 中间价（美元）** 求和为 **MTM**。  
/// 返回 `(mtm, 市场数, token 数, 成功标价数)`：  
/// - **市场** = `event_positions` 中有敞口的 **event_ticker** 去重数（与 Polymarket Gamma「市场」上层概念对齐）  
/// - **token** = `market_positions` 中有敞口的 **ticker** 去重数（单合约/ outcome 腿）  
/// 若无 `event_positions` 数据则 **市场数 = token 数**。
pub async fn aggregate_kalshi_market_positions_mtm_usd_and_count(
    http: &Client,
    cfg: &KalshiTradeConfig,
) -> Result<(f64, usize, usize, usize)> {
    let mut mtm_total = 0.0_f64;
    let mut open_tickers: HashSet<String> = HashSet::new();
    let mut open_events: HashSet<String> = HashSet::new();
    let mut priced_n = 0usize;
    let mut cursor: Option<String> = None;
    for _ in 0..256 {
        let url = format!("{}/portfolio/positions", cfg.base_url);
        let ts = millis_timestamp();
        let sig = sign_request(&cfg.private_key, &ts, "GET", KALSHI_SIGN_PATH_POSITIONS)?;

        let mut req = http
            .get(&url)
            .query(&[("limit", "1000")])
            .header("KALSHI-ACCESS-KEY", &cfg.api_key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &ts)
            .header("KALSHI-ACCESS-SIGNATURE", sig);
        if let Some(c) = cursor.as_deref() {
            if !c.is_empty() {
                req = req.query(&[("cursor", c)]);
            }
        }

        let resp = req.send().await.context("Kalshi GET /portfolio/positions")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Kalshi positions HTTP {}: {}", status.as_u16(), text.trim());
        }
        let v: Value = serde_json::from_str(&text).context("positions JSON")?;
        if let Some(arr) = v["market_positions"].as_array() {
            for pos in arr {
                let is_open = position_row_has_open_position(pos);
                if is_open {
                    if let Some(t) = pos.get("ticker").and_then(|x| x.as_str()) {
                        open_tickers.insert(t.to_string());
                    }
                }
                if !is_open {
                    continue;
                }
                let Some(ticker) = pos.get("ticker").and_then(|x| x.as_str()) else {
                    continue;
                };
                let Some((contracts, side)) = kalshi_position_fp_contracts_and_side(pos) else {
                    continue;
                };
                if let Some(book) = fetch_market_orderbook_json(http, cfg, ticker).await? {
                    if let Some(mid) = kalshi_orderbook_mid_dollars_for_outcome(&book, side) {
                        mtm_total += contracts * mid;
                        priced_n += 1;
                    }
                }
                sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await;
            }
        }
        if let Some(evs) = v["event_positions"].as_array() {
            for pos in evs {
                if !event_position_row_has_open(pos) {
                    continue;
                }
                if let Some(et) = pos.get("event_ticker").and_then(|x| x.as_str()) {
                    open_events.insert(et.to_string());
                }
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
    let ks_tokens_n = open_tickers.len();
    let ks_markets_n = if open_events.is_empty() {
        ks_tokens_n
    } else {
        open_events.len()
    };
    Ok((mtm_total, ks_markets_n, ks_tokens_n, priced_n))
}

/// 已开仓 Kalshi 市场单腿仓位（按 ticker 聚合，`position_fp` 符号已转为 `side`）。
#[derive(Debug, Clone)]
pub struct KalshiOpenPos {
    pub ticker: String,
    /// "YES" 或 "NO"
    pub side: String,
    pub contracts: f64,
}

/// 拉取全部 Kalshi 有净敞口仓位（分页 `GET /portfolio/positions`），返回每个 ticker 的仓位摘要。
pub async fn fetch_kalshi_open_positions_list(
    http: &Client,
    cfg: &KalshiTradeConfig,
) -> Result<Vec<KalshiOpenPos>> {
    let mut out: Vec<KalshiOpenPos> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..256 {
        let url = format!("{}/portfolio/positions", cfg.base_url);
        let ts = millis_timestamp();
        let sig = sign_request(&cfg.private_key, &ts, "GET", KALSHI_SIGN_PATH_POSITIONS)?;
        let mut req = http
            .get(&url)
            .query(&[("limit", "1000")])
            .header("KALSHI-ACCESS-KEY", &cfg.api_key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &ts)
            .header("KALSHI-ACCESS-SIGNATURE", sig);
        if let Some(c) = cursor.as_deref() {
            if !c.is_empty() {
                req = req.query(&[("cursor", c)]);
            }
        }
        let resp = req.send().await.context("Kalshi GET /portfolio/positions (list)")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Kalshi positions list HTTP {}: {}", status.as_u16(), text.trim());
        }
        let v: Value = serde_json::from_str(&text).context("positions list JSON")?;
        if let Some(arr) = v["market_positions"].as_array() {
            for pos in arr {
                if !position_row_has_open_position(pos) {
                    continue;
                }
                let Some(ticker) = pos.get("ticker").and_then(|x| x.as_str()) else {
                    continue;
                };
                let Some((contracts, side)) = kalshi_position_fp_contracts_and_side(pos) else {
                    continue;
                };
                out.push(KalshiOpenPos {
                    ticker: ticker.to_string(),
                    side: side.to_string(),
                    contracts,
                });
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
    Ok(out)
}

/// `GET /markets/{ticker}`（公开）：读取 `market.result` 字段。
/// 返回 `Some("yes")` / `Some("no")` 表示已结算；`None` 表示尚未结算或字段缺失。
pub async fn fetch_kalshi_market_settlement_result(
    http: &Client,
    cfg: &KalshiTradeConfig,
    ticker: &str,
) -> Result<Option<String>> {
    if ticker.is_empty() {
        return Ok(None);
    }
    let url = format!("{}/markets/{}", cfg.base_url, ticker);
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Kalshi GET /markets/{}", ticker))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "Kalshi GET /markets/{} HTTP {}",
            ticker,
            resp.status().as_u16()
        );
    }
    let v: Value = resp.json().await.context("market result JSON")?;
    let result = v["market"]["result"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase());
    Ok(result)
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
            // 整数字符串 "10"
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
            // 浮点字符串 "10.00"（Kalshi fill_count_fp 格式）
            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()).map(|x| x as i64))
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

    // 使用识别阶段计算的 worst-ask 限价（阶梯最差价位），保证能成交 n 份
    let price_cents = opp.ks_limit_cents.clamp(1, 99);

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
    // Kalshi API 曾用 fill_count（整数），现已改为 fill_count_fp（字符串浮点，如 "10.00"）；两者均尝试。
    let fill_count = match json_order_i64(order, "fill_count")
        .or_else(|| json_order_i64(order, "fill_count_fp"))
    {
        Some(fc) if fc > 0 => fc,
        // fill_count=0 或字段缺失均视为未成交；缺失时不能假设全量成交（曾引发幻象成交）
        Some(_) => anyhow::bail!("Kalshi 买单 IOC 未成交（fill_count=0）: {}", text.trim()),
        None => anyhow::bail!("Kalshi 买单 IOC 响应缺少 fill_count/fill_count_fp 字段，视为未成交: {}", text.trim()),
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
    // Kalshi API 曾用 fill_count（整数），现已改为 fill_count_fp（字符串浮点，如 "10.00"）；两者均尝试。
    let fill_count = match json_order_i64(order, "fill_count")
        .or_else(|| json_order_i64(order, "fill_count_fp"))
    {
        Some(fc) if fc > 0 => fc,
        // fill_count=0 或字段缺失均视为未成交；缺失时不能假设全量成交
        Some(_) => anyhow::bail!("Kalshi 卖单 IOC 未成交（fill_count=0）: {}", text.trim()),
        None => anyhow::bail!("Kalshi 卖单 IOC 响应缺少 fill_count/fill_count_fp 字段，视为未成交: {}", text.trim()),
    };
    Ok(KalshiIocResult {
        order_id,
        requested_count: count,
        fill_count,
        limit_price_cents: price_cents,
    })
}
