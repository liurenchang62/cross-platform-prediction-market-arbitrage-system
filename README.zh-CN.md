# `trader` — Kalshi ↔ Polymarket 跨所执行栈

[English](README.md) | **简体中文**

## 本仓库是什么

**`trader`** 是主 **Rust** 程序：在 **Polymarket** 与 **Kalshi** 之间做文本与类别匹配（`config/categories.toml`），维护追踪对，用**实时订单簿**做可执行盈亏评估，校验通过后可下**实盘单**（Kalshi IOC / Polymarket CLOB FOK），成交与摘要写入 **`logs/desk_events.jsonl`**。

**`portfolio-monitor`** 是同工作区下的配套 crate：**只读**组合工具（快照、现金流、利润汇总、换手图、PM 赎回诊断）以及供 `trader` 启动时调用的 **`pm_relayer_automation`** 库逻辑。

密钥通过**环境变量**（及本机私钥文件）配置；具体命令与环境说明见 **`PROJECT_COMMANDS.txt`**。

---

## 工作区结构

| 路径 | 作用 |
|------|------|
| **`Cargo.toml`** | 工作区成员：**`trader`**、**`portfolio_monitor`** |
| **`trader/`** | 实盘主循环、匹配、下单、会话风控、飞书推送等 |
| **`portfolio_monitor/`** | 库 + 独立工具：`portfolio-snapshot`、`portfolio-cashflow`、`portfolio-profit`、`portfolio-turnover`、`portfolio-pm-redeem-diagnose` |
| **`config/categories.toml`** | 分类与关键词，参与匹配 |
| **`logs/`** | `desk_events.jsonl`、`arb_rounds_store.json`、`trades_cache.txt`（路径见 `system_params.rs` 与 `trades_cache.rs`） |

核心实现位于 **`trader/src/`**：`main.rs`、`market_matcher.rs`、`arbitrage_detector.rs`、`validation.rs`、`session_risk.rs`、`desk_events.rs`、`kalshi_trade.rs`、`polymarket_trade.rs`、`feishu_push.rs`、`system_params.rs` 等。

---

## 快速运行

```bash
cargo run -p trader --release
```

常用：

```bash
cargo run -p trader --bin check_env --release
cargo run -p portfolio-monitor --bin portfolio-snapshot --release
```

`trader` 仅在**进程启动、进入主循环前**尝试一次经 Relayer 的 PM 赎回与 CLOB Collateral 刷新（需 `POLYMARKET_SIGNATURE_TYPE=1` 及 Relayer 相关环境变量）；后续监控**周期内不再重复**。

---

## 环境要求

- **Rust** stable，**edition 2021**
- 能访问 Kalshi、Polymarket 相关 HTTP API（以代码中实现为准）

---

## 免责声明

仅供研究与学习；不构成投资建议。实盘结果与模型快照可能不一致；请遵守各平台条款与所在地法规。

## 许可

若仓库根目录无 `LICENSE` 文件，则默认保留所有权利；若对外分发请自行补充许可文件。
