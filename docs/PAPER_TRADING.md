# Paper trading (simulation)

## Enable

In **`src/system_params.rs`**:

- **`PAPER_TRADING_ENABLED`**: set to `true` to construct the paper engine; any other value is compile-time off (`false`).
- **`PAPER_WRITE_TRADE_LOG`**: set to `true` to create/append **`PAPER_TRADES_CSV`**; if `false`, positions and cash are still simulated but **no CSV rows** are written.

Optional environment variable (unchanged):

```powershell
# Optional: label this test run in CSV `notes`
$env:PAPER_RUN_LABEL="run_A_after_fix"
cargo run
```

## Behaviour

- **Per orientation** (`pm_market_id:kalshi_market_id|PM_side|KS_side`, e.g. `…|YES|NO` vs `…|NO|YES`): at most **one** open position per key; **no add-ons** on the same leg. The inverse leg is a **different** key, so both directions may be open at once when both are profitable.
- **Per leg cap**: `PAPER_PER_LEG_CAP_USDT` in `src/system_params.rs` (default 100), same probe as monitor.
- **Paths / labels**: `PAPER_RUN_LABEL_ENV`, `PAPER_SESSION_COUNTER_FILE`, `PAPER_TRADES_CSV` in `src/system_params.rs`.
- **Entry**: ask ladders for open; **cash** deducts `capital_used + fees_open + gas_open` (full outlay).
- **Cooldown**: after a close, **5 monitor cycles** before **that same orientation** can open again (`PAPER_COOLDOWN_CYCLES`).
- **Each monitor cycle start** (after cooldown tick): for **every** open position, the engine fetches PM + Kalshi order books again, walks **bids** to sell exactly **`n`** contracts per leg (`proceeds_for_exact_contracts_sell`), then:
  - **Close** if net early proceeds (after **close fees** and **close gas** = `GAS_FEE_PER_TX * 2`) beat modeled hold-to-settlement by at least `PAPER_MIN_EDGE_EARLY_USD`.
  - Otherwise appends **`NO_CLOSE`** with `no_close_reason` (`edge_below_threshold`, `insufficient_liquidity_*`, fetch/parse errors, etc.).
- **Session ID**: each **process start** increments `logs/paper_session_counter.txt` and writes `session_id` to CSV.
- **Cash**: starts at `PAPER_INITIAL_CASH`; insufficient cash skips new opens.

## Outputs (single file)

- **`logs/paper_trades.csv`** — one log file only:
  - **`OPEN`**: `simulated_open_time_utc` (RFC3339), market ids, token id, sides, `n`, entry averages, `fees_open`, `gas_open`, `cash_after`, etc.
  - **`NO_CLOSE`**: same cycle column = **check cycle**; `check_time_utc` set; `no_close_reason` explains why no early exit this cycle.
  - **`CLOSE`**: `exit_type=early_bid`, `proceeds_gross`, `fees_close`, **`gas_close`**, `pnl_realized`, `check_time_utc`.
- `logs/paper_session_counter.txt` — monotonic session counter.

## Limits

- Close checks depend on live order-book HTTP; failures are logged as **`NO_CLOSE`** with a fetch/parse reason.
