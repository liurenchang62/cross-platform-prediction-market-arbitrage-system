# Cross-market arbitrage monitor (`arbitrage-monitor`)

**English** | [简体中文](README.zh-CN.md)

## Overview

`arbitrage-monitor` is a **Rust** application that discovers cross-venue relationships between **Polymarket** and **Kalshi** prediction markets, keeps a watchlist of high-confidence pairs, and evaluates executable economics from **live order-book data**. After strict validation it places **live orders** via official production APIs (Kalshi IOC, Polymarket CLOB FOK market buy), logging fills and request summaries to **`logs/live_trades.csv`**.

The pipeline combines text-based market matching (vector similarity and category rules), scheduled full refreshes and incremental tracking cycles, console output, and that CSV.

The repository is a **Cargo workspace**: the main binary is `arbitrage-monitor` (package root), and a separate **`backtest`** crate summarizes **legacy paper-trading** sessions in `logs/paper_trades.csv` format (schema differs from the live CSV).

## Features

- **Market data**: Loads open markets from the Polymarket Gamma API and the Kalshi Trade API, with configurable pagination and upper bounds.
- **Matching**: TF-IDF-style text vectors and cosine similarity; optional category constraints and scoring from `config/categories.toml`.

### Matching volume: raw hits vs. final pairs

Full runs typically log three stages: **raw vector hits** (bidirectional Top-K rows, with duplicates), **deduped candidate pairs** `(Polymarket id, Kalshi id)` keeping the best cosine score, then **final pairs** after `ValidationPipeline` rules and the configurable confidence gate. A **small** final count together with a **large** deduped middle (e.g. thousands of candidates reduced to on the order of 10² pairs) usually means the **second pass is doing most of the filtering**, not that matrix matching failed to generate neighbours. Raising recall is a tuning problem (`SIMILARITY_TOP_K`, `SIMILARITY_THRESHOLD`, category coverage in `config/categories.toml`), not a sign the batched matmul path is dropping pairs after dedup.
- **Execution-style PnL**: For each candidate pair, costs and net profit for a **notional-capped, depth-walked** buy scenario are derived from **parsed ask ladders** on both venues (see **Order-book PnL model**).
- **Tracking**: Maintains a set of tracked pairs across cycles, with periodic full rebuilds and parameter-driven intervals (`src/system_params.rs`).
- **Cycle statistics**: Global and per-big-period (full-match window) rollups of arbitrage hit counts and capital summaries, without changing detector logic (`src/cycle_statistics.rs`).
- **Live trade log**: The main loop persists **`logs/live_trades.csv`** only (no monitor or paper CSV).
- **Paper backtest CLI (`backtest`)**: Still reads **`logs/paper_trades.csv`-style** historical logs (columns differ from the live CSV). See **Paper backtest tool** below.
- **Kalshi balance probe**: `cargo run --bin kalshi_balance` queries **production** `GET /portfolio/balance`; credentials match the main binary (below).

## Order-book PnL model

The scenario used for reporting (e.g. Top-10 and `net_profit_100`) is defined as follows:

1. **Order-book snapshots**  
   For a matched pair, the program requests the **current** Polymarket and Kalshi **order books** over HTTP and parses resting liquidity into **ascending ask ladders** `(price, size)`.

2. **Per-leg notional cap (probe)**  
   **`TOTAL_CAPITAL_USD`** (default 1000 USD) × **`PER_LEG_CAP_FRAC_OF_CAPITAL`** (default 0.005 per leg) gives the per-leg cap (`arb_probe_caps()`). The ladder is traversed until that cap is reached or liquidity is exhausted (`calculate_slippage_with_fixed_usdt`), yielding a **fillable contract count** per venue.

3. **Hedged size**  
   The scenario size **n** is the **minimum** of the two per-leg contract counts so that both legs can be notionally filled at the same size.

4. **Cost and profit at size n**  
   For exactly **n** contracts, per-leg total cost and **volume-weighted average prices** are recomputed by walking each ladder again (`cost_for_exact_contracts`). Combined legs yield **`capital_used`**. Platform fees and a fixed gas assumption are subtracted to obtain **`net_profit_100`**. A row is treated as an actionable opportunity when **`net_profit_100`** exceeds the detector’s configured minimum; this gate is applied to the depth-based result, not to a single-level quote in isolation.

**Implementation reference**: `validate_arbitrage_pair` in `src/main.rs`; `calculate_arbitrage_100usdt`, `calculate_slippage_with_fixed_usdt`, and `cost_for_exact_contracts` in `src/arbitrage_detector.rs`.

## Requirements

- **Rust** 1.70+ (stable recommended, `edition = "2021"`)
- Network access to the above public APIs (verify against current Kalshi and Polymarket documentation if endpoints or policies change)

## Quick start

```bash
cargo run              # debug
cargo run --release    # recommended for long runs
```

Ensure **`config/categories.toml`** exists before the first run (included in the repository).

### Paper backtest tool

```bash
cargo run -p backtest
```

- Defaults to `logs/paper_trades.csv`; override with environment variable **`PAPER_TRADES_CSV`** if needed.
- **Session vs. monitor cycle**: A session follows one paper-engine lifetime (one process run). It is **independent** of the periodic **full-match** refresh (every `FULL_FETCH_INTERVAL` monitor cycles). A single session normally spans **many** full-match and tracking cycles until the process exits.
- **Calendar rule**: Only sessions whose **start** timestamp falls on the chosen UTC day are listed; metrics always use **all** rows for that `session_id` (multi-day runs are not cut at midnight for reporting).
- **Report layout**: Session metrics are grouped (overview, time span, balances, positions, costs, P&L, yields). Footnotes explain calendar rules, cost attribution, and why per-line yields need not sum to the headline yield.

### Kalshi production balance probe

```bash
cargo run --bin kalshi_balance
```

- Queries Kalshi **production** Trade API `GET /portfolio/balance` with RSA-PSS-SHA256 auth.
- **No `.env`**: configure **system or user** environment variables (Windows: Environment Variables; Unix: `export` / shell rc).
- **`KALSHI_API_KEY_ID`** and **`KALSHI_PRIVATE_KEY_PATH`** (PEM path); names in `src/system_params.rs`.

## Configuration

| Path | Purpose |
|------|---------|
| `config/categories.toml` | Category names, weights, and keywords for classification and matching |
| `src/system_params.rs` | HTTP/matching/window, reference capital and per-pair/per-leg fractions, live CSV path, env var name constants |

### Environment variables

- **Live credentials (required)**: `KALSHI_API_KEY_ID`, `KALSHI_PRIVATE_KEY_PATH`; `POLYMARKET_PRIVATE_KEY` (hex, matches official SDK); optional `POLYMARKET_SIGNATURE_TYPE` (`0`/`1`/`2`).
- **Data (optional)**: `POLYMARKET_TAG_SLUG` for Polymarket tag filtering (`src/clients.rs`).
- **Backtest (optional)**: `PAPER_TRADES_CSV` path for paper session analysis.

## Repository layout

```
Cargo.toml              Workspace root (members: `.`, `backtest`)
src/                    Package `arbitrage-monitor`
  main.rs               Entry point and monitor loop
  clients.rs            HTTP clients for Polymarket and Kalshi
  market_matcher.rs     Matching and index construction
  market_filter.rs      Market filtering helpers
  text_vectorizer.rs    Text vectorization
  vector_index.rs       Vector search
  arbitrage_detector.rs Order-book traversal, fees, and PnL helpers
  system_params.rs      Shared tuning constants
  validation.rs         Validation helpers
  tracking.rs           Per-cycle monitor state
  cycle_statistics.rs   Cumulative cycle stats (global / per big period)
  live_trades.rs        Live trade CSV logging
  kalshi_trade.rs       Kalshi production orders
  polymarket_trade.rs   Polymarket CLOB live orders
  bin/
    kalshi_balance.rs   Optional CLI: production portfolio balance
backtest/               Package `backtest` — paper CSV performance CLI
config/
  categories.toml
docs/
  MATCHING_VERIFICATION.md
  PAPER_TRADING.md
```

Further detail on matching and indexing: **[docs/MATCHING_VERIFICATION.md](docs/MATCHING_VERIFICATION.md)**.

## Disclaimer

- Provided for **research and educational use** only; not investment advice.
- Prediction markets differ in rules, settlement, liquidity, and latency; reported PnL is a **model output** from snapshots and assumptions (fees, gas), and **live results may differ**.
- Comply with each platform’s terms of service and applicable law.

## License

If no `LICENSE` file is present, all rights are reserved; add one if you intend to distribute under open terms.
