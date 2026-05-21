# Cross-Platform Arbitrage System

**English** | [简体中文](README.zh-CN.md)

A production-grade monitoring and execution stack for cross-venue binary-contract hedges between **Kalshi** and **Polymarket**. The system continuously ingests full market catalogs from both venues, aligns economically equivalent contracts through similarity search, evaluates boxed hedges against complete order-book ladders, and—when risk gates clear—submits venue-native aggressive orders on both legs in parallel with structured post-trade reconciliation. Each trade targets a combined payoff of approximately **$1** per paired contract at resolution.

> Research and educational use. Not investment advice.

---

## System Architecture

The pipeline operates as a closed loop across two independent central-limit order books:

```
Market ingest → Similarity matching → Dual-orientation expansion
    → Ladder-based detection → Parallel execution → Reconciliation → Ledger
```

**Full-cycle refresh.** Both venue catalogs are pulled via paginated REST and kept current to tens of thousands of contracts. A periodic full fetch (default every 90 minutes, `FULL_FETCH_INTERVAL_WALL_SECS`) forces newly listed markets into the matching index, preventing coverage drift as tickers roll over.

**Pair formation.** TF-IDF cosine similarity over market descriptions combines with curated keyword taxonomies (`config/categories.toml`) to propose candidate pairs. Similarity cuts against `SIMILARITY_THRESHOLD` (0.90); an optional DeepSeek-backed validation path (`validation.rs` / `llm_validator.rs`) handles ambiguous matches.

**Dual-orientation expansion.** Each accepted pair expands into two complementary tracking handles—`Kalshi YES + Polymarket NO` and `Kalshi NO + Polymarket YES`—so that both economically equivalent hedge directions enter surveillance automatically (`expand_dual_orientations` in `main.rs`). This removes a recurring operational risk: manually maintained orientation tables that drift out of sync with the live book.

**Ladder-based detection.** Every cycle rebuilds the visible ask ladder on both venues. The detector walks price levels in sequence, deriving worst-fill IOC limits for Kalshi and budget-aligned FAK notionals for Polymarket (`arbitrage_detector.rs`). Integer Kalshi contract counts are locked at this stage so that detection and execution share the same sizing model—eliminating a classic gap where models think in fractional contracts while venues settle in integers.

**Parallel execution.** Validated opportunities cross a single `tokio::join!` barrier: Kalshi IOC and Polymarket FAK BUY instructions launch simultaneously (`validate_arbitrage_pair_post_success`). Wall-clock latency is bounded by the slower venue's round-trip, rather than the sum of two sequential REST chains. This directly reduces exposure windows during fill competition.

**Reconciliation.** Independent matching engines rarely produce perfectly symmetric fills. When one leg overfills relative to the other, the system unwinds the excess through venue-native opposite-side orders (Kalshi IOC sell, Polymarket FAK sell) above a de minimis notional threshold. Residual imbalances that cannot be unwound are logged to `logs/naked_positions.jsonl` (`NAKED_POSITIONS_JSONL`). Every executed trade—including partial fills and unwind attempts—is appended to `logs/desk_events.jsonl` (`DESK_EVENTS_JSONL`) as structured, timestamped records.

---

## Engineering Implementation

### Runtime Model

| Decision | Implementation |
|----------|----------------|
| Language & concurrency | Single Rust binary + Tokio async runtime. Polling loops, RSA signing, and reconciliation futures share one address space. No managed runtime or interpreter overhead on hot paths. |
| Release profile | Workspace `Cargo.toml`: `opt-level = 3`, `lto = true`, `codegen-units = 1`. Trades incremental compile time for maximum steady-state throughput—the binary's job is to saturate HTTP pipelines, not rebuild frequently. |
| Process model | One OS process fans out many concurrent HTTP calls. No multiprocess coordination overhead. |

### Matching Throughput

TF-IDF similarity search projects venue-wide market descriptions into a joint vector space. Rather than iterating pairwise, the matcher uses chunked matrix multiplication with **512-row blocks** (`MATCH_MATMUL_CHUNK_ROWS`), allowing ndarray-friendly dense kernels to absorb large venue snapshots without scripting-language loop overhead. This keeps full-catalog re-matching within practical wall-clock bounds even as market counts grow into the thousands.

### Three-Stage Pair Validation

Market matching follows a funnel architecture that progressively narrows candidates before they reach the execution pipeline:

```
TF-IDF similarity → Rule-based filtering → LLM validation → Active tracking
```

**Stage 1 — TF-IDF similarity.** Vectorized market descriptions are compared via cosine similarity against `SIMILARITY_THRESHOLD`. This stage is optimized for throughput via chunked matrix multiplication.

**Stage 2 — Rule-based filtering.** Surviving pairs are checked against deterministic rules: keyword category constraints (`config/categories.toml`), settlement date alignment, and minimum volume or open-interest floors. Pairs that fail any rule are dropped without touching the LLM.

**Stage 3 — LLM validation.** All remaining candidates are sent to **DeepSeek Chat** (`llm_validator.rs`) for a final accept/reject decision. The model receives structured prompts comparing the two market descriptions and returns a binary judgment with a short rationale.

| Aspect | Detail |
|--------|--------|
| Model | DeepSeek Chat, zero temperature for deterministic output |
| Timeout | `LLM_TIMEOUT_MS` (default 10s) |
| Concurrency | LLM calls are fanned out with bounded parallelism to stay within API rate limits while processing full-match batches |
| Failure behavior | Timeout, rate-limit, or parse failure → pair is **rejected** (conservative default) |
| Disable | Set `LLM_PROVIDER` to empty or omit `DEEPSEEK_API_KEY` to skip the LLM stage; the funnel degrades to a two-stage pipeline |

The LLM stage runs **off the hot path**—only during pair formation and periodic full re-match cycles, never between signal detection and order execution.

### Ladder-Accurate Economics

| Aspect | Detail |
|--------|--------|
| Price source | Full visible ask ladder on both venues—not top-of-book snapshots |
| IOC limit derivation | Worst resting sell price after walking the cumulative size required to fill the target notional |
| FAK budget | Notional sized against realized VWAP across walked levels, not optimistic best-quote prints |
| Contract integer alignment | Kalshi integer contract counts derived from the same ladder walk used for pricing; detection and execution operate on identical quantities |

### Complementary-Leg Structure

The detector evaluates two boxed bundles side by side:

| Bundle | Composition |
|--------|-------------|
| Orientation A | Kalshi YES asks + Polymarket NO asks |
| Orientation B | Kalshi NO asks + Polymarket YES asks |

Whichever bundle clears post-fee profitability, ROI thresholds, and liquidity checks drives execution. Neither venue is hardcoded as the "YES side" or "NO side"—the economics of the book at that moment determine which orientation fires.

### Venue-Native Fees

| Venue | Fee source | Refresh / fallback behavior |
|-------|-----------|-----------------------------|
| Kalshi | Series-specific taker schedules from Trade API `/series/fee_changes`, filtered per tracked series ticker (`kalshi_fee_schedule.rs`) | Hydrated whenever the orchestrator builds the Kalshi `series_keys` set for a **full-match validation batch** or an **active tracking sweep**—aligned with validation cycles, not necessarily every isolated order-book GET |
| Polymarket | Dynamic CLOB `fee-rate` (bps) via authenticated client | Conservative floor `PM_TAKER_FEE_BPS_WHEN_FETCH_FAILS` (250 bps) on HTTP failure |

Fee models are refreshed alongside those validation passes so promotional windows and fee changes propagate into pricing without manual intervention. The Polymarket fallback is intentionally pessimistic—assuming zero fees silently would inflate apparent edge and risk live losses.

### Concurrency Shaping

| Parameter | Value | Purpose |
|-----------|-------|---------|
| `ARB_TRACK_CONCURRENCY` | 40 | Caps simultaneous book-driven validation tasks |
| Effect | Tokio `buffer_unordered(conc)` around futures streams in `main.rs` | Prevents unbounded in-flight HTTP work during scan cycles while maintaining fresh liquidity across tracked pairs |

Rather than spawning one unbounded task per market pair (which could number in the hundreds), the orchestrator multiplexes work behind this fixed concurrency window—converting potential connection storms into a controlled fan-out that respects local resources and venue rate-limit norms (`KALSHI_OB_RATE_LIMIT_PER_SEC` additionally shapes Kalshi book polling).

### Safety Gates

```
LIVE_TRADING_ON (compile-time) → set_live_arbitrage_orders_allowed at startup
    → require_live_orders_allowed() on execution paths → Orders / unwinds
```

`LIVE_TRADING_ON` is a compile-time flag in `system_params.rs`; `main` mirrors it into `live_arbitrage_orders_allowed` before the loop begins. Every path that can debit collateral issues `require_live_orders_allowed()` (`kalshi_trade.rs`, `polymarket_trade.rs`, reconciliation). Between executions the loop confines itself to read-only venue calls (quotes, ladders, fee endpoints). Setting `LIVE_TRADING_ON=false` keeps the process in surveillance-only mode (opportunities print, no live placement) without rebuilding secondary confirmation machinery.

### Post-Trade Ledger & Round Limits

| Record | Location | Purpose |
|--------|----------------|---------|
| Desk events | `logs/desk_events.jsonl` (`DESK_EVENTS_JSONL`), append-only JSONL | Full trade narrative: fills, partial fills, unwind attempts, timestamps |
| Naked positions | `logs/naked_positions.jsonl` (`NAKED_POSITIONS_JSONL`), append-only JSONL | Residual imbalances that could not be unwound |
| Round counter | `logs/arb_rounds_store.json` (`ARB_ROUNDS_STORE_PATH`), per-pair JSON | Cross-session persistence of successful execution rounds |

The round counter enforces `MAX_PAIR_ARB_ROUNDS = 3` across process restarts—preventing silent pyramiding when the same statistical edge reappears across sessions.

---

## Runtime Cadence

| Rhythm | Interval | Behavior |
|--------|----------|----------|
| Full re-match | 90 min (`FULL_FETCH_INTERVAL_WALL_SECS`) | Pull complete catalogs, rebuild similarity projections, re-expand orientations, enqueue tracking |
| Microstructure polling | Tight loop (~2 s sleep between cycles in `main.rs`, plus network latency) | Rebuild ladders, re-validate active pairs behind `ARB_TRACK_CONCURRENCY` |

**Capital parameters** (fractions reference `session_reference_capital_usd`, seeded at startup from live balances—see `session_risk.rs` / `set_session_reference_capital_usd`):

| Parameter | Value | Constraint |
|-----------|-------|------------|
| Per-leg notional cap | 1.5% (`PER_LEG_CAP_FRAC_OF_CAPITAL`) | Single-venue probe sizing |
| Pair combined cap | 3% (`PAIR_CAP_FRAC_OF_CAPITAL`) | Both legs combined ceiling |
| Kalshi deployable ceiling | 70% (`KALSHI_BALANCE_DEPLOYABLE_FRAC`) | Leaves buffer for withdrawals and concurrent fills |
| Net ROI gate | 10% (`LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP`) | Net profit must exceed this fraction of deployed capital (`capital_used`) before acceptance |

**Calendar guards:**

| Guard | Threshold | Rationale |
|-------|-----------|-----------|
| Settlement date skew | Reject when `abs(ΔUTC seconds) > MAX_SETTLEMENT_DATE_SKEW_DAYS × 86400` (3 days) | Drops duration-mismatched pairs (either side missing dates skips the check) |
| Imminent resolution skip | Skip when either leg resolves within `IMMINENT_RESOLUTION_SKIP_MINS` (10 minutes) | Excludes terminal microstructure noise from ladder logic |

---

## Quick Start

```bash
# Verify credentials and connectivity
cargo run -p trader --bin check_env --release

# Migrate USDC.e to pUSD (Polymarket V2 collateral; run once if needed)
cargo run -p portfolio-monitor --bin pm-wrap-collateral --release

# Launch live surveillance / execution (respects LIVE_TRADING_ON in system_params.rs)
cargo run -p trader --release
```

Startup runs an automated Relayer redeem plus collateral refresh via `portfolio_monitor::pm_relayer_automation::desk_hydrate_pm_proxy` when proxy signing is configured—typically `POLYMARKET_SIGNATURE_TYPE=1`. Secrets live under `env/` (git-ignored); **`std::env` values always win** over file-provided keys (`load_env_file_if_missing`).

---

## Repository Layout

```
cross_platform_arbitrage_system_livetrading/
├── Cargo.toml                   # Workspace: release LTO profile
├── config/
│   └── categories.toml          # Matching keyword taxonomy
├── env/                         # Operator secrets (git-ignored)
├── logs/                        # Runtime: desk_events, naked_positions, round counters
├── trader/
│   └── src/
│       ├── main.rs              # Orchestration loop, buffer_unordered concurrency, join points
│       ├── system_params.rs     # Constants, cadence, env bootstrap, LIVE_TRADING_ON gate
│       ├── clients.rs           # Kalshi / Polymarket REST clients
│       ├── market.rs            # Market model
│       ├── market_filter.rs
│       ├── market_matcher.rs    # TF-IDF + chunked matrix multiplication
│       ├── arbitrage_detector.rs# Ladder economics, IOC/FAK derivation
│       ├── validation.rs        # Deterministic + optional LLM validation
│       ├── llm_validator.rs
│       ├── tracking.rs          # Dual-orientation identifiers
│       ├── tracking_log.rs
│       ├── session_risk.rs      # Liquidity telemetry vs. baseline
│       ├── kalshi_trade.rs      # IOC buy/sell + RSA-signed REST
│       ├── kalshi_fee_schedule.rs
│       ├── polymarket_trade.rs  # Authenticated CLOB wrappers (FAK paths)
│       ├── fee_model.rs         # Shared fee / drag arithmetic
│       ├── desk_events.rs       # Append-only JSONL ledger writer
│       ├── trade_reconciliation.rs
│       ├── position_cleanup.rs  # Residual position hygiene
│       ├── trades_cache.rs
│       ├── feishu_push.rs       # Optional operational alerts
│       ├── text_vectorizer.rs
│       ├── vector_index.rs
│       ├── category_vectorizer.rs
│       ├── category_mapper.rs
│       └── bin/
│           ├── check_env.rs     # Credential sanity check
│           └── pm_redeem.rs     # Standalone redeem utility
└── portfolio_monitor/
    └── src/
        ├── lib.rs
        ├── gamma.rs
        ├── kalshi.rs
        ├── pm.rs
        ├── mapping.rs
        ├── http_client_env.rs   # Shared HTTP client builder (also used by trader)
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

## Requirements

- Rust stable (edition 2021)
- Reliable HTTPS access to Kalshi Trade API and Polymarket Gamma / CLOB
- Recommended proxy region: Japan, Korea, or Indonesia

---

## Disclaimer

Educational and research context only. Not investment advice. Live fills diverge from model estimates. Operators remain responsible for venue terms and jurisdictional compliance.
