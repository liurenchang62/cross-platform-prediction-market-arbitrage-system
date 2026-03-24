# 匹配管线验证说明（精确余弦索引）

## 改了什么

- **索引检索**：每个类别内用堆叠矩阵做一次 `scores = X @ q`（L2 归一化 TF-IDF 下即**精确余弦**），再按 `score >= SIMILARITY_THRESHOLD` 过滤，取前 `SIMILARITY_TOP_K` 条。
- **不再使用** `kdtree`：构建阶段不再逐点插入 KD 树；检索为类内全量精确打分（无 ANN 近似）。

## 与「全不能变差」的关系

- 在**同一阈值、同一 Top-K、同一分桶与向量化**下，候选由「全体点积 ≥ 阈值」定义，不会因为近似近邻而少候选。
- 若将来调整 `SIMILARITY_TOP_K` 或阈值，才需要重新做对比审计。

## 监控输出（每日 CSV）

- 路径：`logs/monitor_YYYY-MM-DD.csv`（按**本地日期**切分）。
- **仅套利行**：每次验证通过追加一行，无 `cycle_report`、无周期 Top10 汇总行。列含 `event_time_utc_rfc3339`、`event_time_local`、`cycle_id`、`cycle_phase` 及与终端详单一致的数值字段；两侧订单簿前 5 档为 `orderbook_pm_top5_json` / `orderbook_kalshi_top5_json`（JSON 数组 `[[价,量],...]`）。
- 解析日窗口：`system_params::RESOLUTION_HORIZON_DAYS = 21`，无解析日期的市场保留；有日期且晚于「当前 UTC + 21 天」的剔除（全量拉取与追踪列表修剪）。

## 建议操作

### 1. 单元测试

```powershell
cd D:\cross_market_arbitrage_project
cargo test
```

应包含 `vector_index::tests::exact_top_matches_brute_force`。

### 2. Release 构建

```powershell
cargo build --release
```

### 3. 试运行监控（与线上一致）

按你平时的方式启动（需 API/配置）：

```powershell
cargo run --release
```

关注日志中的：

- `构建精确余弦索引` / `堆叠相似度矩阵`
- `并行匹配耗时`、`初筛匹配对`、`二筛过滤`

### 4. 可选：前后对比（严格验收）

1. **保存旧版二进制或打 git tag**，在同一快照数据上各跑一轮，比较 `初筛匹配对`（二筛前）是否 **≥ 旧版**（或记录差异原因）。
2. 若有黄金样本（已知 PM↔Kalshi 真对），检查在**二筛前**是否仍出现在候选里。

## 可调参数（`src/system_params.rs`）

| 常量 | 含义 |
|------|------|
| `SIMILARITY_THRESHOLD` | 余弦下限 |
| `SIMILARITY_TOP_K` | 每 query×类别 保留条数；**仅增大**会扩大初筛候选集 |

---

变更后依赖中已移除 `kdtree`，仅需 `ndarray` 做矩阵向量乘。
