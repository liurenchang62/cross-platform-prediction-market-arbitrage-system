use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

const GAMMA: &str = "https://gamma-api.polymarket.com";

pub async fn pm_market_title(http: &Client, pm_market_id: &str) -> Result<String> {
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
    let m = arr.first().context("Polymarket market id not found")?;
    Ok(m["question"]
        .as_str()
        .or_else(|| m["title"].as_str())
        .unwrap_or("(no title)")
        .to_string())
}

pub async fn pm_market_closed(http: &Client, pm_market_id: &str) -> Result<bool> {
    let url = format!("{}/markets", GAMMA);
    let arr: Vec<Value> = http
        .get(&url)
        .query(&[("id", pm_market_id), ("limit", "1")])
        .send()
        .await?
        .json()
        .await
        .unwrap_or_default();
    let Some(m) = arr.first() else {
        return Ok(false);
    };
    let closed = m["closed"].as_bool().unwrap_or(false);
    Ok(closed)
}
