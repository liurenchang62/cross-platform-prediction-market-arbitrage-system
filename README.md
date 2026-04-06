# `trader` — Cross-venue Kalshi ↔ Polymarket execution stack

**English** | [简体中文](README.zh-CN.md)

## What this workspace is

**`trader`** is the main **Rust** binary: it matches **Polymarket** and **Kalshi** prediction markets (text similarity + `config/categories.toml`), maintains a tracked pair set, evaluates **live order-book** economics, and can place **production orders** (Kalshi IOC / Polymarket CLOB FOK) after strict validation. Fills and summaries go to **`logs/desk_events.jsonl`**.

**`portfolio-monitor`** is a sibling crate: **read-only** portfolio tools (snapshot, cash flows, P&amp;L summary, turnover charts, PM redeem diagnose) and shared **`pm_relayer_automation`** used by `trader` at startup.

Secrets stay in **environment variables** (and local key files); see **`PROJECT_COMMANDS.txt`** for commands and env hints.

---

## Workspace layout

| Path | Role |
|------|------|
| **`Cargo.toml`** | Workspace members: **`trader`**, **`portfolio_monitor`** |
| **`trader/`** | Live monitor loop, matching, execution, session risk, Feishu hooks |
| **`portfolio_monitor/`** | Lib + CLI: `portfolio-snapshot`, `portfolio-cashflow`, `portfolio-profit`, `portfolio-turnover`, `portfolio-pm-redeem-diagnose` |
| **`config/categories.toml`** | Categories / keywords for classification and matching |
| **`logs/`** | `desk_events.jsonl`, `arb_rounds_store.json`, `trades_cache.txt` (paths in `trader/src/system_params.rs` & `trades_cache.rs`) |

Implementation highlights inside **`trader/src/`**: `main.rs` (cycle loop, live gate), `market_matcher.rs`, `arbitrage_detector.rs`, `validation.rs`, `session_risk.rs`, `desk_events.rs`, `kalshi_trade.rs`, `polymarket_trade.rs`, `feishu_push.rs`, `system_params.rs`.

---

## Quick start

```bash
cargo run -p trader --release
```

Also useful:

```bash
cargo run -p trader --bin check_env --release    # env / keys sanity check
cargo run -p portfolio-monitor --bin portfolio-snapshot --release
```

On startup, `trader` runs a **one-time** Polymarket Relayer path (redeem + collateral refresh when `POLYMARKET_SIGNATURE_TYPE=1` and Relayer env is set); it does **not** repeat that every monitor cycle.

---

## Requirements

- **Rust** stable, **edition 2021**
- Network access to Kalshi, Polymarket (Gamma / CLOB / Data APIs as implemented)

---

## Disclaimer

Research / educational use. Not investment advice. Live P&amp;L differs from model snapshots; comply with each platform’s terms and applicable law.

## License

If no `LICENSE` file is present, rights reserved unless you add one.
