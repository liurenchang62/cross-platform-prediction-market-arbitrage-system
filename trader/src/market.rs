// src/market.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct MarketPrices {
    pub yes: f64,
    pub no: f64,
    pub liquidity: f64,
    pub yes_ask: Option<f64>,
    pub no_ask: Option<f64>,
    pub last_price: Option<f64>,
}

impl MarketPrices {
    pub fn new(yes: f64, no: f64, liquidity: f64) -> Self {
        Self {
            yes,
            no,
            liquidity,
            yes_ask: None,
            no_ask: None,
            last_price: None,
        }
    }
    
    pub fn with_asks(mut self, yes_ask: f64, no_ask: f64, last_price: Option<f64>) -> Self {
        self.yes_ask = Some(yes_ask);
        self.no_ask = Some(no_ask);
        self.last_price = last_price;
        self
    }
    
    pub fn validate(&self) -> bool {
        (self.yes + self.no - 1.0).abs() < 0.01
    }
    
    pub fn yes_ask_or_fallback(&self) -> f64 {
        self.yes_ask.unwrap_or(self.yes)
    }
    
    pub fn no_ask_or_fallback(&self) -> f64 {
        self.no_ask.unwrap_or(self.no)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub platform: String,
    pub market_id: String,
    pub title: String,
    pub description: String,
    #[serde(with = "chrono::serde::ts_seconds_option")]
    pub resolution_date: Option<DateTime<Utc>>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub slug: Option<String>,
    pub token_ids: Vec<String>,
    pub outcome_prices: Option<(f64, f64)>,
    pub best_ask: Option<f64>,
    pub best_bid: Option<f64>,
    pub last_trade_price: Option<f64>,
    pub vector_cache: Option<Vec<f64>>,
    pub categories: Vec<String>,
    pub volume_24h: f64,
}

impl Market {
    pub fn new(
        platform: String,
        market_id: String,
        title: String,
        description: String,
    ) -> Self {
        Self {
            platform,
            market_id,
            title,
            description,
            resolution_date: None,
            category: None,
            tags: Vec::new(),
            slug: None,
            token_ids: Vec::new(),
            outcome_prices: None,
            best_ask: None,
            best_bid: None,
            last_trade_price: None,
            vector_cache: None,
            categories: Vec::new(),
            volume_24h: 0.0,
        }
    }
    
    pub fn with_resolution_date(mut self, date: DateTime<Utc>) -> Self {
        self.resolution_date = Some(date);
        self
    }
    
    pub fn with_category(mut self, category: String) -> Self {
        self.category = Some(category);
        self
    }
    
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }
    
    pub fn with_slug(mut self, slug: String) -> Self {
        self.slug = Some(slug);
        self
    }
    
    pub fn with_token_ids(mut self, token_ids: Vec<String>) -> Self {
        self.token_ids = token_ids;
        self
    }
    
    pub fn with_outcome_prices(mut self, yes: f64, no: f64) -> Self {
        self.outcome_prices = Some((yes, no));
        self
    }
    
    pub fn with_market_data(mut self, best_ask: f64, best_bid: f64, last_trade: Option<f64>) -> Self {
        self.best_ask = Some(best_ask);
        self.best_bid = Some(best_bid);
        self.last_trade_price = last_trade;
        self
    }
    
    pub fn slug_is_15m_crypto(&self) -> bool {
        self.slug
            .as_deref()
            .map(|s| s.contains("updown-15m"))
            .unwrap_or(false)
    }
    
    fn ticker_looks_15m_crypto(ticker: &str) -> bool {
        let lower = ticker.to_lowercase();
        let has_15m = lower.contains("15m");
        let has_coin = lower.contains("btc")
            || lower.contains("eth")
            || lower.contains("sol")
            || lower.contains("bitcoin")
            || lower.contains("ethereum")
            || lower.contains("solana");
        has_15m && has_coin
    }
    
    pub fn is_15m_crypto_market(&self) -> bool {
        if self.slug_is_15m_crypto() {
            return true;
        }
        let ticker = self.slug.as_deref().unwrap_or(&self.market_id);
        self.platform == "kalshi" && Self::ticker_looks_15m_crypto(ticker)
    }
    
    pub fn coin_from_slug(&self) -> Option<String> {
        if let Some(slug) = self.slug.as_deref() {
            if slug.contains("updown-15m") {
                let prefix = slug.split("-updown-15m").next()?;
                if !prefix.is_empty() {
                    return Some(prefix.to_lowercase());
                }
            }
        }
        let ticker = self.slug.as_deref().unwrap_or(&self.market_id).to_lowercase();
        if ticker.contains("btc") || ticker.contains("bitcoin") {
            return Some("btc".to_string());
        }
        if ticker.contains("eth") || ticker.contains("ethereum") {
            return Some("eth".to_string());
        }
        if ticker.contains("sol") || ticker.contains("solana") {
            return Some("sol".to_string());
        }
        None
    }
}