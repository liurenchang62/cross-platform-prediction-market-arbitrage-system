# Cross-Platform Arbitrage System

**English** | [简体中文](README.zh-CN.md)

> Live arbitrage between **Kalshi** (NO leg) and **Polymarket** (YES leg) prediction markets.
> Research / educational use. Not investment advice.

---

## Overview

This Rust workspace contains two crates:

| Crate | Role |
|-------|------|
| **`trader`** | Main live trading loop: market matching, order-book economics, execution (Kalshi IOC + Polymarket CLOB FOK), session risk, Feishu alerts |
| **`portfolio-monitor`** | Read-only analytics + one-off tools: portfolio snapshot, P&L, cash flows, turnover charts, redeem/wrap utilities |

**How it works:**
1. Every 90-minute full-fetch cycle, `trader` pulls all open markets from both venues.
2. TF-IDF cosine similarity + `config/categories.toml` keyword rules match market pairs.
3. Optionally, a **DeepSeek LLM** pass validates doubtful matches.
4. For each tracked pair, real-time order-book data feeds an economics model (fees, ROI gate, capital caps).
5. When a live opportunity clears all guards, `trader` submits a Kalshi IOC and a Polymarket FOK atomically.
6. Fills land in `logs/desk_events.jsonl`; Feishu bot pushes summaries.

---

## Repository Layout

```
cross_platform_arbitrage_system_livetrading/
├── Cargo.toml                   # workspace: trader + portfolio_monitor
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
│       ├── main.rs              # cycle loop, live gate, startup Relayer call
│       ├── system_params.rs     # all constants + env file loader
│       ├── market_matcher.rs    # TF-IDF matching engine
│       ├── arbitrage_detector.rs
│       ├── validation.rs        # LLM + rule-based pair validation
│       ├── session_risk.rs      # capital baseline + cash warnings
│       ├── kalshi_trade.rs      # Kalshi IOC execution
│       ├── polymarket_trade.rs  # Polymarket FOK execution
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

## Environment Setup

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

## Quick Start

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

## Trading Parameters

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
| `ARB_TRACK_CONCURRENCY` | 40 | Parallel order-book checks per cycle |

---

## Polymarket V2 Migration (April 28, 2026)

Polymarket migrated CLOB collateral from **USDC.e → pUSD**.

- SDK updated to **`polymarket_client_sdk_v2 = "0.5.1"`**
- pUSD contract: `0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB` (Polygon)
- CollateralOnramp: `0x93070a847efEf7F70739046A929D47a521F5B8ee` (Polygon)
- If your proxy wallet still holds USDC.e, run `pm-wrap-collateral` to convert it automatically.

---

## Feishu Notifications

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
- Recommended proxy region: Japan, Korea, or Indonesia (exchanges are sensitive to network stability)

---

## Disclaimer

Research / educational use only. Not investment advice. Live P&L differs from model snapshots; comply with each platform's terms of service and applicable law.
