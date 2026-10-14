# Configuration

Configuration is divided between source constants, environment variables and ignored local files. Review all three before starting an order-capable process.

## Execution controls

The main control is `LIVE_TRADING_ON` in `trader/src/system_params.rs`. The current repository value is `true`. At startup, the process copies this value directly into the runtime order permission and then performs authenticated preflight checks. It does not request a separate interactive confirmation.

Important source-level parameters include:

| Parameter | Current value | Purpose |
|---|---:|---|
| `SIMILARITY_THRESHOLD` | `0.90` | Minimum cosine similarity before later validation |
| `MATCH_MATMUL_CHUNK_ROWS` | `512` | Row block size for matrix similarity operations |
| `MAX_SETTLEMENT_DATE_SKEW_DAYS` | `3` | Rejects pairs with materially different scheduled resolution times |
| `IMMINENT_RESOLUTION_SKIP_MINS` | `10` | Excludes pairs near resolution |
| `PER_LEG_CAP_FRAC_OF_CAPITAL` | `0.015` | Per-venue sizing cap relative to session reference capital |
| `PAIR_CAP_FRAC_OF_CAPITAL` | `0.03` | Combined pair cap relative to session reference capital |
| `MAX_PAIR_ARB_ROUNDS` | `3` | Persistent maximum successful rounds for an oriented pair |
| `ARB_TRACK_CONCURRENCY` | `40` | Maximum concurrent active-pair validation tasks |
| `FULL_FETCH_INTERVAL_WALL_SECS` | `5,400` | Full catalog and matching refresh interval |

Changing these values changes live behavior. Treat modifications as strategy or risk changes rather than documentation edits.

## Credential sources

The process can load values from the operating-system environment or the following ignored files:

| File | Intended values |
|---|---|
| `env/env_params.env` | `LOAD_KEY_THROUGH_ENV` file-loading switch |
| `env/proxy.env` | HTTP and HTTPS proxy variables |
| `env/llm.env` | DeepSeek key and optional alert webhook |
| `env/kalshi.env` | Kalshi key identifier and RSA private-key path |
| `env/polymarket.env` | Wallet key source, signature type and relayer settings |

Existing process variables are not overwritten by file values. Set `LOAD_KEY_THROUGH_ENV=false` to use only the process environment.

## Core environment variables

| Variable | Required for | Notes |
|---|---|---|
| `KALSHI_API_KEY_ID` | Kalshi authenticated access | Key identifier only; the private key is separate |
| `KALSHI_PRIVATE_KEY_PATH` | Kalshi request signing | Path to an RSA PEM file |
| `POLYMARKET_PRIVATE_KEY` or `POLYMARKET_PRIVATE_KEY_PATH` | Polymarket signing | Use one controlled secret source |
| `POLYMARKET_SIGNATURE_TYPE` | Polymarket account mode | Must match the configured wallet or proxy account |
| `LLM_PROVIDER` | Optional LLM pair validation | Use `deepseek` for the implemented provider |
| `DEEPSEEK_API_KEY` or `DEEPSEEK_API_KEY_PATH` | DeepSeek validation | Missing configuration disables the LLM provider path |
| `FEISHU_WEBHOOK` | Optional operational alerts | Treat the webhook as a secret |

Additional relayer and proxy variables are checked by the relevant utilities. Run `cargo run -p trader --bin check_env --release` after any environment change.

## Matching taxonomy

`config/categories.toml` assigns keywords to broad market categories. It narrows the vector search space and supports category-aware rules; it does not by itself establish that two contracts have identical settlement conditions.

## Local output

Runtime records and reports are written under `logs/` and `portfolio_monitor/out/`. Both locations are ignored by Git. Preserve them when investigating execution behavior, and remove account identifiers before sharing excerpts.
