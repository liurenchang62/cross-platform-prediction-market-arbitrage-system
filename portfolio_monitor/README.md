# Portfolio Monitor

The `portfolio-monitor` crate reads venue state and trader records after execution. It keeps portfolio accounting, settlement review, collateral utilities, and manual recovery commands outside the latency-sensitive trading loop.

## Data relationships

```text
desk_events.jsonl ─┬─→ pair and leg mapping ─→ snapshots / profit / cash flow
                   └─→ execution estimates ──→ settled reconciliation

Kalshi APIs ───────┐
Polymarket APIs ───┼─→ positions, fills, settlements, prices, collateral
Gamma API ─────────┘
```

Desk events link the two venue legs under one arbitrage cycle. Venue APIs remain the source for live positions, fills, and settlements.

## Utility matrix

| Binary | Primary output | External or financial side effects |
|---|---|---|
| `portfolio-snapshot` | Combined positions, cash, collateral, market labels, and report files | Authenticated reads and local output |
| `portfolio-profit` | Open and closed position profit analysis | Authenticated reads |
| `portfolio-cashflow` | Time-window cash-flow reconstruction and report output | Authenticated reads; may invoke configured redemption automation |
| `portfolio-turnover` | Turnover series from desk events | Local reads and report output |
| `arb-reconcile` | Detection-time estimate versus fills or settled outcome | Authenticated reads when settlement mode is selected |
| `portfolio-position-markets` | Current books around held positions | Public and authenticated reads |
| `close-position` | Venue-native position reduction | **Can submit live sell orders** |
| `portfolio-pm-redeem-diagnose` | Polymarket redemption eligibility and diagnostics | Network and chain reads |
| `pm-wrap-collateral` | Collateral wrapping through the configured relayer | **Can change on-chain collateral state** |

Run `--help` on an individual binary before supplying account or position identifiers. The complete command forms are maintained in [`../COMMANDS.md`](../COMMANDS.md).

## Library modules

| Module | Responsibility |
|---|---|
| `mapping.rs` | Reconstructs paired strategy legs from append-only desk events |
| `kalshi.rs` | Kalshi authentication, signed reads, fills, settlements, books, and IOC close orders |
| `pm.rs` | Polymarket authentication, conditional-token balances, trade ledger, and FAK close orders |
| `gamma.rs` | Polymarket market titles, expiry display, and closed state |
| `snapshot_report.rs` | Cross-venue position and collateral snapshot |
| `profit.rs` | Open and closed outcome accounting |
| `cashflow.rs` | Venue activity normalization and chronological cash-flow reports |
| `turnover.rs` | Desk-event turnover aggregation and plotting |
| `arb_reconcile.rs` | Pre-trade estimate, execution record, and settlement comparison |
| `pm_relayer_automation.rs` | Proxy approvals, redemption, and collateral wrapping workflows |

## Output and evidence boundaries

Generated reports are written under `portfolio_monitor/out/`; trader records are expected under `logs/`. Both locations are ignored by Git because they may contain account-specific data.

The tools distinguish detection estimates, returned fills, open mark-to-market values, and settled results. These quantities should not be combined into a single profitability figure without preserving their time and data source.

## Operational cautions

- Local JSONL may be incomplete after abrupt termination and does not replace venue statements.
- Position valuation depends on available market prices and the venue's collateral conventions.
- Settlement timing can differ across nominally equivalent contracts.
- Close, redeem, approval, and wrap utilities are operational commands rather than read-only reports.
- Remove wallet addresses, account identifiers, order identifiers, and balances before sharing outputs.
