# 跨平台套利系统

[English](README.md) | **简体中文**

一套面向 **Kalshi** 与 **Polymarket** 的生产级跨所二元合约对冲监控与执行系统。系统持续接入两所全量开放市场，通过相似度检索对齐经济等价合约，基于完整订单簿卖档阶梯评估装箱对冲成本，在风险门控通过后以交易所原生订单语义并行执行双腿下单，并结构化记录成交差异与事后对账。每笔交易的目标是在结算时每份配对合约合计兑付约 **1 美元**。

> 仅供研究与学习，不构成投资建议。

---

## 系统架构

整条管线作为跨两个独立中央限价订单簿的闭环运行：

```
市场拉取 → 相似度匹配 → 双取向扩展 → 订单簿拉取 → 阶梯检测 → 并行执行 → 对账 → 台账
```

**全量周期刷新。** 两所市场目录通过分页 REST 持续拉取，单边上限可达数万级合约。周期性全量刷新（默认每 90 分钟一次，`FULL_FETCH_INTERVAL_WALL_SECS`）将新上市 ticker 强制送入匹配索引，防止覆盖漂移——即市场 ID 滚动后匹配逻辑长期附着在已失效的标识符上。

**配对生成。** 基于市场描述的 TF-IDF 余弦相似度与精选关键词分类（`config/categories.toml`）共同产出候选配对；与常量 `SIMILARITY_THRESHOLD`（0.90）比较后筛掉弱匹配。可选接入 DeepSeek 相关校验通路（`validation.rs` / `llm_validator.rs`），处理边界情形。

**双取向扩展。** 每个被接受的配对自动展开为两个互补追踪标识——`Kalshi YES + Polymarket NO` 与 `Kalshi NO + Polymarket YES`——使两种经济等价的对冲方向同时进入监控队列（`main.rs` 中 `expand_dual_orientations`）。这消除了一个反复出现的手工运维风险：人工维护的取向对照表与实盘簿脱节。

**阶梯检测。** 每个心跳周期在两所重建可见卖档阶梯。检测器沿价位逐级扫量，推导 Kalshi IOC 所需的最差成交限价以及 Polymarket FAK 的资金预算（`arbitrage_detector.rs`）。Kalshi 的整数合约份数在此阶段锁定，确保识别层与执行层使用同一套数量模型——消除「模型按小数计算、交易所按整数成交」的经典断层。

**并行执行。** 通过验证的机会在单个 `tokio::join!` 屏障上同时发出：Kalshi IOC 与 Polymarket FAK 买单并行投递（`validate_arbitrage_pair_post_success`）。墙钟延迟由较慢一侧的往返时延决定，而非两条 REST 链路的首尾串接。在争夺稀薄流动性的场景下，这直接换算为更短的暴露窗口。

**成交对账。** 两所撮合引擎相互独立，成交数量很少精确对称。当一侧相对另一侧超额成交时，系统在名义金额超过清理阈值时通过交易所原生反向订单（Kalshi IOC 卖出、Polymarket FAK 卖出）平掉多余头寸。无法消化的残留差异写入 `logs/naked_positions.jsonl`（`NAKED_POSITIONS_JSONL`）。每笔已执行交易——含部分成交与平仓尝试——均以结构化、带时间戳的记录追加写入 `logs/desk_events.jsonl`（`DESK_EVENTS_JSONL`）。

---

## 工程实现

### 运行时模型

| 决策 | 实现 |
|------|------|
| 语言与并发 | 单 Rust 进程 + Tokio 异步运行时。行情轮询、RSA 签名、对账 Future 共享同一地址空间。热点路径上无托管运行时或解释器开销。 |
| 发布编译配置 | 工作区 `Cargo.toml`：`opt-level = 3`、`lto = true`、`codegen-units = 1`。以增量编译时间为代价，换取稳态运行下的最大吞吐——这类二进制程序的 KPI 是 HTTP 管线饱和度，而非频繁本地迭代的速度。 |
| 进程模型 | 单个操作系统进程扇出大量并发 HTTP 调用。无多进程协调开销。 |

### 匹配吞吐

TF-IDF 相似度检索将全所市场描述投影到联合向量空间。匹配器不以逐对迭代方式运行，而是采用 **512 行为一块**（`MATCH_MATMUL_CHUNK_ROWS`）的分块矩阵乘法，允许 ndarray 友好的密集计算内核对大体量市场快照进行批量处理，避免脚本式双重循环的开销。这使得在市场数量增长至数千级别时，全量重新匹配仍能保持在可控墙钟时间内。

### 阶梯忠实估价

| 方面 | 细节 |
|------|------|
| 价格来源 | 两所完整可见卖档阶梯——非最优报价快照 |
| IOC 限价推导 | 沿阶梯累积至目标名义金额所需数量后的最差挂单价 |
| FAK 预算 | 基于扫量路径上的实际 VWAP 确定名义规模，非乐观的簿顶报价 |
| 合约整数对齐 | Kalshi 整数合约份数从同一次阶梯扫量中产出；检测与执行在完全相同的数量上操作 |

### 互补装箱结构

检测器并列评估两种装箱组合：

| 组合 | 构成 |
|------|------|
| 取向 A | Kalshi YES 卖盘 + Polymarket NO 卖盘 |
| 取向 B | Kalshi NO 卖盘 + Polymarket YES 卖盘 |

扣除手续费、通过 ROI 门槛及流动性检查后仍有利可图的那一侧驱动执行。任一交易所均不被硬编码为「YES 侧」或「NO 侧」——该时刻订单簿的实际经济学决定哪个取向点火。

### 交易所原生费率

| 交易所 | 费率来源 | 刷新 / 兜底行为 |
|--------|----------|----------------|
| Kalshi | Trade API `/series/fee_changes`，按当前追踪的 series ticker 过滤解析（`kalshi_fee_schedule.rs`） | 在构建本轮 Kalshi `series_keys` 集合之后拉取：**全量配对校验批次**与**活跃追踪扫描周期**各触发一次——与校验节拍对齐，而非每一笔孤立订单簿 GET 都附带费率刷新 |
| Polymarket | CLOB `fee-rate` 动态 bps | HTTP 失败时退回保守硬编码下限 `PM_TAKER_FEE_BPS_WHEN_FETCH_FAILS`（250 bps） |

费率模型与上述校验节拍对齐刷新，使促销窗口和费率变更无需人工干预即可进入定价。Polymarket 的兜底值被刻意设为偏悲观——若静默假设零费率，会虚增表面优势并引致实盘亏损风险。

### 并发塑形

| 参数 | 值 | 作用 |
|------|-----|------|
| `ARB_TRACK_CONCURRENCY` | 40 | 限制同一扫描周期内并行展开的订单簿驱动校验任务数 |
| 实现要点 | `main.rs` 中对异步流使用 Tokio `buffer_unordered(conc)` | 防止瞬时海量未完成 HTTP 任务拖垮本机资源；Kalshi 订单簿侧另有 `KALSHI_OB_RATE_LIMIT_PER_SEC`（10 req/s）约束拉簿节奏 |

系统不会为每个市场对无界生成任务，而是用固定并发窗口把 burst 摊平成可控扇面，兼顾文件描述符上限与交易所限速惯例。

### 安全门控

```
LIVE_TRADING_ON（编译期常量）→ 启动时 set_live_arbitrage_orders_allowed
    → 执行路径 require_live_orders_allowed() → 下单 / 卖平
```

`LIVE_TRADING_ON` 定义于 `system_params.rs`；`main` 在进入主循环前将其写入 `live_arbitrage_orders_allowed`。凡可能减少保证金余额的路径均调用 `require_live_orders_allowed()`（`kalshi_trade.rs`、`polymarket_trade.rs`、对账分支）。`LIVE_TRADING_ON=false` 时进程处于纯监控模式（机会可打印，不发真实委托），逻辑集中在编译期开关而非额外的交互确认层。

### 台账与轮次限制

| 记录 | 路径 / 格式 | 用途 |
|------|-------------|------|
| 主台账 | `logs/desk_events.jsonl`（`DESK_EVENTS_JSONL`），追加式 JSONL | 完整成交叙事：成交、部分成交、平仓尝试、时间戳 |
| 未对冲残仓 | `logs/naked_positions.jsonl`（`NAKED_POSITIONS_JSONL`），追加式 JSONL | 无法平仓的残留不平衡头寸 |
| 轮次计数 | `logs/arb_rounds_store.json`（`ARB_ROUNDS_STORE_PATH`），按配对持久化 | 跨进程记录成功执行轮次 |

轮次计数器跨进程重启强制实施 `MAX_PAIR_ARB_ROUNDS = 3`——阻止同一统计优势跨会话静默加仓。

---

## 运行时节律

| 节律 | 间隔 | 行为 |
|------|------|------|
| 全量重匹配 | 90 分钟（`FULL_FETCH_INTERVAL_WALL_SECS`） | 拉取完整目录、重建相似度投影、重新扩展取向、写入追踪状态 |
| 微结构轮询 | 主循环约 2 秒节拍（见 `main.rs` sleep）叠加网络耗时 | 在 `ARB_TRACK_CONCURRENCY` 上限内重建阶梯、复核活跃配对 |

**资金参数**（比例锚定在启动快照写入的 `session_reference_capital_usd`，见 `session_risk.rs` / `set_session_reference_capital_usd`）：

| 参数 | 值 | 约束 |
|------|-----|------|
| 单腿名义上限 | 1.5%（`PER_LEG_CAP_FRAC_OF_CAPITAL`） | 单交易所暴露上限 |
| 配对合计上限 | 3%（`PAIR_CAP_FRAC_OF_CAPITAL`） | 双腿合计天花板 |
| Kalshi 可部署上限 | 70%（`KALSHI_BALANCE_DEPLOYABLE_FRAC`） | 为赎回和并发成交留出缓冲 |
| 净 ROI 门槛 | 10%（`LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP`） | 净利润相对本轮实际占用资金 `capital_used` 必须高于该比例方可放行 |

**日历门控：**

| 门控 | 阈值 | 理由 |
|------|------|------|
| 结算时刻偏差 | 当 `abs(ΔUTC 秒) > MAX_SETTLEMENT_DATE_SKEW_DAYS × 86400`（3 天）即剔除；任一侧缺少 `resolution_date` 则跳过本检查 | 过滤久期严重错配的配对 |
| 临近到期跳过 | 任一侧距结算不足 `IMMINENT_RESOLUTION_SKIP_MINS`（10 分钟）整轮跳过 | 降低末端博弈噪声污染阶梯估价 |

---

## 快速开始

```bash
# 验证凭证与连通性
cargo run -p trader --bin check_env --release

# 将 USDC.e 转换为 pUSD（Polymarket V2 抵押链路；仅在仍需迁移时执行）
cargo run -p portfolio-monitor --bin pm-wrap-collateral --release

# 启动实盘监控 / 执行（是否真实下单由 system_params.rs 中 LIVE_TRADING_ON 决定）
cargo run -p trader --release
```

进程启动后，若配置代理钱包签名（常见写法为 `POLYMARKET_SIGNATURE_TYPE=1`），会经由 `portfolio_monitor::pm_relayer_automation::desk_hydrate_pm_proxy` 尝试一轮 Relayer 赎回与抵押刷新。凭证放在 `env/`（git 忽略）；**已存在于操作系统环境变量中的键不会被文件覆盖**（`load_env_file_if_missing`）。

---

## 仓库结构

```
cross_platform_arbitrage_system_livetrading/
├── Cargo.toml                   # 工作区：release LTO 配置
├── config/
│   └── categories.toml          # 匹配关键词分类
├── env/                         # 运维凭证（git 忽略）
├── logs/                        # 运行时：desk_events、残仓记录、轮次计数
├── trader/
│   └── src/
│       ├── main.rs              # 主循环、buffer_unordered 并发、下单汇合点
│       ├── system_params.rs     # 常量、节律、环境装载、LIVE_TRADING_ON 门控
│       ├── clients.rs           # Kalshi / Polymarket REST 客户端
│       ├── market.rs            # Market 模型
│       ├── market_filter.rs
│       ├── market_matcher.rs    # TF-IDF + 分块矩阵乘法
│       ├── arbitrage_detector.rs# 阶梯经济学、IOC/FAK 推导
│       ├── validation.rs        # 确定性规则 + 可选 LLM
│       ├── llm_validator.rs
│       ├── tracking.rs          # 双取向标识符
│       ├── tracking_log.rs
│       ├── session_risk.rs      # 流动性遥测 vs. 基线
│       ├── kalshi_trade.rs      # IOC 买卖 + RSA 签名 REST
│       ├── kalshi_fee_schedule.rs
│       ├── polymarket_trade.rs  # 认证 CLOB 封装（FAK 路径）
│       ├── fee_model.rs         # 费用与摩擦共用算术
│       ├── desk_events.rs       # 追加式 JSONL 台账写入
│       ├── trade_reconciliation.rs
│       ├── position_cleanup.rs  # 残仓清理
│       ├── trades_cache.rs
│       ├── feishu_push.rs       # 可选运营告警
│       ├── text_vectorizer.rs
│       ├── vector_index.rs
│       ├── category_vectorizer.rs
│       ├── category_mapper.rs
│       └── bin/
│           ├── check_env.rs     # 凭证自检
│           └── pm_redeem.rs     # 独立赎回工具
└── portfolio_monitor/
    └── src/
        ├── lib.rs
        ├── gamma.rs
        ├── kalshi.rs
        ├── pm.rs
        ├── mapping.rs
        ├── http_client_env.rs   # 共用 HTTP 客户端构建（trader 亦依赖）
        ├── snapshot_report.rs
        ├── cashflow.rs
        ├── profit.rs
        ├── turnover.rs
        ├── arb_reconcile.rs
        ├── pm_relayer_automation.rs
        └── bin/
            ├── portfolio-snapshot.rs
            ├── portfolio-cashflow.rs
            ├── portfolio-profit.rs
            ├── portfolio-turnover.rs
            ├── arb-reconcile.rs
            ├── portfolio-position-markets.rs
            ├── close-position.rs
            ├── portfolio-pm-redeem-diagnose.rs
            └── pm-wrap-collateral.rs
```

---

## 环境要求

- Rust stable（edition 2021）
- 到 Kalshi Trade API 与 Polymarket Gamma / CLOB 的稳定 HTTPS 连接
- 推荐代理区域：日本、韩国、印度尼西亚

---

## 免责声明

仅供研究与教学语境参考。不构成投资建议。实盘成交与模型估算必然存在偏差。使用者需自行遵守各交易所条款与属地法律法规。
