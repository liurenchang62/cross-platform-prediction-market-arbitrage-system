//! 飞书群自定义机器人文本推送（`msg_type: text`）。失败仅打日志，不中断主流程。
//!
//! 与历史 Python 脚本行为对齐：若机器人启用「关键词」校验，正文需含 `【机器人】`（发送前自动补上）。
//! Webhook / Authorization 可通过环境变量覆盖（便于不把密钥写死在仓库）：`FEISHU_WEBHOOK`、`FEISHU_BOT_AUTHORIZATION`。

use std::time::Duration;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use reqwest::blocking::Client;
use serde::Serialize;

use crate::system_params::DESK_EVENTS_JSONL;
use crate::trades_cache;

/// 机器人若开启「关键词」校验，正文需包含此串（与旧脚本一致）。
const FEISHU_KEYWORD: &str = "【机器人】";

/// 默认 webhook（可被 `FEISHU_WEBHOOK` 覆盖）。
const DEFAULT_FEISHU_WEBHOOK: &str =
    "https://open.feishu.cn/open-apis/bot/v2/hook/646392d3-c205-41fc-b7c0-01a342eac341";
/// 默认 `Authorization` 请求头值（可被 `FEISHU_BOT_AUTHORIZATION` 覆盖）。
const DEFAULT_FEISHU_AUTHORIZATION: &str = "tooilGCilgCxxhlub0Qtbb";

static FEISHU_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("feishu reqwest client")
});

#[derive(Serialize)]
struct FeishuTextPayload {
    msg_type: &'static str,
    content: FeishuTextContent,
}

#[derive(Serialize)]
struct FeishuTextContent {
    text: String,
}

fn feishu_webhook() -> String {
    std::env::var("FEISHU_WEBHOOK").unwrap_or_else(|_| DEFAULT_FEISHU_WEBHOOK.to_string())
}

fn feishu_authorization() -> String {
    std::env::var("FEISHU_BOT_AUTHORIZATION")
        .unwrap_or_else(|_| DEFAULT_FEISHU_AUTHORIZATION.to_string())
}

/// 第一腿成交且第二腿有明确结果时推送导师端飞书。
pub fn feishu_should_notify(status: &str) -> bool {
    matches!(
        status,
        "OK" | "UNWIND_COMPARE" | "UNWIND_TIMEOUT" | "UNWIND_TIMEOUT_PARTIAL"
    )
}

pub fn push_trade_summary_to_feishu(summary: &str) -> Result<()> {
    let summary = summary.trim().replace('\r', "");
    if summary.is_empty() {
        anyhow::bail!("飞书摘要为空");
    }
    if summary.len() > 8000 {
        anyhow::bail!("飞书摘要过长 (>8000)，请缩短");
    }

    let text = if summary.contains(FEISHU_KEYWORD) {
        summary
    } else {
        format!("{FEISHU_KEYWORD}{summary}")
    };

    let payload = FeishuTextPayload {
        msg_type: "text",
        content: FeishuTextContent { text },
    };

    let webhook = feishu_webhook();
    let resp = FEISHU_CLIENT
        .post(&webhook)
        .header("Authorization", feishu_authorization())
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .with_context(|| format!("飞书 webhook 请求 ({webhook})"))?;

    let status = resp.status();
    let body = resp.text().context("读取飞书响应体")?;
    if !status.is_success() {
        anyhow::bail!("飞书 HTTP {}: {}", status, body.trim());
    }

    if !body.trim().is_empty() {
        let line = format!("   [飞书] {}", body.trim());
        println!("{}", line);
        trades_cache::append_line(&line);
    }
    Ok(())
}

/// 生成飞书文本（关键词由 [`push_trade_summary_to_feishu`] 在发送前保证包含「【机器人】」）。
#[allow(clippy::too_many_arguments)]
pub fn format_trade_summary(
    pair_label: &str,
    trading_session_utc_date: &str,
    session_cycle_id: usize,
    global_tick: usize,
    cycle_phase: &str,
    pm_title: &str,
    kalshi_title: &str,
    pm_market_id: &str,
    kalshi_market_id: &str,
    pm_side: &str,
    kalshi_side: &str,
    status: &str,
    ks_order_id: &str,
    ks_client_oid: &str,
    log_contracts: f64,
    pm_orders_n: usize,
    ks_unwind_n: usize,
    net_profit_model: f64,
    capital_used_model: f64,
) -> String {
    let status_cn = match status {
        "OK" => "第二腿完成：PM 与 Kalshi 敞口已按目标对齐",
        "UNWIND_COMPARE" => "第二腿选择卖平 Kalshi（对冲 H 明显劣于卖平 U）",
        "UNWIND_TIMEOUT" => "时间窗结束：未完全对冲，已 IOC 卖平剩余 Kalshi",
        "UNWIND_TIMEOUT_PARTIAL" => "时间窗结束：已部分 PM 对冲，剩余 Kalshi 已卖平",
        _ => status,
    };
    format!(
        "【套利实盘成交】 ARBITRAGE LIVE FILL\n\
市场对: {}\n\
UTC会话日: {} | 会话内周期 #{} | 调度 tick {}\n\
阶段: {}\n\
策略: Polymarket 买 {} + Kalshi 买 {}\n\
执行结果: {} — {}\n\
— — —\n\
PM 标题: {}\n\
PM market_id: {}\n\
Kalshi 标题: {}\n\
Kalshi ticker: {}\n\
— — —\n\
Kalshi 主订单: order_id={}\n\
client_order_id: {}\n\
记录份数 contracts_n: {:.4}\n\
第二腿 Polymarket 成交笔数: {}\n\
Kalshi 卖平订单数(本窗): {}\n\
— — —\n\
识别阶段(模型): 净利≈${:.4} 探针成本≈${:.4}\n\
完整成交分项以本机终端「实盘核对」与 {} 为准。",
        pair_label,
        trading_session_utc_date,
        session_cycle_id,
        global_tick,
        cycle_phase,
        pm_side,
        kalshi_side,
        status,
        status_cn,
        pm_title,
        pm_market_id,
        kalshi_title,
        kalshi_market_id,
        ks_order_id,
        ks_client_oid,
        log_contracts,
        pm_orders_n,
        ks_unwind_n,
        net_profit_model,
        capital_used_model,
        DESK_EVENTS_JSONL,
    )
}
