# Architecture

This document describes code-level ownership, state transitions, and the boundary between research logic and order-capable components. The chronological operating workflow remains in the repository README.

## Component map

![Component architecture](component-architecture.svg)

**Color key:** blue = contract intelligence · green = trading runtime · coral = recovery · violet = persistent state and reporting

## Component boundaries

| Area | Primary modules | Responsibility |
|---|---|---|
| Venue access | `clients.rs`, `kalshi_trade.rs`, `polymarket_trade.rs` | Public catalog and order-book access, authentication, order submission and venue response parsing |
| Text representation | `text_vectorizer.rs`, `vector_index.rs`, `category_vectorizer.rs`, `category_mapper.rs` | TF-IDF features, normalized vector indexes and category-specific retrieval |
| Pair formation | `market_matcher.rs`, `validation.rs`, `llm_validator.rs`, `market_filter.rs` | Candidate generation, deterministic equivalence checks, optional LLM review and expiry filtering |
| Economics | `arbitrage_detector.rs`, `fee_model.rs`, `kalshi_fee_schedule.rs` | Depth walking, integer-aligned sizing, fee estimation, liquidity checks and return thresholds |
| Orchestration | `main.rs`, `tracking.rs`, `session_risk.rs` | Refresh cadence, oriented-pair tracking, concurrency limits, live gate and session capital state |
| Reconciliation | `trade_reconciliation.rs`, `position_cleanup.rs`, `desk_events.rs` | Fill comparison, excess-leg cleanup, unresolved-exposure records and event persistence |
| Portfolio tools | `portfolio_monitor/src/` | Account snapshots, cash flow, P&L, turnover, settlement reconciliation and operator utilities |

Detailed directory-level maps are available in the [trader guide](../trader/README.md) and [portfolio-monitor guide](../portfolio_monitor/README.md).

## Execution sequence

```text
TrackedArbitrage
    │
    ├─ fetch Kalshi and Polymarket books
    ├─ build executable YES/NO ladders
    ├─ calculate one common integer quantity
    ├─ apply fees, buffer, return threshold, and capital caps
    └─ ArbitrageOpportunity
           │
           ├─ require process-wide live-order permission
           ├─ launch Kalshi IOC and Polymarket FAK futures together
           ├─ await both venue responses
           ├─ compare returned fills
           ├─ submit compensating sell for any excess leg
           └─ append execution and residual state
```

The opportunity object is the boundary between market observation and an order-capable action. It carries the shared quantity and venue-specific limit prices derived from the books used in the decision.

## Data and control flow

The main process periodically fetches complete venue catalogs and constructs a shared representation. Category-specific vectorizers retrieve exact cosine-similarity candidates in matrix chunks. Deterministic rules and optional LLM validation narrow the set before each accepted pair is expanded into two oriented tracking records.

Active records are evaluated concurrently within a bounded task window. Order books and fee schedules are read before the detector walks executable depth. The detector returns an opportunity only when both legs support a common integer quantity and the estimated post-cost return exceeds the configured threshold.

The execution function launches both venue requests through `tokio::join!`. Returned fills are compared after both futures complete. Any excess on one venue is submitted for immediate cleanup; remaining exposure and all execution details are recorded for later review.

## State and persistence

| State | Lifetime | Storage |
|---|---|---|
| Market catalogs and vector indexes | Process memory, rebuilt on full refresh | In memory |
| Active oriented pairs | Process memory, pruned by expiry and matching cycles | In memory |
| Session reference capital and cash alerts | Current process | In memory |
| Successful rounds per oriented pair | Across restarts until expiry pruning | `logs/arb_rounds_store.json` |
| Execution narrative and raw fill fields | Append-only | `logs/desk_events.jsonl` |
| Unresolved residual exposure | Append-only | `logs/naked_positions.jsonl` |

## Concurrency model

- Tokio schedules catalog requests, validation, order-book reads and order submission.
- `buffer_unordered` bounds active pair validation instead of spawning an unrestricted task per pair.
- A separate rate limiter shapes Kalshi order-book traffic.
- LLM validation uses bounded concurrency during matching and remains outside the execution hot path.
- Kalshi and Polymarket order futures begin from the same `tokio::join!` call, but the venues remain independent systems with no atomic commit.

Concurrency is bounded at the workload that creates it: matching validation uses a configured buffer, active-pair evaluation uses `ARB_TRACK_CONCURRENCY`, and Kalshi order-book requests use a separate limiter. This prevents catalog size from becoming an unbounded number of simultaneous external requests.

## Design boundaries

- Exact vector similarity does not establish economic equivalence; the later validation stages remain necessary.
- The code evaluates visible liquidity, not hidden orders or future queue changes.
- Post-trade cleanup is compensating action rather than a rollback.
- JSONL files support traceability and portfolio tooling but are not a transactional database.
- Account credentials, private logs and live-validation evidence are intentionally outside the public repository.
