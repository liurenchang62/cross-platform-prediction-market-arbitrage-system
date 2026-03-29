# Cross-market arbitrage monitor (`arbitrage-monitor`)

**English** | [简体中文](README.zh-CN.md)

## Overview

`arbitrage-monitor` is a **Rust** application that discovers cross-venue relationships between **Polymarket** and **Kalshi** prediction markets, keeps a watchlist of high-confidence pairs, and evaluates executable economics from **live order-book data**. After strict validation it can place **live orders** via official production APIs (**Kalshi IOC** buy, **Polymarket CLOB FOK**), logging fills and request summaries to **`logs/live_trades.csv`**.

The pipeline combines text-based market matching (vector similarity and category rules), scheduled full refreshes and incremental tracking cycles, console output, and that CSV.

Secrets are **not** stored in the repo: Kalshi and Polymarket credentials are read from **environment variables** (and PEM / optional key files on your machine). See **Environment variables** and `.gitignore` (`.env`, `secrets/`).

## Features

- **Market data**: Loads open markets from the Polymarket Gamma API and the Kalshi Trade API, with configurable pagination and upper bounds.
- **Matching**: TF-IDF-style text vectors and cosine similarity; optional category constraints and scoring from `config/categories.toml`.

### Matching volume: raw hits vs. final pairs

Full runs typically log three stages: **raw vector hits** (bidirectional Top-K rows, with duplicates), **deduped candidate pairs** `(Polymarket id, Kalshi id)` keeping the best cosine score, then **final pairs** after `ValidationPipeline` rules and the configurable confidence gate. A **small** final count together with a **large** deduped middle (e.g. thousands of candidates reduced to on the order of 10² pairs) usually means the **second pass is doing most of the filtering**, not that matrix matching failed to generate neighbours. Raising recall is a tuning problem (`SIMILARITY_TOP_K`, `SIMILARITY_THRESHOLD`, category coverage in `config/categories.toml`), not a sign the batched matmul path is dropping pairs after dedup.

- **Execution-style PnL**: For each candidate pair, costs and net profit for a **notional-capped, depth-walked** buy scenario are derived from **parsed ask ladders** on both venues, plus **venue-accurate taker fees** and **auxiliary friction** (see **Order-book PnL & cost model**).
- **Tracking**: Maintains a set of tracked pairs across cycles, with periodic full rebuilds and parameter-driven intervals (`src/system_params.rs`).
- **Live trade log**: The main loop persists **`logs/live_trades.csv`** when live trading is enabled.
- **Live execution**: First leg Kalshi IOC, second leg Polymarket FOK; second leg re-fetches books and re-checks net profit (business attempts capped in `main.rs`). Optional stop-loss path compares relaxed PM hedge vs. Kalshi IOC sell.
- **Auxiliary CLIs**: `kalshi_balance` (production balance), `check_env` (mask/verify credentials setup), `live_account_positions`, `smoke_live_orders` (see **Optional binaries**).

## Order-book PnL & cost model

The scenario used for reporting (e.g. Top-10 and `net_profit_100`) is defined as follows:

1. **Order-book snapshots**  
   For a matched pair, the program requests the **current** Polymarket and Kalshi **order books** over HTTP and parses resting liquidity into **ascending ask ladders** `(price, size)`.

2. **Per-leg notional cap (probe) and pair cap**  
   **`TOTAL_CAPITAL_USD`** × **`PER_LEG_CAP_FRAC_OF_CAPITAL`** gives each leg’s probe cap; **`TOTAL_CAPITAL_USD`** × **`PAIR_CAP_FRAC_OF_CAPITAL`** gives **`pair_cap`** (max combined notional for the pair). See `arb_probe_caps()` in `src/system_params.rs`. If combined cost would exceed `pair_cap`, size **n** is scaled down.

3. **Hedged size**  
   The scenario size **n** is the **minimum** of the two per-leg fillable contract counts (after scaling to `pair_cap` if needed).

4. **Costs at size n**  
   **Trading fees** (implementation in `src/fee_model.rs`):
   - **Kalshi taker**: \( \lceil 100 \cdot 0.07 \cdot m \cdot C \cdot P(1-P) \rceil / 100 \) USD, with series multiplier \(m\) from Kalshi fee schedule; \(P\) in dollars.
   - **Polymarket taker**: \( C \cdot p \cdot (\text{bps}/10^4) \cdot (p(1-p))^{\text{exponent}} \) USDC with rounding per helper; `fee_bps` from CLOB `fee-rate`, `exponent` from `categories.toml` mapping.

   **Non-trading friction** (also in `system_params` / `fee_model`):
   - **Assumed on-chain / execution overhead (USD)**: fixed **`LIVE_ARBIT_GAS_ASSUMED_USD`** (small conservative floor; HTTP/CLOB path).
   - **Friction cost (surplus buffer)**: **`pair_cap` × `LIVE_ARBIT_SURPLUS_BUFFER_FRAC_OF_PAIR_CAP`** (default **0.1%** of `pair_cap`).

   **Net profit** (symbolically):  
   `gross (n dollars at resolution) − capital_used − fee_PM − fee_KS − (gas_assumed + friction_buffer)`.  
   An opportunity must satisfy **net > `pair_cap` × `LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP`** (default **0.2%** of `pair_cap`); see `ArbitrageDetector` in `src/arbitrage_detector.rs`.

**Implementation reference**: `validate_arbitrage_core` / `validate_arbitrage_pair` in `src/main.rs`; `calculate_arbitrage_100usdt`, `validate_opportunity_from_ladders`, slippage helpers in `src/arbitrage_detector.rs`.

## Requirements

- **Rust** 1.70+ (stable recommended, `edition = "2021"`)
- Network access to the above public APIs (verify against current Kalshi and Polymarket documentation if endpoints or policies change)

## Quick start

```bash
cargo run              # debug
cargo run --release    # recommended for long runs
```

On startup, choose **track-only** vs **live**; live mode requires the environment variables below and confirmation. Ensure **`config/categories.toml`** exists (included in the repository).

## Optional binaries

| Command | Purpose |
|--------|---------|
| `cargo run --bin kalshi_balance` | Production `GET /portfolio/balance` (Kalshi RSA auth). |
| `cargo run --bin check_env` | Sanity-check env vars and key loading (masked output). |
| `cargo run --bin live_account_positions` | Account/position style diagnostics (see bin docs). |
| `cargo run --bin smoke_live_orders` | Controlled smoke tests for live paths (use with care). |

## API probe script (Windows)

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\verify_apis.ps1
```

Public read-only checks; writes JSON under `tmp_api_probe/` (ignored by git).

## Configuration

| Path | Purpose |
|------|---------|
| `config/categories.toml` | Category names, weights, and keywords for classification and matching |
| `src/system_params.rs` | HTTP/matching/window, reference `TOTAL_CAPITAL_USD`, per-leg/per-pair fractions, gas, friction & min-profit fractions, CSV path, env var name constants |
| `src/fee_model.rs` | Taker fee formulas, `ArbCostParams`, PM exponent table |

### Environment variables

- **Kalshi (live / balance / orders)**: `KALSHI_API_KEY_ID`, `KALSHI_PRIVATE_KEY_PATH` (PEM file path). Names in `src/system_params.rs`.
- **Polymarket CLOB**: `POLYMARKET_PRIVATE_KEY` (hex, SDK-compatible) **or** `POLYMARKET_PRIVATE_KEY_PATH` (text file with hex); optional `POLYMARKET_SIGNATURE_TYPE` (`0` / `1` / `2`).
- **Data (optional)**: `POLYMARKET_TAG_SLUG` for Polymarket tag filtering (`src/clients.rs`).

Do **not** commit `.env` files or `secrets/`; configure system/user environment variables instead.

## Repository layout

```
Cargo.toml              Package manifest (single crate; workspace root is this package)
scripts/
  verify_apis.ps1       Optional public API shape checks (Windows)
src/
  main.rs               Entry point and monitor loop
  clients.rs            HTTP clients for Polymarket and Kalshi
  market_matcher.rs     Matching and index construction
  market_filter.rs      Market filtering helpers
  text_vectorizer.rs    Text vectorization
  vector_index.rs       Vector search
  arbitrage_detector.rs Order-book traversal, sizing, PnL
  fee_model.rs          Kalshi/PM taker fees, friction helpers
  kalshi_fee_schedule.rs Kalshi series fee multipliers
  system_params.rs      Shared tuning constants and env names
  validation.rs         Validation helpers
  tracking.rs           Per-cycle monitor state
  live_trades.rs        Live trade CSV logging
  kalshi_trade.rs       Kalshi production orders
  polymarket_trade.rs   Polymarket CLOB live orders
  category_mapper.rs    categories.toml-backed classification
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

Further detail on matching and indexing: **[docs/MATCHING_VERIFICATION.md](docs/MATCHING_VERIFICATION.md)**.

## Disclaimer

- Provided for **research and educational use** only; not investment advice.
- Prediction markets differ in rules, settlement, liquidity, and latency; reported PnL is a **model output** from snapshots and assumptions (fees, friction), and **live results may differ**.
- Comply with each platform’s terms of service and applicable law.

## License

If no `LICENSE` file is present, all rights are reserved; add one if you intend to distribute under open terms.
