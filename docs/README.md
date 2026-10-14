# Documentation Guide

The documents in this directory answer different review questions rather than repeating the root README.

| Document | Use it to understand |
|---|---|
| [Architecture](ARCHITECTURE.md) | Component ownership, concurrency, persistent state, and design boundaries |
| [Configuration](CONFIGURATION.md) | Source-level controls, credentials, matching taxonomy, and local outputs |
| [Operations](OPERATIONS.md) | Preflight, runtime states, post-trade review, and residual-position response |
| [Command reference](../COMMANDS.md) | Exact build, test, report, and operational command forms |
| [Security](../SECURITY.md) | Secret handling, private artifacts, and disclosure scope |
| [Trader internals](../trader/README.md) | Matching, order-book economics, execution invariants, and module map |
| [Portfolio monitor](../portfolio_monitor/README.md) | Reports, settlement tools, and commands with financial side effects |

## Visuals

- [`system-workflow.svg`](system-workflow.svg) follows one pair from discovery to post-trade review.
- [`component-architecture.svg`](component-architecture.svg) is displayed on the project homepage and shows code and state boundaries rather than chronological steps.

The visuals intentionally use different structures: a three-lane sequence for lifecycle reasoning and a layered component map for ownership and dependencies.

## Suggested reading paths

**Technical reviewer:** root README → Architecture → Trader internals → Portfolio monitor.

**Operator:** Configuration → Operations → Command reference → Security.

**Research reviewer:** root README contract-intelligence and validation sections → Trader matching internals → configuration taxonomy.
