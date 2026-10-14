# Trader

The `trader` crate owns the order-capable path: venue catalog ingestion, cross-platform contract matching, order-book economics, session controls, two-leg submission, and post-trade reconciliation.

## Runtime sequence

```text
catalog refresh
  → category-aware candidate retrieval
  → deterministic and optional semantic validation
  → two oriented tracking records per accepted pair
  → current order books and fees
  → executable common quantity
  → capital and live-order gates
  → concurrent venue orders
  → fill comparison and residual response
  → persistent desk event
```

Pair formation and execution run at different cadences. The matching path may use an external language model; an already tracked pair does not wait for an LLM request when live books are evaluated.

## Contract matching and validation

| Responsibility | Modules | Implemented behavior |
|---|---|---|
| Shared market model | `market.rs`, `clients.rs` | Normalizes venue identifiers, titles, outcomes, prices, token identifiers, and resolution dates |
| Text representation | `text_vectorizer.rs`, `category_vectorizer.rs` | Tokenization, stemming, stop-word handling, TF-IDF features, and category-specific vector spaces |
| Exact retrieval | `vector_index.rs`, `market_matcher.rs` | Bidirectional cosine candidates, top-k retention, normalized vectors, and chunked matrix multiplication |
| Semantic rules | `validation.rs`, `market_filter.rs` | Event-type, team, player-stat, score, threshold, election-round, date-skew, expiry, and settlement-horizon checks |
| Optional LLM review | `llm_validator.rs` | Structured DeepSeek validation with bounded concurrency, retries, rate-limit handling, and reject-on-failure behavior |
| Tracking identity | `tracking.rs`, `main.rs` | Expands accepted pairs into both complementary orientations and assigns stable direction-aware identifiers |

The category taxonomy in [`../config/categories.toml`](../config/categories.toml) narrows retrieval and selects context-specific rules. It is a search aid, not evidence that two contracts settle identically.

## Opportunity economics

`arbitrage_detector.rs` evaluates current executable ladders for both orientations. It:

- reconstructs venue-specific YES and NO buy ladders;
- rejects malformed, crossed, empty, or insufficient books;
- walks price levels for an exact common contract count;
- records depth-weighted average prices and worst accepted limits;
- applies Kalshi series fees, Polymarket CLOB fees, and a fallback fee when the venue query fails;
- includes a fixed gas estimate and a configurable surplus buffer;
- applies per-leg and combined capital limits before returning an opportunity.

`fee_model.rs` and `kalshi_fee_schedule.rs` keep cost logic outside the matching layer. Detection-time return remains an estimate because the books can change before either venue responds.

## Execution path and invariants

The order path is intentionally narrow:

1. `require_live_orders_allowed()` must pass before an order-capable function proceeds.
2. The opportunity contains one shared integer quantity and a limit derived from each visible ladder.
3. Kalshi IOC and Polymarket FAK requests begin within the same `tokio::join!` call.
4. Both futures complete before returned fills are compared.
5. Excess contracts trigger a venue-native immediate sell when the cleanup threshold is met.
6. Any remaining imbalance is appended to `logs/naked_positions.jsonl` and reflected in the desk event.

The two venue requests are independent. These invariants reduce avoidable asymmetry but cannot provide atomic execution or rollback.

## Risk and operational state

| Control | Source of truth | Lifetime |
|---|---|---|
| Live-order permission | `system_params.rs` | Process-wide atomic gate initialized at startup |
| Per-leg and pair capital caps | `system_params.rs` | Derived from session reference capital |
| Kalshi deployable-balance fraction | `system_params.rs` | Applied during opportunity sizing |
| Successful-round limit | `tracking.rs` | Persisted across restarts until pruning |
| Session cash and collateral checks | `session_risk.rs` | Current process, referenced to startup state |
| Returned fills and raw response fields | `desk_events.rs` | Append-only JSONL |
| Residual exposure | `position_cleanup.rs` | Append-only JSONL plus operator review |

## Module map

```text
src/
├── main.rs                    process lifecycle and tracking loop
├── clients.rs                 public venue catalogs and order books
├── market*.rs                 shared model, filters, and pair construction
├── text_vectorizer.rs         TF-IDF representation
├── vector_index.rs            normalized exact cosine index
├── category_*.rs              taxonomy-specific vector spaces
├── validation.rs              deterministic semantic rules
├── llm_validator.rs           optional DeepSeek review
├── arbitrage_detector.rs      ladder walking and executable sizing
├── fee_model.rs               cost aggregation
├── kalshi_fee_schedule.rs     series fee retrieval and selection
├── kalshi_trade.rs            signed IOC orders
├── polymarket_trade.rs        signed FAK orders
├── session_risk.rs            startup-capital monitoring
├── position_cleanup.rs        excess and residual handling
├── desk_events.rs             structured execution events
├── trade_reconciliation.rs    returned-fill and account review
└── bin/                       environment and redemption utilities
```

## Tests and review points

Unit tests cover text normalization, vector search, semantic validators, order-book parsing, exact ladder sizing, fee calculations, tracking identifiers, response parsing, and selected environment helpers. Authenticated integration behavior still depends on current venue APIs and account state.

Useful review points:

```bash
cargo test -p trader
cargo run -p trader --bin check_env --release
```

The main binary can place live orders with the checked-in gate setting. Use the [operating guide](../docs/OPERATIONS.md) before running it.
