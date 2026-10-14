# Cross-Platform Prediction Market Arbitrage System

An event-driven Rust system for identifying and executing hedged trades across economically equivalent binary contracts on Kalshi and Polymarket. The system builds cross-venue contract pairs from live market catalogs, evaluates both complementary hedge orientations against visible order-book depth, submits venue-native orders concurrently, and reconciles fills and residual exposure after execution.

> Research software. Live trading involves execution, settlement, venue, and regulatory risk. Nothing in this repository is investment advice or a guarantee of future returns.

## System at a glance

| Layer | Implementation |
|---|---|
| Contract matching | Category-specific TF-IDF vector spaces, exact cosine-similarity search with chunked matrix multiplication, deterministic semantic and settlement rules, and optional DeepSeek validation |
| Opportunity model | Both Kalshi YES + Polymarket NO and Kalshi NO + Polymarket YES are evaluated against the fixed $1 complementary payoff |
| Execution | Full visible ask ladders, depth-weighted prices, venue-specific fees, slippage and capital limits; Kalshi IOC and Polymarket FAK orders are submitted concurrently |
| Risk and operations | Central live-order permission gate, per-leg and per-pair limits, fill reconciliation, residual-position cleanup, persistent round limits, structured event records, and portfolio-monitoring utilities |

## System workflow

![System workflow](docs/system-workflow.svg)

⬜ Processing or state · 🟨 Decision or control · 🟥 Exception recovery

The workflow separates contract equivalence from trade economics. A pair must first survive text retrieval, deterministic validation, and, when configured, LLM review. The execution path then obtains current order books, evaluates both hedge directions, applies fee and liquidity constraints, and submits orders only after the live-order gate and session controls pass.

## Arbitrage model

For two contracts that settle on the same binary event, the system evaluates the complementary bundles

```text
Kalshi YES + Polymarket NO
Kalshi NO  + Polymarket YES
```

Each completed bundle pays approximately $1 at settlement when the two contracts are economically equivalent and their resolution rules align. For a target quantity `q`, the detector walks both visible ask ladders and estimates

```text
gross settlement value = q × $1
estimated total cost   = Kalshi depth cost + Polymarket depth cost
                         + venue fees + execution buffer
estimated net return   = gross settlement value - estimated total cost
```

An opportunity proceeds only when executable depth supports the same integer-aligned quantity on both venues and the estimated net return clears the configured threshold. This is a pre-trade estimate; realized fills, fees, settlement outcomes, and any residual exposure can differ.

## Contract-matching pipeline

| Stage | Role | Failure behavior |
|---|---|---|
| Catalog normalization | Converts Kalshi and Polymarket listings into a shared market representation with titles, outcomes and resolution metadata | Incomplete or out-of-horizon markets are excluded |
| TF-IDF retrieval | Builds category-specific vector spaces and performs exact cosine-similarity search in 512-row matrix blocks | Candidates below the similarity threshold are discarded |
| Deterministic validation | Checks event semantics, settlement-date alignment and category-specific rules | A failed rule removes the pair before tracking |
| LLM validation | Sends remaining ambiguous pairs to DeepSeek with a structured accept/reject prompt | Timeout, rate-limit or parsing failure rejects the pair |
| Active tracking | Expands each accepted pair into both complementary hedge orientations | Expired, imminent or round-limited pairs are removed |

LLM calls occur during pair formation rather than between signal detection and order submission. The execution path therefore does not depend on an LLM response after a pair enters active tracking.

## Order-book execution and risk controls

| Control area | Implementation |
|---|---|
| Executable pricing | Walks the full visible ask ladder on each venue and derives average fill prices and worst accepted limits for the requested quantity |
| Cost model | Includes Kalshi series fee schedules, Polymarket CLOB fee rates, a conservative fallback fee, and a configurable execution buffer |
| Concurrent submission | Uses a single `tokio::join!` point to submit Kalshi IOC and Polymarket FAK orders without intentionally sequencing one venue behind the other |
| Fill reconciliation | Compares returned fills, sells excess contracts with venue-native immediate-or-cancel behavior, and records unresolved exposure separately |
| Capital limits | Applies session-reference sizing, per-leg and combined-pair caps, a Kalshi deployable-balance ceiling, and a maximum number of successful rounds per oriented pair |
| Session monitoring | Tracks venue cash and collateral relative to startup balances, pauses new executions when thresholds are crossed, and can issue rebalancing alerts |
| Operational records | Writes append-only desk events, unresolved-position records, persistent round counters, and post-trade reconciliation data under the ignored `logs/` directory |

Cross-venue orders are not atomic. Concurrent submission reduces intentional delay between legs but cannot eliminate venue latency, rejection, partial-fill, or residual-position risk. Cleanup orders may also fail or execute at unfavorable prices.

## Validation scope

The project was exercised with live venue catalogs, authenticated order paths, portfolio reconciliation tools, and small-scale live trading. The following results describe the development and validation scope; they are not general performance guarantees.

| Area | Observed scope |
|---|---|
| Catalog scale | Market ingestion and matching were configured for approximately 50,000 listings per venue |
| Similarity search | Exact cosine scoring used chunked matrix operations; development measurements recorded microsecond-level scoring at the comparison stage |
| Pair validation | The manually validated retained-pair set used during development recorded 100% agreement with its reference labels; the result is specific to that evaluated set |
| End-to-end execution | Completed small-capital arbitrage cycles exercised detection, concurrent orders, fill reconciliation and portfolio accounting |
| Live outcomes | Completed cycles in the reported validation period produced net returns of 10%–20% on the capital deployed in those individual cycles; this is not an annualized result or evidence of repeatable future returns |

Raw account records, credentials and private trading logs are intentionally not published. The repository exposes the implementation and record schemas, but the reported live outcomes cannot be independently reconstructed from public data alone.

## Safety boundary

`LIVE_TRADING_ON` selects live execution or surveillance behavior at build time. At startup, its value is copied directly into the central runtime permission gate checked by order and cleanup entry points. The current implementation does not prompt for an additional interactive confirmation.

Credentials can be supplied through environment variables or ignored files under `env/`; environment variables take precedence. No credentials, local logs, private keys, or account exports should be committed.

The repository currently ships with `LIVE_TRADING_ON = true`. Review the configuration and use the connectivity checker before running the trader. See [Configuration](docs/CONFIGURATION.md) and [Operations](docs/OPERATIONS.md).

## Repository structure

```text
cross-platform-prediction-market-arbitrage-system/
├── Cargo.toml                  # Rust workspace and release profile
├── config/
│   └── categories.toml         # Category taxonomy for contract matching
├── docs/
│   ├── ARCHITECTURE.md         # Component boundaries and data flow
│   ├── CONFIGURATION.md        # Controls and credential sources
│   ├── OPERATIONS.md           # Safe operating and recovery workflow
│   ├── component-architecture.svg
│   └── system-workflow.svg
├── trader/
│   └── src/                    # Matching, detection, execution and reconciliation
├── portfolio_monitor/
│   └── src/                    # Portfolio, cash-flow and settlement utilities
├── COMMANDS.md                 # Build, run and reporting commands
├── SECURITY.md                 # Credential and disclosure guidance
└── README.md
```

The source retains detailed implementation comments and runtime diagnostics in their original language where changing them would not improve public documentation. All visitor-facing project documentation is in English.

## Quick start

Install the stable Rust toolchain, then run read-only checks before starting the main process:

```bash
# Compile the workspace
cargo build --workspace --release

# Validate credential presence and venue connectivity
cargo run -p trader --bin check_env --release

# Run the trader; this can place orders when LIVE_TRADING_ON is true
cargo run -p trader --release
```

The main process connects to external services and can submit live orders when its gates are enabled. It is not a simulation command. For environment variables, operational checks, portfolio tools and recovery commands, use:

- [Configuration](docs/CONFIGURATION.md)
- [Operations](docs/OPERATIONS.md)
- [Command reference](COMMANDS.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Security](SECURITY.md)

## Technology

- Rust 2021 and Tokio
- `reqwest` for HTTP integrations
- `ndarray` for vectorized similarity calculations
- Kalshi RSA-PSS request signing
- Polymarket CLOB SDK and Alloy signing components
- JSONL event records and CSV/PNG portfolio reports

## Limitations

- Contract equivalence is model- and rule-dependent; similar wording does not guarantee identical resolution criteria.
- Visible order-book depth can change between detection and execution.
- Kalshi and Polymarket do not provide an atomic cross-venue transaction.
- Immediate-or-cancel behavior limits resting exposure but does not guarantee balanced fills.
- LLM validation depends on a third-party service and rejects uncertain or failed responses by design.
- Venue APIs, fee schedules, collateral conventions and jurisdictional restrictions can change.
- Published validation figures describe a bounded historical test period and do not establish expected profitability.

## License

No open-source license has been declared. The repository is publicly viewable, but reuse rights are not granted unless a license is added by the author.
