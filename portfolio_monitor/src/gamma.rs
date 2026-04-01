use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

const GAMMA: &str = "https://gamma-api.polymarket.com";

async fn market_row(http: &Client, pm_market_id: &str) -> Result<Option<Value>> {
    let url = format!("{}/markets", GAMMA);
    let arr: Vec<Value> = http
        .get(&url)
        .query(&[("id", pm_market_id), ("limit", "1")])
        .send()
        .await
        .context("Gamma markets")?
        .json()
        .await
        .context("Gamma json")?;
    Ok(arr.into_iter().next())
}

pub async fn pm_market_title(http: &Client, pm_market_id: &str) -> Result<String> {
    let m = market_row(http, pm_market_id)
        .await?
        .context("Polymarket market id not found")?;
    Ok(m["question"]
        .as_str()
        .or_else(|| m["title"].as_str())
        .unwrap_or("(no title)")
        .to_string())
}

/// 跟踪口径下的预期到期展示（Gamma `endDate` / `endDateIso`）。
pub async fn pm_market_expiry_display(http: &Client, pm_market_id: &str) -> String {
    match market_row(http, pm_market_id).await {
        Ok(Some(m)) => m["endDate"]
            .as_str()
            .or_else(|| m["endDateIso"].as_str())
            .unwrap_or("—")
            .to_string(),
        _ => "—".to_string(),
    }
}

pub async fn pm_market_closed(http: &Client, pm_market_id: &str) -> Result<bool> {
    Ok(market_row(http, pm_market_id)
        .await?
        .map(|m| m["closed"].as_bool().unwrap_or(false))
        .unwrap_or(false))
}
