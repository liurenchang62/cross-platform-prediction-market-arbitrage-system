# Command Reference

Run commands from the repository root. Add `--release` for the optimized profile used by the trading process.

## Build and checks

```bash
cargo build --workspace --release
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
```

## Trader

```bash
# Check credential presence and venue connectivity
cargo run -p trader --bin check_env --release

# Start the market-matching and execution process
cargo run -p trader --release

# Redeem eligible Polymarket positions without starting the trader
cargo run -p trader --bin pm_redeem --release

# Redeem and refresh approvals when required
cargo run -p trader --bin pm_redeem --release -- --approve-batch
```

The main trader can submit live orders as soon as `LIVE_TRADING_ON` is enabled and startup preflight completes. There is no separate interactive confirmation in the current implementation. Use the checker first and review [Operations](docs/OPERATIONS.md).

## Portfolio monitoring

```bash
# Combined venue snapshot
cargo run -p portfolio-monitor --bin portfolio-snapshot --release

# Cash-flow export
cargo run -p portfolio-monitor --bin portfolio-cashflow --release

# Profit summary
cargo run -p portfolio-monitor --bin portfolio-profit --release

# Turnover charts
cargo run -p portfolio-monitor --bin portfolio-turnover --release

# Detection, execution and settlement reconciliation
cargo run -p portfolio-monitor --bin arb-reconcile --release
cargo run -p portfolio-monitor --bin arb-reconcile --release -- --settled

# Markets associated with recorded positions
cargo run -p portfolio-monitor --bin portfolio-position-markets --release
```

Most reporting commands accept an alternate `desk_events.jsonl` path through `--desk`. Use `--help` on a binary for its complete argument list.

## Position and collateral utilities

These commands can change account state. Confirm identifiers, sides, quantities and credential targets before running them.

```bash
# Close a recorded cross-venue position
cargo run -p portfolio-monitor --bin close-position --release -- --help

# Diagnose redeemable Polymarket positions
cargo run -p portfolio-monitor --bin portfolio-pm-redeem-diagnose --release

# Convert collateral for the configured Polymarket account
cargo run -p portfolio-monitor --bin pm-wrap-collateral --release -- --help
```

## Runtime records

| Path | Contents |
|---|---|
| `logs/desk_events.jsonl` | Execution attempts, fills, cleanup orders and model estimates |
| `logs/naked_positions.jsonl` | Residual exposure that was not fully removed |
| `logs/arb_rounds_store.json` | Persistent successful-round counts by oriented pair |
| `portfolio_monitor/out/` | Generated CSV and chart outputs |

These paths are ignored by Git and may contain account-sensitive information.
