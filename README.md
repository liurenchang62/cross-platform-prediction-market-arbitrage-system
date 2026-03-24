# Cross-market arbitrage monitor (`arbitrage-monitor`)

**English** | [简体中文](README.zh-CN.md)

## Overview

`arbitrage-monitor` is a **Rust** application that discovers cross-venue relationships between **Polymarket** and **Kalshi** prediction markets, keeps a watchlist of high-confidence pairs, and evaluates executable economics from **live order-book data**. It performs **read-only analysis** and does not submit orders.

The pipeline combines text-based market matching (vector similarity and category rules), scheduled full refreshes and incremental tracking cycles, and structured logging under `logs/`.

## Features

- **Market data**: Loads open markets from the Polymarket Gamma API and the Kalshi Trade API, with configurable pagination and upper bounds.
- **Matching**: TF-IDF-style text vectors and cosine similarity; optional category constraints and scoring from `config/categories.toml`.
- **Execution-style PnL**: For each candidate pair, costs and net profit for a **notional-capped, depth-walked** buy scenario are derived from **parsed ask ladders** on both venues (see **Order-book PnL model**).
- **Tracking**: Maintains a set of tracked pairs across cycles, with periodic full rebuilds and parameter-driven intervals (`src/system_params.rs`).
- **Logging**: Cycle outputs and monitor CSVs under `logs/`; unmatched or low-signal items may be recorded under `logs/unclassified/`.
- **Paper trading (optional)**: Set **`PAPER_TRADING_ENABLED`** to `true` in `src/system_params.rs`; CSV output is gated by **`PAPER_WRITE_TRADE_LOG`**. Logic in `src/paper_trading.rs`, details in `docs/PAPER_TRADING.md`.

## Order-book PnL model

The scenario used for reporting (e.g. Top-10 and `net_profit_100`) is defined as follows:

1. **Order-book snapshots**  
   For a matched pair, the program requests the **current** Polymarket and Kalshi **order books** over HTTP and parses resting liquidity into **ascending ask ladders** `(price, size)`.

2. **Per-leg notional cap**  
   Each leg is allocated a maximum spend of **100 USDT** (`PAPER_PER_LEG_CAP_USDT` in `src/system_params.rs`, used for monitoring and paper). The ladder is traversed level by level until that cap is reached or liquidity is exhausted (`calculate_slippage_with_fixed_usdt` in `src/arbitrage_detector.rs`), yielding a **fillable contract count** per venue.

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

## Configuration

| Path | Purpose |
|------|---------|
| `config/categories.toml` | Category names, weights, and keywords for classification and matching |
| `src/system_params.rs` | HTTP/matching/window constants; paper switches (`PAPER_TRADING_ENABLED`, `PAPER_WRITE_TRADE_LOG`, …), CSV paths |

### Optional environment variables

- **`POLYMARKET_TAG_SLUG`**: When set, Polymarket market fetches may be restricted by tag (see `src/clients.rs`).
- **`PAPER_RUN_LABEL`** (optional): appended to paper CSV `notes`; see `PAPER_RUN_LABEL_ENV` in `src/system_params.rs`.

## Repository layout

```
src/
  main.rs               Entry point and monitor loop
  clients.rs            HTTP clients for Polymarket and Kalshi
  market_matcher.rs     Matching and index construction
  text_vectorizer.rs    Text vectorization
  vector_index.rs       Vector search
  arbitrage_detector.rs Order-book traversal, fees, and PnL helpers
  system_params.rs       Shared tuning constants
  validation.rs         Validation helpers
  tracking.rs           Per-cycle monitor state
  paper_trading.rs      Optional paper trading / positions
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
