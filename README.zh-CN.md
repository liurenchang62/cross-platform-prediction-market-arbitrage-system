# 跨市场套利监控 (`arbitrage-monitor`)

[English](README.md) | **简体中文**

## 项目简介

`arbitrage-monitor` 是一款 **Rust** 实现的程序：在 **Polymarket** 与 **Kalshi** 之间基于文本相似度与类别规则发现可对齐的预测市场，维护高置信配对并周期性评估其**可执行层面的经济结果**。定价与盈亏以**实时订单簿**解析结果为依据；在严格校验通过且用户选择**实盘**后，经官方生产接口下单（**Kalshi IOC** 买入、**Polymarket CLOB FOK**），成交与请求摘要写入 **`logs/live_trades.csv`**。

整体流程包括：市场列表拉取、向量匹配与索引、全量刷新与增量追踪、以及控制台输出与上述 CSV。

**仓库内不包含秘钥**：Kalshi / Polymarket 凭据均通过**环境变量**（及本机 PEM / 私钥文件路径）在运行时注入，详见下文与 `.gitignore`（已忽略 `.env`、`secrets/` 等）。

## 功能概览

- **市场数据**：通过 Polymarket Gamma API、Kalshi Trade API 获取开放市场，分页与数量上限可配置。
- **智能匹配**：TF-IDF 风格文本向量与余弦相似度；结合 `config/categories.toml` 中的类别关键词做约束与加权。

### 匹配规模：初筛命中 vs 最终对数

全量匹配日志里一般会看到三段：**初筛原始命中**（双向 Top-K 行命中，含重复）、**按市场对去重后的候选对数**（同一 `(PM id, Kalshi id)` 保留最大余弦）、**统一二筛并过置信度门槛后的最终对数**。若「去重后对」已达数千，而最终只有约百对量级，通常说明 **二筛规则与分数门限吃掉了绝大部分假阳性**，而不是分块矩阵乘在初筛阶段把配对弄丢。要提高召回，需要调参（如 `SIMILARITY_TOP_K`、`SIMILARITY_THRESHOLD`、类别覆盖与 `categories.toml`），而不是当作批处理匹配实现故障。

- **订单簿场景盈亏**：对每个候选配对，在双方**卖档深度**上按**单腿探针上限**与**每对名义上限**模拟吃单；**手续费**按 Kalshi / Polymarket taker 公式计算，并扣除**链上执行成本（估）**与**摩擦成本（friction，原盈余缓冲）**，再与**按 `pair_cap` 比例的最小净利门槛**比较（见 **订单簿盈亏与成本模型**）。
- **状态追踪**：维护追踪列表，按参数进行全量重建与周期更新（`src/system_params.rs`）。
- **实盘日志**：实盘模式下持久化 **`logs/live_trades.csv`**。
- **实盘执行**：第一腿 Kalshi IOC，第二腿 Polymarket FOK；第二腿每次业务尝试**重拉双边簿**并重算净利（业务次数上限见 `main.rs` 常量）。第二腿失败后可选止损路径：放宽 PM 对冲 vs Kalshi IOC 卖平。
- **辅助二进制**：`kalshi_balance`（生产余额）、`check_env`（环境/密钥加载检查）、`live_account_positions`、`smoke_live_orders`（见 **可选二进制工具**）。

## 订单簿盈亏与成本模型

用于展示与排序的场景（如周期 Top 10、`net_profit_100`）在代码中定义为：

1. **订单簿快照**  
   对每个已匹配市场对，程序通过 HTTP 获取双方**当前订单簿**，将可买流动性解析为按价格**升序排列的卖档** `(价格, 数量)`。

2. **单腿探针上限与每对名义上限**  
   **`TOTAL_CAPITAL_USD` × `PER_LEG_CAP_FRAC_OF_CAPITAL`** 为每腿探针名义；**`TOTAL_CAPITAL_USD` × `PAIR_CAP_FRAC_OF_CAPITAL`** 为 **`pair_cap`**（两腿合计名义上界）。见 `src/system_params.rs` 的 `arb_probe_caps()`。若按簿walk得到的两腿总成本超过 `pair_cap`，会对规模 **n** 做比例收缩后再精算。

3. **对冲规模**  
   取两腿可成交份数的**较小值**作为统一规模 **n**（已考虑 `pair_cap` 收缩）。

4. **规模 n 下的成本与利润**  
   - **交易手续费**（`src/fee_model.rs`）：  
     - **Kalshi taker**：\( \lceil 100 \cdot 0.07 \cdot m \cdot C \cdot P(1-P) \rceil / 100 \) 美元，\(m\) 为系列乘数，\(P\) 为美元价。  
     - **Polymarket taker**：\( C \cdot p \cdot (\text{bps}/10^4) \cdot (p(1-p))^{\text{exponent}} \) USDC（舍入见实现）；bps 来自 CLOB `fee-rate`，exponent 来自 `categories.toml` 大类映射。  

   - **非交易摩擦**：  
     - **链上执行成本（估）**：固定 **`LIVE_ARBIT_GAS_ASSUMED_USD`**（HTTP/CLOB 主路径下极小上界）。  
     - **摩擦成本（friction cost）**：**`pair_cap` × `LIVE_ARBIT_SURPLUS_BUFFER_FRAC_OF_PAIR_CAP`**（默认 **pair_cap 的 0.1%**）。  

   - **净利润**（概念式）：兑付面额相关毛收入 − **`capital_used`** − PM/KS 手续费 − (链上成本估 + 摩擦成本)。  
   - **可接受最小净利润**：**`pair_cap` × `LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP`**（默认 **pair_cap 的 0.2%**）；须 **净利 > 该门槛**（`src/arbitrage_detector.rs` 中 `ArbitrageDetector`）。

**实现位置**：`src/main.rs` 的 `validate_arbitrage_core` / `validate_arbitrage_pair`；`src/arbitrage_detector.rs` 的 `calculate_arbitrage_100usdt`、`validate_opportunity_from_ladders` 及滑点相关函数。

## 环境要求

- **Rust** 1.70+（建议使用 stable，`edition = "2021"`）
- 可访问上述公开 API（若端点或合规策略变更，请以平台最新文档为准）

## 快速开始

```bash
cargo run              # 调试
cargo run --release    # 长时间运行建议
```

启动时在交互中选择**仅跟踪**或**实盘**；实盘需配置下文环境变量并确认。首次运行前请确保存在 **`config/categories.toml`**（仓库已附带）。

## 可选二进制工具

| 命令 | 作用 |
|------|------|
| `cargo run --bin kalshi_balance` | 生产环境 Kalshi `GET /portfolio/balance`（RSA 鉴权）。 |
| `cargo run --bin check_env` | 检查环境变量与密钥加载（输出脱敏）。 |
| `cargo run --bin live_account_positions` | 账户/持仓类诊断（详见该 bin 内注释）。 |
| `cargo run --bin smoke_live_orders` | 受控实盘冒烟（谨慎使用）。 |

## API 探针脚本（Windows）

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\verify_apis.ps1
```

仅公开只读接口；JSON 输出至 `tmp_api_probe/`（已被 `.gitignore` 忽略）。

## 配置说明

| 路径 | 说明 |
|------|------|
| `config/categories.toml` | 类别名称、权重、关键词，用于分类与匹配 |
| `src/system_params.rs` | HTTP/匹配/时间窗口、参考总盘子、`pair_cap`/单腿比例、Gas、摩擦与最小净利比例、实盘 CSV 路径、环境变量名常量 |
| `src/fee_model.rs` | 双边 taker 公式、`ArbCostParams`、PM exponent 表 |

### 环境变量

- **Kalshi（下单/余额等）**：`KALSHI_API_KEY_ID`、`KALSHI_PRIVATE_KEY_PATH`（PEM 文件路径）。名称见 `src/system_params.rs`。
- **Polymarket CLOB**：`POLYMARKET_PRIVATE_KEY`（hex，与官方 SDK 一致）**或** `POLYMARKET_PRIVATE_KEY_PATH`（单行 hex 文本文件）；可选 `POLYMARKET_SIGNATURE_TYPE`（`0`/`1`/`2`）。
- **数据（可选）**：`POLYMARKET_TAG_SLUG`：Polymarket 拉取按 tag 过滤（见 `src/clients.rs`）。

请勿将 `.env` 或 `secrets/` 提交到仓库；使用系统/用户环境变量或本机密钥文件路径即可。

## 仓库结构

```
Cargo.toml              单包工程根（非多 crate 工作区）
scripts/
  verify_apis.ps1       可选：公开 API 结构探针（Windows）
src/
  main.rs               入口与监控主循环
  clients.rs            Polymarket / Kalshi HTTP 客户端
  market_matcher.rs     匹配与索引构建
  market_filter.rs      市场过滤辅助
  text_vectorizer.rs    文本向量化
  vector_index.rs       向量检索
  arbitrage_detector.rs 订单簿遍历、规模、盈亏
  fee_model.rs          Kalshi/PM taker 费、摩擦辅助函数
  kalshi_fee_schedule.rs Kalshi 系列费率乘数
  system_params.rs      全局调参与环境变量名字符串
  validation.rs         校验辅助
  tracking.rs           周期监控状态
  live_trades.rs        实盘 CSV 日志
  kalshi_trade.rs       Kalshi 生产下单
  polymarket_trade.rs   Polymarket CLOB 实盘下单
  category_mapper.rs    基于 categories.toml 的分类
  bin/
    kalshi_balance.rs
    check_env.rs
    live_account_positions.rs
    smoke_live_orders.rs
config/
  categories.toml
docs/
  MATCHING_VERIFICATION.md
```

匹配与索引的进一步说明见 **[docs/MATCHING_VERIFICATION.md](docs/MATCHING_VERIFICATION.md)**。

## 免责声明

- 仅供**研究与学习**，不构成投资建议。
- 预测市场在规则、结算、流动性与时延上存在差异；展示盈亏为基于快照与假设（手续费、摩擦等）的**模型输出**，**实盘结果可能不同**。
- 使用第三方 API 须遵守各平台服务条款与适用法律法规。

## 许可证

若仓库未包含 `LICENSE` 文件，默认保留所有权利；若计划以开放许可分发，请自行补充许可证文件。
