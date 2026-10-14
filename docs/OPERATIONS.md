# Operations

This guide separates read-only verification from commands that can change positions or collateral.

## Preflight sequence

1. Review `LIVE_TRADING_ON` and all sizing constants in `trader/src/system_params.rs`.
2. Confirm the Kalshi key, private-key path, Polymarket wallet and signature type target the intended accounts.
3. Confirm venue access and local regulatory eligibility. Do not treat a proxy as authorization to bypass venue restrictions.
4. Run the environment and connectivity check:

   ```bash
   cargo run -p trader --bin check_env --release
   ```

5. Review current balances, collateral and positions with the portfolio snapshot utility.
6. Start the trader only after confirming that order-capable operation is intended; the current process does not ask for a second interactive confirmation.

## Startup behavior

When live execution is enabled, startup unlocks the guarded order paths from the source-level flag and performs authenticated balance and collateral checks. A preflight failure prevents the main loop from proceeding, but successful preflight does not require another operator prompt.

The system periodically refreshes complete catalogs and otherwise polls active pairs. It records successful execution rounds across restarts and prunes expired entries. Operational alerts can be sent through the optional Feishu integration.

## Execution states

| State | Meaning | Operator response |
|---|---|---|
| `OK` | Both returned fills are aligned | Review the desk event and later settlement reconciliation |
| `RECONCILE_KS` / `RECONCILE_PM` | Excess contracts were submitted for cleanup | Verify the cleanup fill and resulting venue positions |
| `NAKED_KS` / `NAKED_PM` | Residual exposure remains after failed or partial cleanup | Stop new execution if necessary and inspect both accounts immediately |
| `NO_FILL` | Neither venue returned a fill | Review rejection details; no paired position was recorded |

Small residuals below the configured cleanup notional can be recorded without an additional sell request. They still require operator awareness.

## Post-trade review

Use the append-only desk log as the primary link between execution and portfolio tools:

```bash
cargo run -p portfolio-monitor --bin arb-reconcile --release
cargo run -p portfolio-monitor --bin arb-reconcile --release -- --settled
cargo run -p portfolio-monitor --bin portfolio-snapshot --release
```

Compare detection-time estimates, returned fills and settlement results. Model return is not realized return; the reconciliation utility exists to keep those stages distinct.

## Residual-position response

1. Inspect `logs/naked_positions.jsonl` and the corresponding event in `logs/desk_events.jsonl`.
2. Verify live positions directly through both venue APIs or interfaces.
3. Determine whether a cleanup order was rejected, partially filled or skipped below the notional threshold.
4. Use the dedicated close utility only after checking ticker, token, side and quantity:

   ```bash
   cargo run -p portfolio-monitor --bin close-position --release -- --help
   ```

5. Re-run the portfolio snapshot and preserve the records used for the decision.

## Operational limitations

- Network success does not guarantee a fill, and an accepted request does not guarantee symmetric fills.
- The two venues can disagree on settlement timing or wording even when titles are similar.
- Fee endpoints and collateral conventions can change independently of the code.
- Local JSONL files can be incomplete after abrupt termination or storage failure.
- Portfolio tools aid reconciliation but do not replace venue statements or direct position verification.
