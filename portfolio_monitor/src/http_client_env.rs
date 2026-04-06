//! 按环境变量为 `reqwest::Client` 配置可选 HTTP 代理（与 curl 常见变量对齐）。

use std::time::Duration;

use reqwest::{Client, ClientBuilder};

fn normalize_proxy_url(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

/// 读取首个非空代理地址：`HTTPS_PROXY` / `https_proxy` / `ALL_PROXY` / `all_proxy` / `HTTP_PROXY` / `http_proxy`。
/// 若为 `host:port` 且无 scheme，则补全为 `http://`。
pub fn proxy_url_from_env() -> Option<String> {
    const KEYS: &[&str] = &[
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    for k in KEYS {
        if let Ok(v) = std::env::var(k) {
            let n = normalize_proxy_url(&v);
            if !n.is_empty() {
                return Some(n);
            }
        }
    }
    None
}

/// 若环境变量中有合法代理，则挂到 builder 上；非法 URL 时打印告警并保留原 builder。
pub fn apply_reqwest_proxy_from_env(mut b: ClientBuilder) -> ClientBuilder {
    if let Some(url) = proxy_url_from_env() {
        match reqwest::Proxy::all(&url) {
            Ok(p) => {
                eprintln!("🌐 reqwest 使用代理: {url}");
                b = b.proxy(p);
            }
            Err(e) => eprintln!("⚠️ 代理 URL 无效，已忽略: {url} ({e})"),
        }
    }
    b
}

/// 与实盘 `KalshiClient` / `PolymarketClient` 一致的连接池与超时；代理来自环境变量。
pub fn build_http_client(timeout: Duration) -> Result<Client, reqwest::Error> {
    let b = Client::builder()
        .timeout(timeout)
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(Duration::from_secs(90));
    apply_reqwest_proxy_from_env(b).build()
}
