# Cross-Platform Arbitrage System

**English** | [简体中文](README.zh-CN.md)

> Live **boxed binary hedges** between **Kalshi** and **Polymarket**: each trade buys complementary YES/NO outcomes across venues so combined payoff targets `$1` per paired contract at resolution—**the engine evaluates both symmetric orientations** (e.g. Kalshi YES + Polymarket NO *and* Kalshi NO + Polymarket YES) and executes whichever passes fees, ROI gates, and risk checks—**not** a fixed “Kalshi always NO / Polymarket always YES” mapping.
>
> Research / educational use. Not investment advice.

---

## Performance & speed

| Angle | What the codebase does |
|-------|-------------------------|
| **Runtime model** | **Rust** + **Tokio** async I/O; hot paths avoid Python-style interpreter overhead and keep one OS process fanning out many concurrent HTTP calls. |
| **Release build** | Workspace `Cargo.toml` enables **`opt-level = 3`**, **`lto = true`**, **`codegen-units = 1`** for maximum throughput in production binaries. |
| **Parallel venue legs** | After validation, **Kalshi IOC buy** and **Polymarket BUY limit FAK** are fired together via **`tokio::join!`**—wall-clock latency is dominated by the slower venue response, not the sum of both sequential RTTs. |
| **Scan concurrency** | **`ARB_TRACK_CONCURRENCY`** (default **40**) caps bounded parallelism for **price tracking** and **full-cycle arbitrage validation**, keeping order-book polling responsive without unbounded connection storms. |
| **Matching throughput** | TF-IDF similarity uses chunked **`MATCH_MATMUL_CHUNK_ROWS`** (**512**) matrix work against large market universes without naive nested loops on every pair. |

---

## Design highlights & innovations

1. **Symmetric complementary legs** — Detection compares **two** boxed structures: `Kalshi YES ask + Polymarket NO ask` vs `Kalshi NO ask + Polymarket YES ask`; whichever clears profit thresholds (after fee models and buffers) drives the live quote path—aligned with how binary contracts actually combine to `$1`.
2. **Dual-orientation expansion** — Matcher output is expanded with **simultaneous YES/NO flips on both venues** so tracking and validation cover both economically equivalent hedge directions without manual relabeling.
3. **Ladder-accurate economics** — Opportunities are refined from **full visible ask ladders** (Polymarket CLOB + Kalshi order books): sweep sizes through levels, derive **IOC limit prices** (worst fill) and **FAK limits**, and grade **integer Kalshi contract counts** consistently between detection and execution.
4. **Venue-native fees** — **Kalshi** taker fees respect **series-specific schedules** from the Trade API; **Polymarket** uses live **`fee-rate`** when available with a conservative **bps fallback** if HTTP fails—avoiding both optimistic zeros and throwing away valid signals.
5. **Safety gates** — **`CONFIRM`** unlock, **`LIVE_TRADING_ON`**, and **`require_live_orders_allowed`** fence every path that can move balances; idle loops stay **read-only** unless a validated opportunity enters execution.
6. **Post-trade reconcile** — **`tokio::join!`** can still yield skewed fills; the runner **IOC-sells excess Kalshi** or **FAK-sells excess Polymarket** above de minimis notional, logs **`naked_positions`** when unwind fails, and records structured outcomes in **`desk_events.jsonl`**.

---

## Overview

Rust workspace crates:

| Crate | Role |
|-------|------|
| **`trader`** | Live loop: venue fetch, TF-IDF matching + optional LLM validation, ladder-based detector, **parallel IOC + FAK** execution, session risk, Feishu alerts |
| **`portfolio-monitor`** | Read-only analytics + utilities: snapshots, P&amp;L, cash flows, turnover charts, redeem / wrap helpers |

**End-to-end flow**

1. On a wall-clock cadence (**`FULL_FETCH_INTERVAL_WALL_SECS`** = **90 min**), refresh open markets from both venues (paginated caps in `system_params.rs`).
2. **TF-IDF cosine** similarity + **`config/categories.toml`** keyword rules propose pairs; optional **DeepSeek** LLM pass for doubtful matches.
3. Expand **dual orientations** per matched PM/Kalshi tuple so both complementary hedge directions are tracked.
4. For each active pair, fetch books concurrently (bounded by **`ARB_TRACK_CONCURRENCY`**); build ladders; run strict detector with Kalshi + PM fee models, ROI gate (**`LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP`**), caps, settlement skew / imminent-resolution skips.
5. When live trading is enabled and checks pass, **`tokio::join!`** submits **Kalshi IOC** and **Polymarket FAK BUY** at ladder-derived limits; reconcile mismatched fills; append **`logs/desk_events.jsonl`** and optional Feishu notifications.

---

## Repository layout

```
cross_platform_arbitrage_system_livetrading/
├── Cargo.toml                   # workspace: trader + portfolio_monitor (release LTO profile)
├── config/
│   └── categories.toml          # keyword categories for matching
├── env/                         # credentials & configuration (git-ignored)
│   ├── env_params.env           # loader switch (LOAD_KEY_THROUGH_ENV)
│   ├── proxy.env                # HTTP/HTTPS proxy (Clash / V2Ray / ClashVerge)
│   ├── llm.env                  # DeepSeek API key + model settings
│   ├── kalshi.env               # Kalshi API key ID + RSA private key path
│   └── polymarket.env           # Polymarket private key + Relayer credentials
├── logs/                        # runtime outputs (auto-created)
│   ├── desk_events.jsonl        # fill ledger (append-only)
│   ├── arb_rounds_store.json    # per-pair round counter (cross-session)
│   ├── naked_positions.jsonl    # emergency: unhedged positions
│   └── trades_cache.txt         # Kalshi trade cache for reconciliation
├── trader/
│   └── src/
│       ├── main.rs              # cycle loop, concurrency, parallel execution + reconcile
│       ├── system_params.rs     # constants + env file loader
│       ├── market_matcher.rs    # TF-IDF matching engine
│       ├── arbitrage_detector.rs # symmetric strategies + ladders + slippage
│       ├── validation.rs        # LLM + rule-based pair validation
│       ├── session_risk.rs      # capital baseline + cash warnings
│       ├── kalshi_trade.rs      # Kalshi IOC / IOC sell-unwind
│       ├── polymarket_trade.rs  # Polymarket FAK BUY / FAK SELL helpers
│       ├── feishu_push.rs       # Feishu webhook alerts
│       ├── desk_events.rs       # fill ledger writer
│       └── bin/
│           ├── check_env.rs     # env / credential sanity check
│           └── pm_redeem.rs     # standalone PM redeem + collateral refresh
└── portfolio_monitor/
    └── src/
        ├── pm_relayer_automation.rs  # Relayer: redeem, approve, wrap, FOK
        └── bin/
            ├── snapshot.rs           # unified KS + PM portfolio snapshot
            ├── cashflow.rs           # cash-flow CSV export
            ├── profit.rs             # P&L summary
            ├── turnover.rs           # trade turnover charts (PNG)
            ├── arb_reconcile.rs      # 3-point return reconciliation
            ├── position_markets.rs   # live order-book view for open positions
            ├── close_position.rs     # directed close (dry-run by default)
            ├── pm_redeem_diagnose.rs # on-chain payout diagnostics
            └── pm_wrap_collateral.rs # USDC.e → pUSD via CollateralOnramp
```

---

## Environment setup

### 1 — Create the `env/` files

The program auto-loads credentials from five files under `env/`. Copy the templates below and fill in your values.

**`env/env_params.env`**
```ini
# true (default): load credentials from env/ files
# false: skip files, rely entirely on system environment variables
LOAD_KEY_THROUGH_ENV=true
```

**`env/proxy.env`**
```ini
# Clash / V2Ray / ClashVerge default port 7897
# Comment out both lines if running on a server without a proxy
HTTP_PROXY=http://127.0.0.1:7897
HTTPS_PROXY=http://127.0.0.1:7897
```

**`env/llm.env`**
```ini
LLM_PROVIDER=deepseek
DEEPSEEK_API_KEY=<your-deepseek-api-key>
LLM_MODEL=deepseek-chat
LLM_TIMEOUT_MS=10000
LLM_MAX_TOKENS=400
LLM_TEMPERATURE=0.0
```

**`env/kalshi.env`**
```ini
KALSHI_API_KEY_ID=<uuid-from-kalshi-settings>
KALSHI_PRIVATE_KEY_PATH=C:\path\to\kalshi_rsa_private.txt
# Optional: print order-book debug info each cycle
# KALSHI_ORDERBOOK_DEBUG=1
```

**`env/polymarket.env`**
```ini
POLYMARKET_SIGNATURE_TYPE=1
POLYMARKET_PRIVATE_KEY_PATH=C:\path\to\pm_private_key.txt
POLYMARKET_API_KEY=<clob-api-key>
POLYMARKET_ADDRESS=<proxy-wallet-address>
RELAYER_API_KEY=<same-as-polymarket-api-key>
RELAYER_API_KEY_ADDRESS=<same-as-polymarket-address>
# POLYGON_RPC_URL=https://polygon-rpc.com   # optional, defaults to public RPC
# POLYMARKET_RELAYER_URL=https://relayer-v2.polymarket.com  # optional
```

### 2 — Key file formats

| Platform | Format | Notes |
|----------|--------|-------|
| Kalshi | RSA PEM private key, `.txt` or `.pem` | Generated from Kalshi Settings → API Keys |
| Polymarket | EVM private key hex, one line, with or without `0x` | The proxy wallet private key |

### 3 — Priority rules

- System environment variables **always win** over `env/` file values (files never overwrite already-set vars).
- Set `LOAD_KEY_THROUGH_ENV=false` to skip file loading entirely and rely on system env vars (CI / server mode).

---

## Quick start

```bash
# 1. Verify credentials and connectivity
cargo run -p trader --bin check_env --release

# 2. Wrap any USDC.e to pUSD before first run (Polymarket V2 requirement)
cargo run -p portfolio-monitor --bin pm-wrap-collateral --release

# 3. Run the live trading monitor
cargo run -p trader --release
```

On startup, `trader` automatically runs one Relayer redeem + collateral refresh pass (requires `POLYMARKET_SIGNATURE_TYPE=1`). The main monitor loop starts immediately after.

---

## Trading parameters

Key constants in `trader/src/system_params.rs`:

| Parameter | Value | Meaning |
|-----------|-------|---------|
| `PER_LEG_CAP_FRAC_OF_CAPITAL` | 1.5 % | Max notional per leg (PM or KS) |
| `PAIR_CAP_FRAC_OF_CAPITAL` | 3 % | Max notional per pair (both legs) |
| `LIVE_ARBIT_MIN_NET_PROFIT_FRAC_OF_PAIR_CAP` | 10 % | Net ROI gate (net profit / capital used) |
| `MAX_PAIR_ARB_ROUNDS` | 3 | Max successful rounds per pair (cross-session) |
| `KALSHI_BALANCE_DEPLOYABLE_FRAC` | 70 % | Max fraction of Kalshi balance deployable |
| `MAX_SETTLEMENT_DATE_SKEW_DAYS` | 3 days | Max allowed resolution date delta between legs |
| `IMMINENT_RESOLUTION_SKIP_MINS` | 10 min | Skip pairs resolving within this window |
| `FULL_FETCH_INTERVAL_WALL_SECS` | 90 min | Full re-match interval |
| `ARB_TRACK_CONCURRENCY` | 40 | Parallel order-book checks / validation workers |

---

## Polymarket V2 migration (April 28, 2026)

Polymarket migrated CLOB collateral from **USDC.e → pUSD**.

- SDK: **`polymarket_client_sdk_v2 = "0.5.1"`**
- pUSD contract: `0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB` (Polygon)
- CollateralOnramp: `0x93070a847efEf7F70739046A929D47a521F5B8ee` (Polygon)
- If your proxy wallet still holds USDC.e, run `pm-wrap-collateral` to convert it automatically.

---

## Feishu notifications

`trader/src/feishu_push.rs` posts alerts for fills, session cash warnings, and Relayer events.

Optional overrides in `env/llm.env`:
```ini
FEISHU_WEBHOOK=https://open.feishu.cn/open-apis/bot/v2/hook/<your-id>
FEISHU_BOT_AUTHORIZATION=<your-token>
```
If not set, the hardcoded default webhook is used.

---

## Requirements

- **Rust** stable (edition 2021)
- Network access to Kalshi REST API and Polymarket Gamma / CLOB / Data APIs
- Recommended proxy region: Japan, Korea, or Indonesia (venues are sensitive to network stability)

---

## Disclaimer

Research / educational use only. Not investment advice. Live P&L differs from model snapshots; comply with each platform's terms of service and applicable law.
