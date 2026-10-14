# Security Policy

## Credential handling

The trader uses private credentials for Kalshi, Polymarket and optional external services. Supply them through process environment variables or files under the ignored `env/` directory. Process environment variables take precedence over file values.

Do not commit:

- exchange API keys or RSA private keys;
- wallet private keys, relayer credentials or signing material;
- proxy credentials or notification webhooks;
- account exports, execution records or portfolio reports;
- screenshots containing balances, order identifiers or credential paths.

The `check_env` utility reports credential presence and connectivity without intentionally printing full secret values. Review terminal output before sharing it because venue responses may still contain account-specific metadata.

## Live-order boundary

The repository contains order-capable code. At startup, `LIVE_TRADING_ON` is copied directly into the runtime permission checked by order and cleanup paths. The current implementation does not require a separate interactive confirmation. Review the source value before every run; this control does not replace account-level permissions, venue limits, wallet controls or operator review.

Use separate low-balance accounts for testing where supported. Verify the selected Kalshi environment, Polymarket wallet, signature type, collateral state and network route before enabling execution.

## Private operational data

Files under `logs/`, `env/`, `secrets/` and `portfolio_monitor/out/` are excluded from version control. If any secret is committed, remove it from the repository history and rotate it immediately; deleting only the current file is insufficient.

## Reporting a vulnerability

Report security issues privately to the repository owner rather than opening a public issue containing exploit details, credentials or account information.
