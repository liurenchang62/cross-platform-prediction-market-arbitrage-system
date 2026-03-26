use crate::market::{Market, MarketPrices};
use crate::system_params::*;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::sleep;

/// Gamma `events[].markets[]` 或 `GET /markets` 单条：`endDate`（RFC3339）写入 `resolution_date`；与 Kalshi 展示对齐时见 `parse_kalshi_market_resolution_date`。
/// 从 Gamma 单条 `market` JSON 解析 `Market`（用于 `/markets?id=` 等）；已关闭/已结算返回 `None`。
pub(crate) fn parse_polymarket_gamma_market_row(
    market_data: &Value,
    category: Option<String>,
    tags: Vec<String>,
) -> Option<Market> {
    let is_closed = market_data["closed"].as_bool().unwrap_or(true);
    let is_resolved = market_data["umaResolutionStatus"].as_str() == Some("resolved");
    if is_closed || is_resolved {
        return None;
    }

    let market_id = market_data["id"].as_str().unwrap_or_default().to_string();
    if market_id.is_empty() {
        return None;
    }
    let question = market_data["question"].as_str().unwrap_or_default().to_string();

    let mut yes_price = 0.0;
    let mut no_price = 0.0;
    if let Some(prices_str) = market_data["outcomePrices"].as_str() {
        if let Ok(prices) = serde_json::from_str::<Vec<String>>(prices_str) {
            if prices.len() >= 2 {
                if let (Ok(yes), Ok(no)) = (prices[0].parse::<f64>(), prices[1].parse::<f64>()) {
                    yes_price = yes;
                    no_price = no;
                }
            }
        }
    }

    let best_ask = market_data["bestAsk"].as_f64();
    let best_bid = market_data["bestBid"].as_f64();
    let last_trade_price = market_data["lastTradePrice"].as_f64();

    let volume_24h = market_data["volume24hr"].as_f64().unwrap_or(0.0);

    let mut token_ids = Vec::new();
    if let Some(token_ids_str) = market_data["clobTokenIds"].as_str() {
        if let Ok(ids) = serde_json::from_str::<Vec<String>>(token_ids_str) {
            token_ids = ids;
        }
    }

    let resolution_date = parse_polymarket_market_resolution_date(market_data);

    Some(Market {
        platform: "polymarket".to_string(),
        market_id,
        title: question,
        description: market_data["description"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        resolution_date,
        category,
        tags,
        slug: market_data["slug"].as_str().map(|s| s.to_string()),
        token_ids,
        outcome_prices: Some((yes_price, no_price)),
        best_ask,
        best_bid,
        last_trade_price,
        vector_cache: None,
        categories: Vec::new(),
        volume_24h,
    })
}

/// Polymarket：统一以 **`endDate`**（及 `endDateIso`）表示 Gamma 上的「市场/事件结束时刻」；缺失时再尝试关市、UMA 等。
/// 与 Kalshi 对表时，优先与 Kalshi 的 **`expected_expiration_time`** 对齐（见下），二者都比 Kalshi 单独的 `close_time` 更接近「结果窗口结束」。
pub(crate) fn parse_polymarket_market_resolution_date(market_data: &Value) -> Option<DateTime<Utc>> {
    for key in ["endDate", "end_date"] {
        if let Some(s) = market_data[key].as_str() {
            if s.is_empty() {
                continue;
            }
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }
        }
    }
    if let Some(s) = market_data["endDateIso"].as_str() {
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            let ndt = d.and_hms_opt(0, 0, 0)?;
            return Some(Utc.from_utc_datetime(&ndt));
        }
    }
    for key in ["closedTime", "closed_time"] {
        if let Some(dt) = parse_rfc3339_field(market_data, key) {
            return Some(dt);
        }
    }
    if let Some(s) = market_data["umaEndDate"].as_str() {
        if !s.is_empty() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }
        }
    }
    if let Some(s) = market_data["umaEndDateIso"].as_str() {
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            let ndt = d.and_hms_opt(0, 0, 0)?;
            return Some(Utc.from_utc_datetime(&ndt));
        }
    }
    None
}

fn parse_rfc3339_field(market_data: &Value, key: &str) -> Option<DateTime<Utc>> {
    if market_data[key].is_null() {
        return None;
    }
    let s = market_data[key].as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Kalshi：与 Polymarket **`endDate`** 做跨平台展示/筛选时，**优先 `expected_expiration_time`**（API 释义：预计到期/结果落定相关），
/// 数值上通常与 PM `endDate` 最接近。`close_time` 多为交易所关市截止，常晚于真实赛果窗口，作第二顺位。
pub(crate) fn parse_kalshi_market_resolution_date(market_data: &Value) -> Option<DateTime<Utc>> {
    for key in [
        "expected_expiration_time",
        "close_time",
        "expiration_time",
        "latest_expiration_time",
    ] {
        if let Some(dt) = parse_rfc3339_field(market_data, key) {
            return Some(dt);
        }
    }
    None
}



// PriceCache 结构体定义
struct PriceCacheEntry {
    prices: MarketPrices,
    timestamp: Instant,
}

struct PriceCache {
    entries: Arc<RwLock<HashMap<String, PriceCacheEntry>>>,
    ttl: Duration,
}

impl PriceCache {
    fn new(ttl_secs: u64) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    async fn get(&self, key: &str) -> Option<MarketPrices> {
        let entries = self.entries.read().await;
        if let Some(entry) = entries.get(key) {
            if entry.timestamp.elapsed() < self.ttl {
                return Some(entry.prices.clone());
            }
        }
        None
    }

    async fn set(&self, key: String, prices: MarketPrices) {
        let mut entries = self.entries.write().await;
        entries.insert(key, PriceCacheEntry {
            prices,
            timestamp: Instant::now(),
        });
    }

    /// 清空缓存（用于追踪周期边界：与 wall-clock TTL 解耦，每周期重新拉价）
    async fn clear(&self) {
        let mut entries = self.entries.write().await;
        entries.clear();
    }
}





















#[derive(Clone)]
pub struct PolymarketClient {
    http_client: Client,
    base_url: String,
    price_cache: Arc<PriceCache>,
}


impl PolymarketClient {
    pub fn new() -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| Client::new());
        
        Self {
            http_client,
            base_url: "https://gamma-api.polymarket.com".to_string(),
            price_cache: Arc::new(PriceCache::new(60)),
        }
    }

    const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

    /// 带重试的请求
    async fn request_with_retry<F, Fut, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut retries = 0;
        loop {
            match f().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    retries += 1;
                    if retries >= MAX_RETRIES {
                        return Err(e);
                    }
                    let delay = RETRY_INITIAL_DELAY_MS * (1 << (retries - 1));
                    eprintln!("⚠️ 请求失败，{}秒后重试 ({}/{}): {}", delay/1000, retries, MAX_RETRIES, e);
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    // PolymarketClient 的 fetch_all_markets 函数
    pub async fn fetch_all_markets(&self) -> Result<Vec<Market>> {
            let tag_slug = std::env::var("POLYMARKET_TAG_SLUG").ok();
            let tag_slug = tag_slug.as_deref().filter(|s| !s.is_empty());
            
            let mut all_markets = Vec::new();
            let mut offset = 0;
            let limit = POLYMARKET_PAGE_LIMIT as usize;
            
            println!("   📡 获取 Polymarket 所有市场 (上限 {} 个)...", POLYMARKET_MAX_MARKETS);
            
            while all_markets.len() < POLYMARKET_MAX_MARKETS {
                let result = self.request_with_retry(|| async {
                    self.fetch_markets_page(tag_slug, offset).await
                }).await;
                
                match result {
                    Ok(mut markets) => {
                        if markets.is_empty() {
                            println!("      无更多市场，获取完成");
                            break;
                        }
                        
                        println!("      偏移 {}: {} 个市场, 累计 {} 个", 
                            offset, markets.len(), all_markets.len());
                        
                        all_markets.append(&mut markets);
                        offset += limit;
                        
                        sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await;
                    }
                    Err(e) => {
                        eprintln!("      获取失败: {}", e);
                        break;
                    }
                }
            }
            
            if all_markets.len() >= POLYMARKET_MAX_MARKETS {
                println!("      达到获取上限 {} 个，停止获取", POLYMARKET_MAX_MARKETS);
                all_markets.truncate(POLYMARKET_MAX_MARKETS);
            }
            
            println!("   ✅ 获取到 {} 个 Polymarket 市场", all_markets.len());
            Ok(all_markets)
        }

    // PolymarketClient 的 fetch_markets_page 函数
    async fn fetch_markets_page(
            &self, 
            tag_slug: Option<&str>, 
            offset: usize
        ) -> Result<Vec<Market>> {
            let limit = POLYMARKET_PAGE_LIMIT as usize;
            let limit_str = limit.to_string();
            let offset_str = offset.to_string();
            
            let mut query = vec![
                ("active", "true"),
                ("closed", "false"),
                ("limit", &limit_str),
                ("offset", &offset_str),
                ("order", "volume24hr"),
                ("ascending", "false"),
            ];

            if let Some(t) = tag_slug {
                if !t.is_empty() {
                    query.push(("tag_slug", t));
                }
            }

            let url = format!("{}/events", Self::GAMMA_API_BASE);
            let response = self
                .http_client
                .get(&url)
                .query(&query)
                .send()
                .await
                .context("Failed to fetch Polymarket events")?;

            if !response.status().is_success() {
                return Err(anyhow::anyhow!("Gamma API error: {}", response.status()));
            }

            let data: Vec<serde_json::Value> = response.json().await?;
            let mut markets = Vec::new();

            for event_data in data {
                let tags: Vec<String> = event_data["tags"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| t["slug"].as_str().or_else(|| t["label"].as_str()))
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();

                let category = event_data["category"].as_str().map(String::from);

                if let Some(event_markets) = event_data["markets"].as_array() {
                    for market_data in event_markets {
                        if let Some(market) = parse_polymarket_gamma_market_row(
                            market_data,
                            category.clone(),
                            tags.clone(),
                        ) {
                            markets.push(market);
                        }
                    }
                }
            }

            Ok(markets)
        }





    pub async fn fetch_prices(&self, market: &Market) -> Result<MarketPrices> {
        if let Some(cached) = self.price_cache.get(&market.market_id).await {
            return Ok(cached);
        }

        let (yes_price, no_price) = match market.outcome_prices {
            Some((yes, no)) => (yes, no),
            None => {
                match (market.best_ask, market.best_bid, market.last_trade_price) {
                    (Some(ask), Some(bid), _) => ((ask + bid) / 2.0, 1.0 - ((ask + bid) / 2.0)),
                    (Some(ask), None, _) => (ask, 1.0 - ask),
                    (None, Some(bid), _) => (bid, 1.0 - bid),
                    (None, None, Some(last)) => (last, 1.0 - last),
                    _ => return Err(anyhow::anyhow!("No price data available")),
                }
            }
        };

        let prices = MarketPrices::new(yes_price, no_price, 0.0)
            .with_asks(
                market.best_ask.unwrap_or(yes_price),
                market.best_bid.map(|b| 1.0 - b).unwrap_or(no_price),
                market.last_trade_price,
            );

        self.price_cache.set(market.market_id.clone(), prices.clone()).await;
        Ok(prices)
    }

    /// 追踪周期边界：清空 Polymarket 价格内存缓存，使本周期内 `fetch_prices` 按周期重新计算（与 60s TTL 解耦）。
    pub async fn clear_price_cache(&self) {
        self.price_cache.clear().await;
    }

    /// Gamma `GET /markets?id=...&limit=1`：拉取单市场快照（用于追踪周期刷新 outcome/bestAsk 等，与全量列表同源）。
    pub async fn fetch_market_snapshot_by_id(&self, market_id: &str) -> Result<Market> {
        if market_id.is_empty() {
            anyhow::bail!("empty polymarket market_id");
        }
        let url = format!("{}/markets", Self::GAMMA_API_BASE);
        let response = self
            .http_client
            .get(&url)
            .query(&[("id", market_id), ("limit", "1")])
            .send()
            .await
            .context("Failed to fetch Polymarket market by id")?;
        if !response.status().is_success() {
            anyhow::bail!("Gamma API error: {}", response.status());
        }
        let arr: Vec<Value> = response.json().await?;
        let market_data = arr
            .first()
            .ok_or_else(|| anyhow::anyhow!("Polymarket market not found: {}", market_id))?;
        parse_polymarket_gamma_market_row(market_data, None, Vec::new()).ok_or_else(|| {
            anyhow::anyhow!("Polymarket market closed or unparsable: {}", market_id)
        })
    }

    /// Gamma `GET /markets?id=...&limit=1`：补全 `Market.resolution_date`（线上响应为 JSON 数组）
    pub async fn fetch_resolution_by_market_id(&self, market_id: &str) -> Option<DateTime<Utc>> {
        if market_id.is_empty() {
            return None;
        }
        let url = format!("{}/markets", Self::GAMMA_API_BASE);
        let response = self
            .http_client
            .get(&url)
            .query(&[("id", market_id), ("limit", "1")])
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let arr: Vec<Value> = response.json().await.ok()?;
        arr.first()
            .and_then(|m| parse_polymarket_market_resolution_date(m))
    }

    pub async fn get_order_book(&self, token_id: &str) -> Result<Option<serde_json::Value>> {
        let url = "https://clob.polymarket.com/book";
        let response = self
            .http_client
            .get(url)
            .query(&[("token_id", token_id)])
            .send()
            .await
            .context("Failed to fetch Polymarket order book")?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let data: serde_json::Value = response.json().await?;
        Ok(Some(data))
    }
}

impl Default for PolymarketClient {
    fn default() -> Self {
        Self::new()
    }
}

// clients.rs (继续)

const KALSHI_DEFAULT_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";

#[derive(Clone)]
pub struct KalshiClient {
    http_client: Client,
    base_url: String,
    price_cache: Arc<PriceCache>,
}

impl KalshiClient {
    pub fn new() -> Self {
        Self::new_with_base_url(KALSHI_DEFAULT_BASE)
    }

    /// 自定义 Trade API 根 URL（须含 `/trade-api/v2`，无末尾 `/`）。
    /// Demo 真下单时需与 `KALSHI_DEMO_BASE_URL` 一致，否则生产 ticker 在 Demo 会 `market_not_found`。
    pub fn new_with_base_url(base_url: impl AsRef<str>) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| Client::new());

        let base = base_url.as_ref().trim_end_matches('/').to_string();

        Self {
            http_client,
            base_url: base,
            price_cache: Arc::new(PriceCache::new(60)),
        }
    }

    /// 带重试的请求
    async fn request_with_retry<F, Fut, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut retries = 0;
        loop {
            match f().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    retries += 1;
                    if retries >= MAX_RETRIES {
                        return Err(e);
                    }
                    let delay = RETRY_INITIAL_DELAY_MS * (1 << (retries - 1));
                    eprintln!("⚠️ 请求失败，{}秒后重试 ({}/{}): {}", delay/1000, retries, MAX_RETRIES, e);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }

    /// 获取所有 Kalshi 市场（5万封顶）

    pub async fn fetch_all_markets(&self) -> Result<Vec<Market>> {
        use std::collections::HashMap;
        
        let mut all_markets = Vec::new();
        let mut cursor = String::new();
        let limit = KALSHI_PAGE_LIMIT;
        
        println!("   📡 获取 Kalshi 所有市场 (上限 {} 个)...", KALSHI_MAX_MARKETS);
        
        // 第一步：先获取所有系列信息，用于 category 映射
        let mut series_category_map: HashMap<String, Option<String>> = HashMap::new();
        
        let events_result = self.fetch_series_info().await;
        if let Ok(events_data) = events_result {
            if let Some(events) = events_data["events"].as_array() {
                for event in events {
                    if let Some(series) = event["series_ticker"].as_str() {
                        let category = event["category"].as_str().map(String::from);
                        series_category_map.insert(series.to_string(), category);
                    }
                }
            }
        }
        
        // 第二步：分页获取所有市场
        let mut page_count = 0;
        
        while all_markets.len() < KALSHI_MAX_MARKETS {
            page_count += 1;
            
            // 第一次请求直接调用，不经过重试
            let result = if cursor.is_empty() {
                self.fetch_markets_page("", limit).await
            } else {
                self.request_with_retry(|| async {
                    self.fetch_markets_page(&cursor, limit).await
                }).await
            };
            
            match result {
                Ok((markets, next_cursor)) => {
                    if markets.is_empty() {
                        break;
                    }
                    
                    println!("      第 {} 页: {} 个市场, 累计 {} 个", 
                        page_count, markets.len(), all_markets.len());
                    
                    // 处理 markets
                    for market_data in markets {
                        if all_markets.len() >= KALSHI_MAX_MARKETS {
                            break;
                        }
                        
                        // 过滤活跃市场
                        let is_active = market_data["status"].as_str() == Some("active");
                        let is_settled = market_data["result"].as_str().unwrap_or("") != "";
                        if !is_active || is_settled {
                            continue;
                        }
                        
                        // 提取候选人名称
                        let candidate_name = market_data["yes_sub_title"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("");
                        
                        // 提取价格
                        let yes_ask_cents = market_data["yes_ask"].as_i64().unwrap_or(0);
                        let yes_bid_cents = market_data["yes_bid"].as_i64().unwrap_or(0);
                        let last_price_cents = market_data["last_price"].as_i64();
                        
                        // 提取24小时成交量
                        let volume_24h = market_data["volume_24h_fp"]
                            .as_str()
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(0.0);
                        
                        // 构建标题
                        let title = if !candidate_name.is_empty() {
                            format!("{} - {}", 
                                market_data["title"].as_str().unwrap_or(""),
                                candidate_name
                            )
                        } else {
                            market_data["title"].as_str().unwrap_or("").to_string()
                        };
                        
                        let market_ticker = market_data["ticker"].as_str().unwrap_or("").to_string();
                        let _event_ticker = market_data["event_ticker"].as_str().unwrap_or("").to_string();
                        
                        let resolution_date = parse_kalshi_market_resolution_date(&market_data);
                        
                        // 获取系列对应的 category
                        let event_ticker_str = market_data["event_ticker"].as_str().unwrap_or("");
                        let series_prefix = event_ticker_str.split('-').next().unwrap_or("");
                        let category = series_category_map.get(series_prefix).cloned().flatten();
                        
                        let market = Market {
                            platform: "kalshi".to_string(),
                            market_id: market_ticker,
                            title,
                            description: market_data["subtitle"].as_str().unwrap_or("").to_string(),
                            resolution_date,
                            category,
                            tags: Vec::new(),
                            slug: None,
                            token_ids: Vec::new(),
                            outcome_prices: None,
                            best_ask: Some(yes_ask_cents as f64 / 100.0),
                            best_bid: Some(yes_bid_cents as f64 / 100.0),
                            last_trade_price: last_price_cents.map(|v| v as f64 / 100.0),
                            vector_cache: None,
                            categories: Vec::new(),
                            volume_24h,
                        };
                        
                        all_markets.push(market);
                    }
                    
                    // 更新 cursor
                    cursor = next_cursor;
                    
                    // 请求间隔
                    tokio::time::sleep(Duration::from_millis(REQUEST_INTERVAL_MS)).await;
                    
                    if cursor.is_empty() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("      获取失败: {}", e);
                    break;
                }
            }
        }
        
        if all_markets.len() >= KALSHI_MAX_MARKETS {
            println!("      达到获取上限 {} 个，停止获取", KALSHI_MAX_MARKETS);
            all_markets.truncate(KALSHI_MAX_MARKETS);
        }
        
        println!("   ✅ 获取到 {} 个 Kalshi 市场", all_markets.len());
        Ok(all_markets)
    }



    /// 获取系列信息
    async fn fetch_series_info(&self) -> Result<serde_json::Value> {
        let url = format!("{}/events", self.base_url);
        let response = self
            .http_client
            .get(&url)
            .query(&[("status", "open"), ("limit", "1000")])
            .send()
            .await
            .context("Failed to fetch Kalshi events")?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!("Kalshi API error: {}", response.status()));
        }

        let data: serde_json::Value = response.json().await?;
        Ok(data)
    }









    async fn fetch_markets_page(&self, cursor: &str, limit: u32) -> Result<(Vec<serde_json::Value>, String)> {
        let url = format!("{}/markets", self.base_url);
        
        let limit_str = limit.to_string();
        let mut params = vec![
            ("status", "open"),
            ("limit", limit_str.as_str()),
            ("mve_filter", "exclude"),
        ];
        
        if !cursor.is_empty() {
            params.push(("cursor", cursor));
        }
        
        let response = self
            .http_client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch Kalshi markets")?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!("Kalshi API error: {}", response.status()));
        }

        let data: serde_json::Value = response.json().await?;
        let markets = data["markets"].as_array().cloned().unwrap_or_default();
        let next_cursor = data["cursor"].as_str().unwrap_or("").to_string();
        
        Ok((markets, next_cursor))
    }









    pub async fn get_market_prices(&self, ticker: &str) -> Result<Option<MarketPrices>> {
        if let Some(cached) = self.price_cache.get(ticker).await {
            return Ok(Some(cached));
        }
        
        let url = format!("{}/markets/{}", self.base_url, ticker);
        
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch Kalshi market")?;
        
        if !response.status().is_success() {
            return Ok(None);
        }
        
        let data: serde_json::Value = response.json().await?;
        
        if let Some(market) = data.get("market") {
            let yes_ask_dollars_str = market["yes_ask_dollars"].as_str().unwrap_or("0");
            let yes_bid_dollars_str = market["yes_bid_dollars"].as_str().unwrap_or("0");
            
            let yes_ask_dollars = yes_ask_dollars_str.parse::<f64>().unwrap_or(0.0);
            let yes_bid_dollars = yes_bid_dollars_str.parse::<f64>().unwrap_or(0.0);
            
            let last_price_cents = market["last_price"]
                .as_i64()
                .or_else(|| {
                    market["last_price_dollars"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|v| (v * 100.0) as i64)
                });
            
            let volume = market["volume_24h_fp"].as_f64().unwrap_or(0.0);
            
            let yes_price = (yes_ask_dollars + yes_bid_dollars) / 2.0;
            let no_price = 1.0 - yes_price;
            
            let prices = MarketPrices::new(yes_price, no_price, volume)
                .with_asks(
                    yes_ask_dollars,
                    1.0 - yes_bid_dollars,
                    last_price_cents.map(|v| v as f64 / 100.0)
                );
            
            self.price_cache.set(ticker.to_string(), prices.clone()).await;
            return Ok(Some(prices));
        }
        
        Ok(None)
    }

    /// 追踪周期边界：清空 Kalshi 价格内存缓存，使本周期内 `get_market_prices` 重新请求 API（与 60s TTL 解耦）。
    pub async fn clear_price_cache(&self) {
        self.price_cache.clear().await;
    }

    /// `GET /markets/{ticker}`，从响应 `market` 对象解析到期日
    pub async fn fetch_resolution_by_ticker(&self, ticker: &str) -> Option<DateTime<Utc>> {
        if ticker.is_empty() {
            return None;
        }
        let url = format!("{}/markets/{}", self.base_url, ticker);
        let response = self.http_client.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let data: Value = response.json().await.ok()?;
        data.get("market")
            .and_then(|m| parse_kalshi_market_resolution_date(m))
    }

    pub async fn get_order_book(&self, ticker: &str) -> Result<Option<serde_json::Value>> {
        let url = format!("{}/markets/{}/orderbook", self.base_url, ticker);
        
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch Kalshi order book")?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let data: serde_json::Value = response.json().await?;
        Ok(Some(data))
    }
}

impl Default for KalshiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod resolution_date_field_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn polymarket_uses_enddate_rfc3339() {
        let market = serde_json::json!({
            "id": "m1",
            "endDate": "2026-12-31T23:59:59Z",
            "question": "test?"
        });
        let dt = parse_polymarket_market_resolution_date(&market).expect("endDate 应解析成功");
        assert_eq!(dt, Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap());

        let with_snake = serde_json::json!({
            "end_date": "2026-01-02T00:00:00Z",
        });
        assert_eq!(
            parse_polymarket_market_resolution_date(&with_snake)
                .unwrap()
                .to_rfc3339(),
            "2026-01-02T00:00:00+00:00"
        );

        let wrong = serde_json::json!({
            "expiration_time": "2026-12-31T23:59:59Z"
        });
        assert!(
            parse_polymarket_market_resolution_date(&wrong).is_none(),
            "Polymarket 不使用 Kalshi 的 expiration_time"
        );
    }

    #[test]
    fn polymarket_enddate_iso_fallback() {
        let market = serde_json::json!({"endDateIso": "2027-03-01"});
        let dt = parse_polymarket_market_resolution_date(&market).unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2027, 3, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn polymarket_enddate_missing_yields_none() {
        let market = serde_json::json!({"id": "x", "question": "no date"});
        assert!(parse_polymarket_market_resolution_date(&market).is_none());
    }

    #[test]
    fn polymarket_closed_time_fallback() {
        let market = serde_json::json!({
            "closedTime": "2026-05-01T12:00:00Z",
            "question": "q"
        });
        let dt = parse_polymarket_market_resolution_date(&market).unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-05-01T12:00:00+00:00");
    }

    #[test]
    fn polymarket_uma_end_date_iso_fallback() {
        let market = serde_json::json!({"umaEndDateIso": "2026-08-15"});
        let dt = parse_polymarket_market_resolution_date(&market).unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2026, 8, 15, 0, 0, 0).unwrap());
    }

    #[test]
    fn kalshi_uses_expiration_time_rfc3339() {
        let market = serde_json::json!({
            "ticker": "TEST-26",
            "expiration_time": "2026-06-15T12:00:00-04:00"
        });
        let dt = parse_kalshi_market_resolution_date(&market).expect("expiration_time 应解析成功");
        assert_eq!(dt.to_rfc3339(), "2026-06-15T16:00:00+00:00");

        let wrong = serde_json::json!({
            "endDate": "2026-06-15T12:00:00Z",
        });
        assert!(
            parse_kalshi_market_resolution_date(&wrong).is_none(),
            "Kalshi 不使用 Polymarket 的 endDate"
        );
    }

    #[test]
    fn kalshi_prefers_expected_expiration_to_align_with_pm_enddate() {
        let market = serde_json::json!({
            "close_time": "2026-04-07T12:00:00Z",
            "expected_expiration_time": "2026-03-24T18:00:00Z",
            "expiration_time": "2026-03-01T00:00:00Z",
            "latest_expiration_time": "2026-06-01T00:00:00Z",
        });
        let dt = parse_kalshi_market_resolution_date(&market).unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-03-24T18:00:00+00:00");
    }

    #[test]
    fn kalshi_falls_back_close_then_expiration_then_latest() {
        let no_expected = serde_json::json!({
            "close_time": "2026-03-15T00:00:00Z",
            "expiration_time": "2026-03-01T00:00:00Z",
            "latest_expiration_time": "2026-06-01T00:00:00Z",
        });
        let dt = parse_kalshi_market_resolution_date(&no_expected).unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-03-15T00:00:00+00:00");

        let only_exp = serde_json::json!({
            "expiration_time": "2026-03-01T00:00:00Z",
            "latest_expiration_time": "2026-06-01T00:00:00Z",
        });
        let dt2 = parse_kalshi_market_resolution_date(&only_exp).unwrap();
        assert_eq!(dt2.to_rfc3339(), "2026-03-01T00:00:00+00:00");
    }

    #[test]
    fn kalshi_expiration_time_missing_yields_none() {
        let market = serde_json::json!({"ticker": "X"});
        assert!(parse_kalshi_market_resolution_date(&market).is_none());
    }
}

/// 真实 API 烟雾测（需外网）：`cargo test live_api_ -- --ignored`
#[cfg(test)]
mod live_api_resolution_smoke {
    use super::*;
    use reqwest::Client;

    #[tokio::test]
    #[ignore]
    async fn live_polymarket_markets_has_resolution_field() {
        let hc = Client::new();
        let arr: Vec<Value> = hc
            .get("https://gamma-api.polymarket.com/markets")
            .query(&[("limit", "3"), ("active", "true")])
            .send()
            .await
            .expect("Polymarket 网络请求失败")
            .json()
            .await
            .expect("json");
        let m = arr.first().expect("至少一条");
        let parsed = parse_polymarket_market_resolution_date(m);
        assert!(
            parsed.is_some(),
            "预期存在 endDate 或 endDateIso，实际 keys: {:?}",
            m.as_object()
                .map(|o| o.keys().map(|k| k.as_str()).collect::<Vec<_>>())
        );
        let id = m["id"].as_str().expect("id");
        let pm = PolymarketClient::new();
        let via_id = pm.fetch_resolution_by_market_id(id).await;
        assert!(
            via_id.is_some(),
            "GET /markets?id={} 应能补全到期",
            id
        );
    }

    #[tokio::test]
    #[ignore]
    async fn live_kalshi_markets_has_resolution_field() {
        let hc = Client::new();
        let v: Value = hc
            .get("https://api.elections.kalshi.com/trade-api/v2/markets")
            .query(&[
                ("status", "open"),
                ("limit", "2"),
                ("mve_filter", "exclude"),
            ])
            .send()
            .await
            .expect("Kalshi 网络请求失败")
            .json()
            .await
            .expect("json");
        let m = &v["markets"][0];
        assert!(
            parse_kalshi_market_resolution_date(m).is_some(),
            "预期能自列表项解析到期；keys: {:?}",
            m.as_object()
                .map(|o| o.keys().map(|k| k.as_str()).collect::<Vec<_>>())
        );
        let ticker = m["ticker"].as_str().expect("ticker");
        let ks = KalshiClient::new();
        let via_detail = ks.fetch_resolution_by_ticker(ticker).await;
        assert!(via_detail.is_some(), "GET /markets/{{ticker}} 应能补全到期");
    }
}