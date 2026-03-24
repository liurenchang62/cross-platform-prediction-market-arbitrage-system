# 跨市场套利监控 (`arbitrage-monitor`)

[English](README.md) | **简体中文**

## 项目简介

`arbitrage-monitor` 是一款 **Rust** 实现的监控程序：在 **Polymarket** 与 **Kalshi** 之间基于文本相似度与类别规则发现可对齐的预测市场，维护高置信配对并周期性评估其**可执行层面的经济结果**。定价与盈亏以**实时订单簿**解析结果为依据；程序**只读分析**，不发起下单。

整体流程包括：市场列表拉取、向量匹配与索引、全量刷新与增量追踪、以及将结果写入 `logs/` 等结构化日志。

## 功能概览

- **市场数据**：通过 Polymarket Gamma API、Kalshi Trade API 获取开放市场，分页与数量上限可配置。
- **智能匹配**：TF-IDF 风格文本向量与余弦相似度；结合 `config/categories.toml` 中的类别关键词做约束与加权。
- **订单簿场景盈亏**：对每个候选配对，在双方**卖档深度**上按**固定本金上限**模拟吃单，得到占用资金、手续费、Gas 假设下的净利润等指标（见 **订单簿盈亏模型**）。
- **状态追踪**：维护追踪列表，按参数进行全量重建与周期更新（`src/system_params.rs`）。
- **日志**：周期输出与监控 CSV 写入 `logs/`；未匹配或低信号样本可记入 `logs/unclassified/`。
- **模拟交易（可选）**：在 `system_params.rs` 将 **`PAPER_TRADING_ENABLED`** 设为 `true`；是否写 CSV 由 **`PAPER_WRITE_TRADE_LOG`** 控制；逻辑在 `paper_trading.rs`，说明见 `docs/PAPER_TRADING.md`。

## 订单簿盈亏模型

用于展示与排序的场景（如周期 Top 10、`net_profit_100`）在代码中定义为：

1. **订单簿快照**  
   对每个已匹配市场对，程序通过 HTTP 获取双方**当前订单簿**，将可买流动性解析为按价格**升序排列的卖档** `(价格, 数量)`。

2. **单腿本金上限**  
   每条腿最多使用 **100 USDT**（`src/system_params.rs` 中的 `PAPER_PER_LEG_CAP_USDT`）。在对应卖档上**逐档累加**直至达到该上限或档位耗尽（`src/arbitrage_detector.rs` 中的 `calculate_slippage_with_fixed_usdt`），得到该腿在给定本金下**可成交的合约份数**。

3. **对冲规模**  
   取两腿可成交份数的**较小值**作为统一成交规模 **n**，以保证两腿可按相同份数同时建仓。

4. **规模 n 下的成本与利润**  
   对规模 **n**，在两侧订单簿上再次按档位精确计算总支出与**成交量加权均价**（`cost_for_exact_contracts`），得到 **`capital_used`**；再扣除平台手续费与预设 Gas，得到 **`net_profit_100`**。是否将该配对视为「存在机会」由 **`net_profit_100`** 是否高于检测器配置的最小阈值决定；该判定基于上述**全深度扫档**结果，而非单一报价层面的简化。

**实现位置**：`src/main.rs` 中的 `validate_arbitrage_pair`；`src/arbitrage_detector.rs` 中的 `calculate_arbitrage_100usdt`、`calculate_slippage_with_fixed_usdt`、`cost_for_exact_contracts`。

## 环境要求

- **Rust** 1.70+（建议使用 stable，`edition = "2021"`）
- 可访问上述公开 API（若端点或合规策略变更，请以平台最新文档为准）

## 快速开始

```bash
cargo run              # 调试
cargo run --release    # 长时间运行建议
```

首次运行前请确保存在 **`config/categories.toml`**（仓库已附带示例）。

## 配置说明

| 路径 | 说明 |
|------|------|
| `config/categories.toml` | 类别名称、权重、关键词，用于分类与匹配 |
| `src/system_params.rs` | HTTP/匹配/时间窗口常量；模拟盘开关（`PAPER_TRADING_ENABLED`、`PAPER_WRITE_TRADE_LOG` 等）、CSV 路径 |

### 环境变量（可选）

- **`POLYMARKET_TAG_SLUG`**：设置后 Polymarket 拉取可按 tag 过滤（见 `src/clients.rs`）。
- **`PAPER_RUN_LABEL`**（可选）：写入模拟交易 CSV 的 `notes`，见 `PAPER_RUN_LABEL_ENV`。

## 仓库结构

```
src/
  main.rs               入口与监控主循环
  clients.rs            Polymarket / Kalshi HTTP 客户端
  market_matcher.rs     匹配与索引构建
  text_vectorizer.rs    文本向量化
  vector_index.rs       向量检索
  arbitrage_detector.rs 订单簿遍历、费用与盈亏辅助逻辑
  system_params.rs       全局调参常量
  validation.rs         校验辅助
  tracking.rs           周期监控状态
  paper_trading.rs      模拟交易与持仓（可选）
config/
  categories.toml
docs/
  MATCHING_VERIFICATION.md
  PAPER_TRADING.md
```

匹配与索引的进一步说明见 **[docs/MATCHING_VERIFICATION.md](docs/MATCHING_VERIFICATION.md)**。

## 免责声明

- 仅供**研究与学习**，不构成投资建议。
- 预测市场在规则、结算、流动性与时延上存在差异；展示盈亏为基于快照与假设（手续费、Gas 等）的**模型输出**，**实盘结果可能不同**。
- 使用第三方 API 须遵守各平台服务条款与适用法律法规。

## 许可证

若仓库未包含 `LICENSE` 文件，默认保留所有权利；若计划以开放许可分发，请自行补充许可证文件。
