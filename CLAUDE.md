# Claude Code AWS Gateway (CCAG)

Self-hosted API gateway that routes Claude Code through your Amazon Bedrock account. Provides a centralized layer for teams to manage API keys, budgets, rate limits, and OIDC SSO, with a built-in admin portal for observability and configuration.

## Build & Test

```bash
make build               # Build gateway + CLI
make test                # Unit tests (no dependencies)
make lint                # Format check + clippy
make test-integration    # Integration tests (needs Docker)
make check               # All of the above (what CI runs)
make test-e2e            # E2E HTTP tests (needs AWS credentials)
make dev                 # Start Postgres + gateway (auto-detects free ports)
make dev-seed            # Start with mock analytics data
make dev-reset           # Wipe Postgres and start fresh
make dev-down            # Stop local dev environment
```

## Project Structure

```
src/
  main.rs              - Gateway entry point, startup, background loops
  cli/                 - CLI operations tool (ccag binary)
    main.rs            - CLI entry point (clap)
    config.rs          - Admin API client, token management
    commands/          - Subcommands: config, keys, users, teams, scim, status, logs
  api/
    mod.rs             - Router, auth endpoints, setup script serving
    handlers.rs        - HTTP handlers (messages, count_tokens, health)
    admin.rs           - Admin API (keys, teams, users, spend, IDPs, endpoints, analytics)
    cli_auth.rs        - Browser-based OIDC login flow
    proxy_login.sh     - apiKeyHelper script (embedded at compile time)
  config/mod.rs        - GatewayConfig, routing prefix auto-detection
  proxy/mod.rs         - GatewayState (shared state)
  auth/
    mod.rs             - In-memory key cache, key validation
    oidc.rs            - Multi-IDP OIDC JWT validation, JWKS caching
    session.rs         - HS256 session token signing/validation
  ratelimit/mod.rs     - Per-key sliding window rate limiter
  db/                  - Postgres pool, migrations, CRUD
    org_analytics.rs   - Cross-org analytics queries (~20 functions)
  spend/mod.rs         - Async spend tracker (buffer + flush)
  budget/              - Budget enforcement and notifications
  endpoint/            - Multi-endpoint pool and routing
    stats.rs           - Per-endpoint rolling-window throttle/error/request counters
  telemetry/mod.rs     - Prometheus metrics, OTLP export
  translate/
    models.rs          - Model ID mapping (Anthropic <-> Bedrock)
    request.rs         - Request translation
    response.rs        - Response normalization
    streaming.rs       - SSE event formatting
  scim/               - SCIM 2.0 provisioning (auth, discovery, users, groups, filter, types)
  websearch/mod.rs     - DuckDuckGo web search interception
static/index.html     - Embedded admin portal SPA (dashboard, analytics, config)
infra/                 - AWS CDK (TypeScript) for ECS Fargate + RDS + CW Dashboard
migrations/            - Postgres schema migrations (auto-run on startup)
docs/
  configuration.md     - Environment variables, runtime settings
  metrics.md           - Metric reference, Prometheus/OTLP config, Grafana examples
```

## Key Design Decisions

- **Presents as Anthropic Direct API**: Set `ANTHROPIC_BASE_URL` (NOT `CLAUDE_CODE_USE_BEDROCK`)
- **Model ID mapping**: Auto-detected from AWS SDK region. See `config/mod.rs`
- **Beta flag allowlist**: Only forward betas Bedrock accepts. See `translate/models.rs`
- **Auth**: Virtual keys (DB cache) + OIDC JWT (multi-IDP) + gateway session tokens + SCIM 2.0 provisioning
- **Web search**: Intercepts `web_search` tool, executes via DuckDuckGo. See `websearch/mod.rs`
- **Cache invalidation**: Polling `cache_version` table every 5s
- **Database**: Postgres required. Migrations auto-run on startup
- **Portal**: Single-file SPA embedded via `include_str!`. Recompile after edits.
- **Analytics**: Cross-org dashboard with 4 tabs (Spend, Activity, Models, Tools). No new tables; queries aggregate `spend_log`. Multi-select filters (team, user, model, endpoint), time range with granularity control (hourly/daily/weekly), Chart.js time-scaled charts, OLS forecast, CSV export

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `PROXY_HOST` | `127.0.0.1` | Listen address |
| `PROXY_PORT` | `8080` | Listen port |
| `ADMIN_USERNAME` | `admin` | Bootstrap admin username |
| `ADMIN_PASSWORD` | `admin` | Bootstrap admin password |
| `DATABASE_URL` | **required** | Postgres connection URL |
| `OIDC_ISSUER` | _(none)_ | Bootstrap OIDC issuer URL |
| `RUST_LOG` | `info` | Log level |
| `LOG_FORMAT` | `text` | Log format: `text` or `json` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | _(none)_ | OTLP gRPC endpoint for metric export |

Full list: see `docs/configuration.md`

## How to Add a Feature

1. **API endpoint**: Handler in `src/api/admin.rs`, route in `src/api/mod.rs`
2. **Database table**: Migration in `migrations/`, CRUD in `src/db/`
3. **Env var**: Read in `src/config/mod.rs`, document in `docs/configuration.md`
4. **Portal**: Edit `static/index.html`, recompile
5. **CLI command**: Module in `src/cli/commands/`, register in `mod.rs` and `main.rs`

## Bedrock Gotchas

- `cache_control` is sanitized before forwarding to Bedrock (only `type` and `ttl` fields are kept; unknown fields like `scope` are stripped)
- `amazon-bedrock-invocationMetrics` must be stripped from SSE events
- Inference profiles are mandatory for newer Claude models (4.5+)
- Beta flags: ALLOWLIST approach (only forward betas Bedrock accepts)
- Bedrock SDK `Display` impl is terse. Use `Debug` format for error messages.

