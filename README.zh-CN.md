# 跨平台套利系统

[English](README.md) | **简体中文**

> **Kalshi**（做空 NO 腿）↔ **Polymarket**（做多 YES 腿）预测市场实盘套利。
> 仅供研究与学习，不构成投资建议。

---

## 简介

本项目为 Rust Cargo 工作区，包含两个 crate：

| Crate | 作用 |
|-------|------|
| **`trader`** | 实盘主循环：市场匹配、订单簿经济模型、下单执行（Kalshi IOC + Polymarket CLOB FOK）、会话风控、飞书推送 |
| **`portfolio-monitor`** | 只读分析 + 一次性工具：组合快照、盈亏汇总、现金流、换手图表、赎回/转换工具 |

**工作流程：**
1. 每隔 90 分钟全量拉取两个平台的开放市场。
2. TF-IDF 余弦相似度 + `config/categories.toml` 关键词规则匹配市场对。
3. 可选：调用 **DeepSeek LLM** 对可疑匹配做二次验证。
4. 对每个追踪对实时拉取订单簿，进行费用/ROI/资金上限评估。
5. 机会通过全部门槛后，同时提交 Kalshi IOC 与 Polymarket FOK。
6. 成交写入 `logs/desk_events.jsonl`；飞书机器人推送摘要。

---

## 仓库结构

```
cross_platform_arbitrage_system_livetrading/
├── Cargo.toml                   # 工作区：trader + portfolio_monitor
├── config/
│   └── categories.toml          # 匹配关键词分类
├── env/                         # 凭证与配置（应加入 .gitignore）
│   ├── env_params.env           # 加载开关（LOAD_KEY_THROUGH_ENV）
│   ├── proxy.env                # HTTP/HTTPS 代理（Clash / V2Ray / ClashVerge）
│   ├── llm.env                  # DeepSeek API Key 与模型配置
│   ├── kalshi.env               # Kalshi API Key + RSA 私钥路径
│   └── polymarket.env           # Polymarket 私钥 + Relayer 凭证
├── logs/                        # 运行输出（自动创建）
│   ├── desk_events.jsonl        # 成交台账（追加写入）
│   ├── arb_rounds_store.json    # 每对套利轮次计数（跨会话持久化）
│   ├── naked_positions.jsonl    # 紧急：未对冲裸仓记录
│   └── trades_cache.txt         # Kalshi 交易缓存，供对账使用
├── trader/
│   └── src/
│       ├── main.rs              # 主循环、实盘门控、启动 Relayer 调用
│       ├── system_params.rs     # 全部常量 + env 文件加载器
│       ├── market_matcher.rs    # TF-IDF 匹配引擎
│       ├── arbitrage_detector.rs
│       ├── validation.rs        # LLM + 规则双重验证
│       ├── session_risk.rs      # 资金基准 + 现金预警
│       ├── kalshi_trade.rs      # Kalshi IOC 下单
│       ├── polymarket_trade.rs  # Polymarket FOK 下单
│       ├── feishu_push.rs       # 飞书 Webhook 推送
│       ├── desk_events.rs       # 成交台账写入
│       └── bin/
│           ├── check_env.rs     # 环境变量 / 凭证自检
│           └── pm_redeem.rs     # 独立 PM 赎回 + Collateral 刷新
└── portfolio_monitor/
    └── src/
        ├── pm_relayer_automation.rs  # Relayer：赎回、授权、包装、FOK
        └── bin/
            ├── snapshot.rs           # Kalshi + PM 统一组合快照
            ├── cashflow.rs           # 现金流 CSV 导出
            ├── profit.rs             # 利润汇总
            ├── turnover.rs           # 换手图表（PNG）
            ├── arb_reconcile.rs      # 套利三时点回报对比
            ├── position_markets.rs   # 当前持仓的实时订单簿
            ├── close_position.rs     # 定向平仓（默认 dry-run）
            ├── pm_redeem_diagnose.rs # 链上 payout 诊断（不提交）
            └── pm_wrap_collateral.rs # USDC.e → pUSD 自动转换
```

---

## 环境配置

### 第一步 — 创建 `env/` 文件

程序自动从 `env/` 目录下的五个文件中加载凭证。按照下方模板填写：

**`env/env_params.env`**
```ini
# true（默认）：从 env/ 文件加载凭证
# false：跳过文件，完全依赖系统环境变量（服务器/CI 模式）
LOAD_KEY_THROUGH_ENV=true
```

**`env/proxy.env`**
```ini
# Clash / V2Ray / ClashVerge 默认端口 7897
# 若在服务器上运行或不需要代理，请注释掉以下两行
HTTP_PROXY=http://127.0.0.1:7897
HTTPS_PROXY=http://127.0.0.1:7897
```

**`env/llm.env`**
```ini
LLM_PROVIDER=deepseek
DEEPSEEK_API_KEY=<你的 DeepSeek API Key>
LLM_MODEL=deepseek-chat
LLM_TIMEOUT_MS=10000
LLM_MAX_TOKENS=400
LLM_TEMPERATURE=0.0
```

**`env/kalshi.env`**
```ini
KALSHI_API_KEY_ID=<Kalshi 设置页面生成的 UUID>
KALSHI_PRIVATE_KEY_PATH=C:\路径\kalshi_rsa_private.txt
# 可选：每周期打印订单簿拉取细节
# KALSHI_ORDERBOOK_DEBUG=1
```

**`env/polymarket.env`**
```ini
POLYMARKET_SIGNATURE_TYPE=1
POLYMARKET_PRIVATE_KEY_PATH=C:\路径\pm_private_key.txt
POLYMARKET_API_KEY=<CLOB API Key>
POLYMARKET_ADDRESS=<代理钱包地址>
RELAYER_API_KEY=<与 POLYMARKET_API_KEY 相同>
RELAYER_API_KEY_ADDRESS=<与 POLYMARKET_ADDRESS 相同>
# POLYGON_RPC_URL=https://polygon-rpc.com      # 可选，默认使用公共 RPC
# POLYMARKET_RELAYER_URL=https://relayer-v2.polymarket.com  # 可选
```

### 第二步 — 私钥文件格式

| 平台 | 格式 | 说明 |
|------|------|------|
| Kalshi | RSA PEM 私钥，`.txt` 或 `.pem` | 在 Kalshi Settings → API Keys 生成并下载 |
| Polymarket | EVM 私钥 hex，单行，可带或不带 `0x` | 代理钱包私钥 |

### 第三步 — 优先级规则

- 系统环境变量**始终优先**于 `env/` 文件（文件不会覆盖已设置的系统变量）。
- 将 `LOAD_KEY_THROUGH_ENV=false` 可完全跳过文件加载，改用系统环境变量（服务器部署推荐）。

---

## 快速开始

```bash
# 1. 验证凭证与网络连通性
cargo run -p trader --bin check_env --release

# 2. 首次运行前，将代理钱包中的 USDC.e 转换为 pUSD（Polymarket V2 要求）
cargo run -p portfolio-monitor --bin pm-wrap-collateral --release

# 3. 启动实盘套利主程序
cargo run -p trader --release
```

启动时，`trader` 会自动执行一次 Relayer 赎回 + Collateral 刷新（需 `POLYMARKET_SIGNATURE_TYPE=1`），完成后进入主监控循环。

---

## 交易参数

核心参数位于 `trader/src/system_params.rs`：

| 参数 | 数值 | 含义 |
|------|------|------|
| `PER_LEG_CAP_FRAC_OF_CAPITAL` | 1.5% | 单腿最大名义金额（相对参考总资金） |
| `PAIR_CAP_FRAC_OF_CAPITAL` | 3% | 每对最大名义金额（双腿合计） |
| `LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP` | 10% | 净利润 ROI 门槛（净利 / 实际部署资金） |
| `MAX_PAIR_ARB_ROUNDS` | 3 | 同一市场对最大套利轮次（跨会话累计） |
| `KALSHI_BALANCE_DEPLOYABLE_FRAC` | 70% | Kalshi 余额最大可用比例 |
| `MAX_SETTLEMENT_DATE_SKEW_DAYS` | 3 天 | 两腿结算日期最大允许偏差 |
| `IMMINENT_RESOLUTION_SKIP_MINS` | 10 分钟 | 跳过即将结算的市场对 |
| `FULL_FETCH_INTERVAL_WALL_SECS` | 90 分钟 | 全量重匹配间隔 |
| `ARB_TRACK_CONCURRENCY` | 40 | 每周期并行订单簿查询数 |

---

## Polymarket V2 迁移（2026 年 4 月 28 日）

Polymarket 将 CLOB 保证金从 **USDC.e → pUSD**。

- SDK 升级至 **`polymarket_client_sdk_v2 = "0.5.1"`**
- pUSD 合约地址（Polygon）：`0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB`
- CollateralOnramp 合约（Polygon）：`0x93070a847efEf7F70739046A929D47a521F5B8ee`
- 若代理钱包中仍有 USDC.e，运行 `pm-wrap-collateral` 可自动转换。

---

## 飞书通知

`trader/src/feishu_push.rs` 在成交、现金预警、Relayer 事件时推送飞书消息。

在 `env/llm.env` 中可配置自定义 Webhook：
```ini
FEISHU_WEBHOOK=https://open.feishu.cn/open-apis/bot/v2/hook/<你的-id>
FEISHU_BOT_AUTHORIZATION=<你的-token>
```
不配置则使用源码内置的默认 Webhook。

---

## 环境要求

- **Rust** stable（edition 2021）
- 能访问 Kalshi REST API 与 Polymarket Gamma / CLOB / Data APIs
- 推荐代理节点：日本、韩国、印尼（交易所对网络稳定性要求较高）

---

## 免责声明

仅供研究与学习，不构成投资建议。实盘结果与模型快照可能存在差异；请遵守各平台服务条款及所在地法律法规。
