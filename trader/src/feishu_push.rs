//! 成功实盘成交后调用项目根目录 `【消息推送】飞书转发.py` 推送摘要（失败仅打日志，不中断主流程）。

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::system_params::DESK_EVENTS_JSONL;

fn project_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = trader/（crate 子目录）；Python 脚本在 workspace 根目录（上一级）
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root exists")
        .to_path_buf()
}

fn feishu_script_path() -> PathBuf {
    project_root().join("【消息推送】飞书转发.py")
}

/// 第一腿成交且第二腿有明确结果时推送导师端飞书。
pub fn feishu_should_notify(status: &str) -> bool {
    matches!(
        status,
        "OK" | "UNWIND_COMPARE" | "UNWIND_TIMEOUT" | "UNWIND_TIMEOUT_PARTIAL"
    )
}

/// 通过 **stdin** 将整段 UTF-8 摘要传给脚本（避免 Windows 命令行长度限制）。
pub fn push_trade_summary_to_feishu(summary: &str) -> Result<()> {
    let script = feishu_script_path();
    if !script.exists() {
        anyhow::bail!("飞书脚本不存在: {:?}", script);
    }
    let summary = summary.trim().replace('\r', "");
    if summary.is_empty() {
        anyhow::bail!("飞书摘要为空");
    }
    if summary.len() > 8000 {
        anyhow::bail!("飞书摘要过长 (>8000)，请缩短");
    }

    let attempts: &[(&str, &[&str])] = &[
        ("py", &["-3", "-u"]),
        ("python", &["-u"]),
        ("python3", &["-u"]),
    ];

    let mut last: Option<String> = None;
    for (exe, prefix) in attempts {
        let mut cmd = Command::new(exe);
        for p in *prefix {
            cmd.arg(p);
        }
        cmd.arg(&script);
        cmd.arg("-");
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.current_dir(project_root());
        cmd.env("PYTHONUTF8", "1");

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                last = Some(format!("{} 启动失败: {}", exe, e));
                continue;
            }
        };
        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                last = Some(format!("{} 无 stdin", exe));
                continue;
            }
        };
        stdin
            .write_all(summary.as_bytes())
            .with_context(|| format!("写入飞书脚本 stdin ({exe})"))?;
        drop(stdin);

        let out = child
            .wait_with_output()
            .with_context(|| format!("等待飞书子进程 ({exe})"))?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if !s.trim().is_empty() {
                println!("   [飞书] {}", s.trim());
            }
            return Ok(());
        }
        last = Some(format!(
            "{} exit={} stderr={}",
            exe,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    anyhow::bail!(
        "飞书推送失败（需 Python3 + requests，且仓库根目录有 飞书转发.py）。详情: {:?}",
        last
    )
}

/// 生成飞书文本（关键词由 Python 侧确保包含「【机器人】」）。
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
