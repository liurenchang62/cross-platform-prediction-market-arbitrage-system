//! **实盘冒烟**：默认 **仅 Polymarket FOK 买 1 份**；可选再跑 Kalshi IOC（两侧 YES/NO 一致）。
//!
//! Kalshi 鉴权签名规则（官方）：**签名字符串里的 path 必须去掉 `?` 及查询参数**，只保留路径段。
//!
//! **与主程序解耦**：不 `mod` 其它业务 crate，仅复用本文件内 HTTP + SDK 逻辑。
//!
//! ## 环境变量（与主程序一致）
//! - Kalshi（仅当 `SMOKE_RUN_KALSHI=1`）: `KALSHI_API_KEY_ID`, `KALSHI_PRIVATE_KEY_PATH`
//! - Polymarket: `POLYMARKET_PRIVATE_KEY_PATH` 或 `POLYMARKET_PRIVATE_KEY`，可选 `POLYMARKET_SIGNATURE_TYPE`  
//!   （网页/邮箱注册常见为 **Proxy**，若 CLOB 余额显示 0 但网站有钱，试 `POLYMARKET_SIGNATURE_TYPE=1`）
//!
//! ## 可选
//! - `SMOKE_RUN_KALSHI=1` — 同时跑 Kalshi IOC（默认不跑，避免误下单）
//! - `SMOKE_OUTCOME` — `yes` 或 `no`（默认 `yes`），**两家平台同一取值**
//! - `KALSHI_SMOKE_TICKER` — Kalshi `ticker`（否则自动扫描）
//! - `POLYMARKET_SMOKE_TOKEN_ID` — CLOB token（否则 Gamma 自动选；YES=首 token，NO=次 token）
//! - `SMOKE_SKIP_PM=1` — 跳过 Polymarket
//!
//! ## 运行
//! ```text
//! cargo run --bin smoke_live_orders
//! ```

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
use serde_json::{json, Value};
use uuid::Uuid;

use alloy_signer_local::PrivateKeySigner;
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::request::{BalanceAllowanceRequest, TradesRequest};
use polymarket_client_sdk::clob::types::{Amount, AssetType, OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client as PmClient, Config as PmConfig};
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::POLYGON;

const KALSHI_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
const KALSHI_KEY_ID: &str = "KALSHI_API_KEY_ID";
const KALSHI_KEY_PATH: &str = "KALSHI_PRIVATE_KEY_PATH";
const PM_HOST: &str = "https://clob.polymarket.com";
const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";

fn ts_ms() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

/// Kalshi 官方：签名用 path **不含** query（`?` 及之后一律去掉）。
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

async fn kalshi_signed_post_json(
    http: &Client,
    key_id: &str,
    key: &RsaPrivateKey,
    url: &str,
    sign_path: &str,
    body: &Value,
) -> Result<String> {
    let t = ts_ms();
    let sig = kalshi_sign(key, &t, "POST", sign_path)?;
    let resp = http
        .post(url)
        .header("KALSHI-ACCESS-KEY", key_id)
        .header("KALSHI-ACCESS-TIMESTAMP", &t)
        .header("KALSHI-ACCESS-SIGNATURE", sig)
        .header("Content-Type", "application/json")
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

fn json_num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .or_else(|| v.as_i64().map(|i| i as f64))
}

/// 等价于主程序里「买 YES」的卖价阶梯：来自 `no_dollars` bid，卖价 = 1 - bid。
/// 买 NO：最优 NO ask 来自 `yes_dollars` bid，价 = 1 - bid。
fn kalshi_best_no_ask(orderbook_root: &Value) -> Option<(f64, f64)> {
    let ob = orderbook_root
        .get("orderbook")
        .or_else(|| orderbook_root.get("orderbook_fp"))?;
    let yes_bids = ob.get("yes_dollars")?.as_array()?;
    let mut best_ask = f64::MAX;
    let mut size_at = 0_f64;
    for row in yes_bids {
        let bid = json_num(row.get(0)?)?;
        let sz = json_num(row.get(1)?)?;
        let ask = 1.0 - bid;
        if ask > 0.0 && ask < 1.0 && sz >= 1.0 && ask < best_ask {
            best_ask = ask;
            size_at = sz;
        }
    }
    if best_ask >= 1.0 || max_ask_invalid(best_ask) {
        return None;
    }
    Some((best_ask, size_at))
}

fn kalshi_best_yes_ask(orderbook_root: &Value) -> Option<(f64, f64)> {
    let ob = orderbook_root
        .get("orderbook")
        .or_else(|| orderbook_root.get("orderbook_fp"))?;
    let no_bids = ob.get("no_dollars")?.as_array()?;
    let mut best_ask = f64::MAX;
    let mut size_at = 0_f64;
    for row in no_bids {
        let bid = json_num(row.get(0)?)?;
        let sz = json_num(row.get(1)?)?;
        let ask = 1.0 - bid;
        if ask > 0.0 && ask < 1.0 && sz >= 1.0 && ask < best_ask {
            best_ask = ask;
            size_at = sz;
        }
    }
    if best_ask >= 1.0 || max_ask_invalid(best_ask) {
        return None;
    }
    Some((best_ask, size_at))
}

fn max_ask_invalid(a: f64) -> bool {
    !a.is_finite() || a <= 0.0
}

fn smoke_outcome_yes() -> bool {
    match std::env::var("SMOKE_OUTCOME")
        .unwrap_or_else(|_| "yes".into())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "n" | "no" => false,
        _ => true,
    }
}

fn kalshi_best_ask_for_side(ob: &Value, buy_yes: bool) -> Option<(f64, f64)> {
    if buy_yes {
        kalshi_best_yes_ask(ob)
    } else {
        kalshi_best_no_ask(ob)
    }
}

async fn run_kalshi_smoke(http: &Client) -> Result<()> {
    let buy_yes = smoke_outcome_yes();
    println!(
        "\n======== Kalshi 冒烟（官方 Trade API，买 {} × 1 份 IOC）========",
        if buy_yes { "YES" } else { "NO" }
    );
    let (key_id, key) = load_kalshi_key()?;

    let bal_path = "/trade-api/v2/portfolio/balance";
    let bal_url = format!("{}/portfolio/balance", KALSHI_BASE);
    let bal_text = kalshi_signed_get(http, &key_id, &key, &bal_url, bal_path).await?;
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

    let ticker_from_env = std::env::var("KALSHI_SMOKE_TICKER").ok();
    let ticker = if let Some(t) = ticker_from_env.filter(|s| !s.trim().is_empty()) {
        println!("使用环境变量 KALSHI_SMOKE_TICKER={}", t.trim());
        t.trim().to_string()
    } else {
        println!(
            "扫描 /markets 寻找有 {} 侧流动性的 ticker…",
            if buy_yes { "YES" } else { "NO" }
        );
        // 与 `KalshiClient::fetch_markets_page` 一致：open=可交易列表；单条 JSON 里常见 `status`: **`active`**（不是字面 `"open"`）。
        let list_url = format!(
            "{}/markets?limit=200&status=open&mve_filter=exclude",
            KALSHI_BASE
        );
        let list = http.get(&list_url).send().await.context("Kalshi GET markets")?;
        let list_text = list.text().await?;
        let v: Value = serde_json::from_str(&list_text)?;
        let markets = v["markets"].as_array().context("markets[]")?;
        let mut chosen: Option<String> = None;
        for m in markets {
            let st = m["status"].as_str().unwrap_or("");
            if st != "active" && st != "open" {
                continue;
            }
            let settled = m["result"].as_str().unwrap_or("") != "";
            if settled {
                continue;
            }
            let t = m["ticker"].as_str().unwrap_or("");
            if t.is_empty() {
                continue;
            }
            let ob_url = format!("{}/markets/{}/orderbook", KALSHI_BASE, t);
            let ob_raw = http.get(&ob_url).send().await;
            let Ok(ob_resp) = ob_raw else { continue };
            if !ob_resp.status().is_success() {
                continue;
            }
            let Ok(ob_txt) = ob_resp.text().await else {
                continue;
            };
            let Ok(ob_v) = serde_json::from_str::<Value>(&ob_txt) else {
                continue;
            };
            if kalshi_best_ask_for_side(&ob_v, buy_yes).is_some() {
                chosen = Some(t.to_string());
                println!("选中 ticker={}", t);
                break;
            }
        }
        chosen.context("未找到合适 Kalshi 市场（可自行设 KALSHI_SMOKE_TICKER）")?
    };

    let ob_url = format!("{}/markets/{}/orderbook", KALSHI_BASE, ticker);
    let ob: Value = http
        .get(&ob_url)
        .send()
        .await
        .context("orderbook")?
        .json()
        .await?;
    let (ask, depth) = kalshi_best_ask_for_side(&ob, buy_yes)
        .with_context(|| format!("该 ticker {} 无可用 ask", if buy_yes { "YES" } else { "NO" }))?;
    if depth < 1.0 {
        anyhow::bail!("深度不足 1 份");
    }
    let price_cents = (ask * 100.0).ceil().clamp(1.0, 99.0) as i64;
    let count = 1_i64;
    println!(
        "订单簿最优 {} ≈ {:.4} USD/份，深度 {:.1} → 买 **1 份** 限价 {}¢ IOC",
        if buy_yes { "YES" } else { "NO" },
        ask,
        depth,
        price_cents
    );

    let client_oid = format!("smoke-ks-{}", Uuid::new_v4().simple());
    let body = if buy_yes {
        json!({
            "ticker": ticker,
            "action": "buy",
            "side": "yes",
            "count": count,
            "type": "limit",
            "yes_price": price_cents,
            "time_in_force": "immediate_or_cancel",
            "client_order_id": client_oid,
        })
    } else {
        json!({
            "ticker": ticker,
            "action": "buy",
            "side": "no",
            "count": count,
            "type": "limit",
            "no_price": price_cents,
            "time_in_force": "immediate_or_cancel",
            "client_order_id": client_oid,
        })
    };
    let ord_path = "/trade-api/v2/portfolio/orders";
    let ord_url = format!("{}/portfolio/orders", KALSHI_BASE);
    let ord_text = kalshi_signed_post_json(http, &key_id, &key, &ord_url, ord_path, &body).await?;
    println!("POST /portfolio/orders — 响应摘要（不含敏感字段）已接收");

    let ord_v: Value = serde_json::from_str(&ord_text).context("order json")?;
    if let Some(order) = ord_v.get("order") {
        println!(
            "order_id={} status={:?} fill_count={:?}",
            order["order_id"].as_str().unwrap_or("?"),
            order["status"].as_str(),
            order["fill_count"]
        );
    }

    // 签名 path 不得含 query；请求 URL 可带 `ticker=` 过滤。
    let pos_sign_path = "/trade-api/v2/portfolio/positions";
    let pos_url = format!("{}/portfolio/positions?ticker={}", KALSHI_BASE, ticker);
    let pos_text = kalshi_signed_get(http, &key_id, &key, &pos_url, pos_sign_path).await?;
    let pos_v: Value = serde_json::from_str(&pos_text)?;
    if let Some(arr) = pos_v["market_positions"].as_array() {
        for p in arr {
            if p["ticker"].as_str() == Some(&ticker) {
                println!(
                    "GET /portfolio/positions — ticker={} position_fp={} market_exposure_dollars={}",
                    ticker,
                    p["position_fp"].as_str().unwrap_or("?"),
                    p["market_exposure_dollars"].as_str().unwrap_or("?")
                );
            }
        }
    }
    Ok(())
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

async fn pm_connect() -> Result<(PmClient<Authenticated<Normal>>, String)> {
    let hex = system_params::load_polymarket_private_key_hex().context("Polymarket 私钥")?;
    let signer = PrivateKeySigner::from_str(&hex)
        .context("PrivateKeySigner")?
        .with_chain_id(Some(POLYGON));
    let client = PmClient::new(PM_HOST, PmConfig::default())
        .map_err(|e| anyhow::anyhow!("{}", e))?
        .authentication_builder(&signer)
        .signature_type(pm_signature_type())
        .authenticate()
        .await
        .map_err(|e| anyhow::anyhow!("CLOB 鉴权: {}", e))?;
    Ok((client, hex))
}

fn gamma_outcome_reference_price(m: &Value, idx: usize) -> Option<f64> {
    let raw = m.get("outcomePrices")?.as_str()?;
    let arr: Vec<String> = serde_json::from_str(raw).ok()?;
    let p = arr.get(idx)?.parse::<f64>().ok()?;
    if p > 0.0 && p < 1.0 { Some(p) } else { None }
}

/// Gamma `/markets` 既可能是 `[..]` 也可能是 `{ "markets": [..] }`。
fn gamma_markets_from_response(v: &Value) -> Result<Vec<Value>> {
    if let Some(a) = v.as_array() {
        return Ok(a.clone());
    }
    if let Some(o) = v.as_object() {
        for key in ["markets", "data", "results"] {
            if let Some(a) = o.get(key).and_then(|x| x.as_array()) {
                return Ok(a.clone());
            }
        }
    }
    anyhow::bail!("Gamma /markets JSON 非预期（需根数组或含 markets/data/results 数组）")
}

fn gamma_clob_ids(m: &Value) -> Vec<String> {
    let raw = m
        .get("clobTokenIds")
        .or_else(|| m.get("clob_token_ids"))
        .cloned()
        .unwrap_or(Value::Null);
    match raw {
        Value::String(s) => serde_json::from_str(&s).unwrap_or_default(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => vec![],
    }
}

/// 自动选 token：YES=clobTokenIds[0]，NO=[1]。优先 `orderMinSize≤1`；否则选全局最小 `orderMinSize` 的市场并告警。
async fn gamma_pick_smoke_token(http: &Client, want_yes: bool) -> Result<String> {
    let url = format!(
        "{}/markets?limit=500&active=true&closed=false&archived=false",
        GAMMA_BASE
    );
    let v: Value = http
        .get(&url)
        .send()
        .await
        .context("Gamma markets")?
        .json()
        .await?;
    let markets = gamma_markets_from_response(&v)?;
    println!("Gamma /markets 解析条数: {}", markets.len());

    let idx = if want_yes { 0usize } else { 1usize };
    for m in &markets {
        let ids = gamma_clob_ids(m);
        let Some(t) = ids.get(idx).filter(|s| !s.is_empty()) else {
            continue;
        };
        let min_sz = m
            .get("orderMinSize")
            .and_then(json_num)
            .unwrap_or(1.0)
            .max(1.0);
        if min_sz > 1.01 {
            continue;
        }
        let ref_px = gamma_outcome_reference_price(m, idx).unwrap_or(0.1);
        println!(
            "Gamma 选取 market_id={:?} {} token_idx={} orderMinSize≈{:.2} 参考价≈{:.3}",
            m.get("id").or_else(|| m.get("conditionId")),
            if want_yes { "YES" } else { "NO" },
            idx,
            min_sz,
            ref_px
        );
        return Ok(t.clone());
    }

    let mut best: Option<(f64, String)> = None;
    for m in &markets {
        let ids = gamma_clob_ids(m);
        let Some(t) = ids.get(idx).filter(|s| !s.is_empty()) else {
            continue;
        };
        let min_sz = m
            .get("orderMinSize")
            .and_then(json_num)
            .unwrap_or(1.0)
            .max(1.0);
        if best.as_ref().map_or(true, |(s, _)| min_sz < *s) {
            best = Some((min_sz, t.clone()));
        }
    }
    if let Some((min_sz, t)) = best {
        println!(
            "⚠️ 未找到 orderMinSize≤1 的市场；改用当前页中门槛最小的市场 orderMinSize≈{:.2}（1 份 FOK 可能被拒，可设 POLYMARKET_SMOKE_TOKEN_ID）",
            min_sz
        );
        return Ok(t);
    }

    anyhow::bail!(
        "Gamma 未找到含 {} token 的活跃市场；请设 POLYMARKET_SMOKE_TOKEN_ID",
        if want_yes { "YES" } else { "NO" }
    )
}

/// CLOB 公共 `book`：最优卖一价（买 1 份上限用）。
async fn pm_clob_best_ask(http: &Client, token_id: &str) -> Result<f64> {
    let v: Value = http
        .get(format!("{}/book", PM_HOST))
        .query(&[("token_id", token_id)])
        .send()
        .await
        .context("CLOB GET /book")?
        .json()
        .await
        .context("book json")?;
    let asks = v["asks"]
        .as_array()
        .context("book.asks")?;
    let mut best = f64::MAX;
    for row in asks {
        let p = match row.get("price").and_then(json_num) {
            Some(x) => x,
            None => continue,
        };
        if p > 0.0 && p < best {
            best = p;
        }
    }
    if best >= 1.0 || !best.is_finite() {
        anyhow::bail!("无法从订单簿得到有效卖方最优价");
    }
    Ok(best)
}

async fn run_polymarket_smoke(http: &Client) -> Result<()> {
    let want_yes = smoke_outcome_yes();
    println!(
        "\n======== Polymarket CLOB 冒烟（买 {} × 1 份 FOK）========",
        if want_yes { "YES" } else { "NO" }
    );
    let (client, pk_hex) = pm_connect().await?;
    let signer = move || {
        PrivateKeySigner::from_str(&pk_hex)
            .expect("pk")
            .with_chain_id(Some(POLYGON))
    };

    let sig_t = pm_signature_type();
    println!(
        "CLOB 签名地址（私钥对应 EOA）: {:#x}",
        client.address()
    );
    println!(
        "POLYMARKET_SIGNATURE_TYPE 生效值={}（0=EOA；1=Proxy 网页/邮箱常见；2=Gnosis Safe）",
        sig_t as u8
    );

    let collateral_req = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    match client
        .update_balance_allowance(collateral_req.clone())
        .await
    {
        Ok(()) => println!("已调用 balance-allowance/update（刷新 CLOB 抵押缓存）"),
        Err(e) => println!(
            "⚠️ balance-allowance/update 失败（仍继续读余额）: {}",
            e
        ),
    }
    let col_before = client.balance_allowance(collateral_req).await?;
    println!(
        "balance-allowance (COLLATERAL) balance={} allowances 条目数={}",
        col_before.balance,
        col_before.allowances.len()
    );
    if col_before.balance == Decimal::ZERO {
        println!(
            "提示: 网站「Portfolio」与 CLOB `balance-allowance` 不是同一视图；资金在代理钱包时常见为 signature_type 未设为 1。\
             若你确定同一账户有余额，请试：`POLYMARKET_SIGNATURE_TYPE=1` 后重跑，并核对上方地址与网站展示是否同一体系。"
        );
    }

    let token = if let Ok(t) = std::env::var("POLYMARKET_SMOKE_TOKEN_ID") {
        let t = t.trim().to_string();
        if t.is_empty() {
            gamma_pick_smoke_token(http, want_yes).await?
        } else {
            println!("使用 POLYMARKET_SMOKE_TOKEN_ID");
            t
        }
    } else {
        gamma_pick_smoke_token(http, want_yes).await?
    };

    let token_u: U256 = token.parse().context("token_id → U256")?;

    let cond_before = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Conditional)
        .token_id(token_u)
        .build();
    let bal_t0 = client.balance_allowance(cond_before).await?;
    println!("conditional balance (token) before = {}", bal_t0.balance);

    let ask = pm_clob_best_ask(http, &token).await?;
    // 市价可成交买：CLOB 要求「marketable BUY」名义至少 **$1**；且 USDC 最多 2 位小数。
    // 粗算仍可参考 ask×1.15，但不低于 $1、上限 cap 防异常。
    let spend_f = (ask * 1.15_f64).max(1.0_f64).clamp(1.0_f64, 50.0_f64);
    println!(
        "CLOB 最优卖价 ≈ {:.4} USD/份 → USDC 花费粗算（已保底 $1） {:.2}",
        ask, spend_f
    );
    let spend = Decimal::try_from(spend_f).ok().context("Decimal spend")?;
    let spend = spend.max(Decimal::ONE).round_dp(2);
    println!("Amount::usdc 花费（2 位小数，且 ≥ $1）: {}", spend);
    let amount = Amount::usdc(spend).map_err(|e| anyhow::anyhow!("Amount: {}", e))?;
    let order = client
        .market_order()
        .token_id(token_u)
        .amount(amount)
        .side(Side::Buy)
        .order_type(OrderType::FOK)
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("build order: {}", e))?;
    let signed = client
        .sign(&signer(), order)
        .await
        .map_err(|e| anyhow::anyhow!("sign: {}", e))?;
    let posted = client
        .post_order(signed)
        .await
        .map_err(|e| anyhow::anyhow!("post_order: {}", e))?;
    println!("post_order 返回: {:?}", posted);

    tokio::time::sleep(Duration::from_secs(2)).await;

    let bal_t1 = client
        .balance_allowance(
            BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Conditional)
                .token_id(token_u)
                .build(),
        )
        .await?;
    println!(
        "conditional balance after = {} (应 ≥ before，若 FOK 成交)",
        bal_t1.balance
    );

    let trades_req = TradesRequest::builder().asset_id(token_u).build();
    let trades_page = client.trades(&trades_req, None).await?;
    let n = trades_page.data.len().min(3);
    println!("最近该 asset 成交条数（最多展示 {} 条）: {}", n, trades_page.data.len());
    for tr in trades_page.data.iter().take(3) {
        println!(
            "  id={} side={:?} size={} price={}",
            tr.id, tr.side, tr.size, tr.price
        );
    }

    let col_after = client
        .balance_allowance(
            BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Collateral)
                .build(),
        )
        .await?;
    println!(
        "balance-allowance (COLLATERAL) after = {} （若买入成交，通常略低于 before）",
        col_after.balance
    );

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!(
        "\n⚠️  smoke_live_orders：默认 **仅 Polymarket** 真实 FOK 买 1 份；Kalshi 需设 `SMOKE_RUN_KALSHI=1`（`SMOKE_OUTCOME=yes|no`）。Ctrl+C 可中断。\n"
    );

    let http = Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?;

    if std::env::var("SMOKE_RUN_KALSHI").unwrap_or_default() == "1" {
        run_kalshi_smoke(&http).await?;
    } else {
        println!("跳过 Kalshi（默认；若要测 Kalshi IOC 请设 SMOKE_RUN_KALSHI=1）");
    }

    if std::env::var("SMOKE_SKIP_PM").unwrap_or_default() != "1" {
        run_polymarket_smoke(&http).await?;
    } else {
        println!("跳过 Polymarket（SMOKE_SKIP_PM=1）");
    }

    println!("\n✅ 冒烟流程结束（若某侧 IOC/FOK 未成交，请自行看控制台与交易所 UI）。");
    Ok(())
}
