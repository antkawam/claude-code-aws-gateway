---
title: "CCAG Configuration Reference"
description: "Complete reference for Claude Code AWS Gateway environment variables, runtime settings, and deployment configuration."
---

# Configuration

CCAG has three layers of configuration, each serving a different purpose:

1. **Deployment config** (`environments.json` for CDK, or `~/.ccag/config.json` for the CLI): deployment targets and infrastructure settings
2. **Environment variables**: bootstrap settings, set at deploy time in the ECS task or locally
3. **Runtime settings** (admin API / portal): dynamic settings that can be changed without redeployment

## Configuration Hierarchy

```
environments.json / CLI config  (deploy-time, infrastructure)
       |
  Environment vars       (startup-time, bootstrap)
       |
  DB settings / portal   (runtime, dynamic, no restart needed)
```

Environment variables bootstrap the gateway on startup. Once running, most settings can be managed through the admin portal or API and are stored in the database. DB settings are synced across instances via a polling mechanism (every 5 seconds).

## Deployment Config

Deployment configuration varies by deployment method:

- **CDK deployments**: use `environments.json` in the project root (see [`infra/README.md`](../infra/README.md) for the schema)
- **CLI (`ccag`)**: uses `~/.ccag/config.json` for storing the gateway URL and auth tokens (created automatically by `ccag config set-url`)

The fields below apply to `~/.ccag/config.json` (CLI config):

### Fields

| Field | Description |
|---|---|
| `account_id` | AWS account ID |
| `region` | AWS region for Bedrock API calls and infrastructure |
| `aws_profile` | AWS CLI profile name (configured in `~/.aws/config`) |
| `domain_name` | Full domain for the gateway (e.g., `ccag.example.com`) |
| `hosted_zone_name` | Route53 hosted zone for DNS record creation |
| `certificate_arn` | ACM certificate ARN (null = auto-created via DNS validation) |
| `admin_users` | Comma-separated OIDC subjects bootstrapped as admin |
| `desired_count` | Number of ECS Fargate tasks |

To update CLI configuration, edit `~/.ccag/config.json` directly or use `ccag config set-url`.

## Environment Variables

Environment variables are set in the ECS task definition (via CDK) or locally when running the gateway. They control startup behavior and cannot be changed at runtime without restarting.

### Core

| Variable | Default | Description |
|---|---|---|
| `PROXY_HOST` | `127.0.0.1` | Listen address. Set to `0.0.0.0` for container/remote access. |
| `PROXY_PORT` | `8080` | HTTP listen port. |
| `RUST_LOG` | `info` | Log level. Set to `debug` to see request/response bodies. |
| `LOG_FORMAT` | `text` | Log output format: `text` (human-readable) or `json` (structured, recommended for production/CloudWatch Logs Insights). |

### Database

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | _(none)_ | Full Postgres connection string (e.g., `postgres://user:pass@host/db`). If unset, the gateway runs without DB features (no virtual keys, spend tracking, or persistent settings). |
| `DATABASE_HOST` | _(none)_ | Postgres hostname. Alternative to `DATABASE_URL` for ECS/CDK deployments where host, port, and credentials are separate. |
| `DATABASE_PORT` | `5432` | Postgres port (used with `DATABASE_HOST`). |
| `DATABASE_NAME` | `proxy` | Postgres database name (used with `DATABASE_HOST`). |
| `DATABASE_USER` | `proxy` | Postgres username (used with `DATABASE_HOST`). |
| `DB_PASSWORD` | _(none)_ | Postgres password (used with `DATABASE_HOST`). |
| `RDS_IAM_AUTH` | `false` | Enable IAM authentication for RDS. When `true`, the gateway generates short-lived IAM auth tokens instead of using a static password. Requires the DB user to be configured for IAM auth and the task role to have `rds-db:connect` permission. |

When `DATABASE_HOST` is set, the gateway constructs the connection string from the individual components. This is the pattern used by the CDK stack, where credentials come from AWS Secrets Manager.

### Authentication

| Variable | Default | Description |
|---|---|---|
| `ADMIN_USERNAME` | `admin` | Bootstrap admin username for password-based login. |
| `ADMIN_PASSWORD` | `admin` | Bootstrap admin password. **Change this in production.** The gateway logs a warning on startup if the default is used. Setting this env var does NOT force admin login to be enabled; use `ADMIN_PASSWORD_ENABLE` for that. |
| `ADMIN_PASSWORD_ENABLE` | _(none)_ | Set to `true` to force admin login on, overriding the portal setting. Use as a **recovery mechanism** when SSO is broken. Remove after recovery. |
| `ADMIN_USERS` | _(none)_ | Comma-separated list of OIDC subject identifiers (typically email addresses) to pre-provision as `admin` users. Created in the database on startup — if a user already exists, they are skipped. When these users later log in via SSO, they inherit the pre-seeded admin role. |
| `OIDC_ISSUER` | _(none)_ | OIDC issuer URL (e.g., `https://dev-12345.okta.com`). **One-time bootstrap:** seeds the IDP to the database on first startup. On subsequent restarts, if the issuer already exists in the DB, this env var is silently ignored. After seeding, manage the IDP via the admin portal or API. See [Identity Providers](#identity-providers-idps) below. |
| `OIDC_NAME` | _(auto)_ | Display name for the env-level IDP. Auto-detected from issuer URL (Okta, Azure AD, Google, or "SSO"). |
| `OIDC_AUDIENCE` | _(none)_ | Expected JWT audience claim. |
| `OIDC_JWKS_URL` | _(auto)_ | JWKS endpoint URL. Auto-discovered from `{issuer}/.well-known/openid-configuration` if not set. |

### TLS

| Variable | Default | Description |
|---|---|---|
| `TLS_PORT` | `443` | HTTPS listener port. Automatically starts when listening on loopback (needed for OIDC redirect flows). |
| `TLS_CERT` | _(none)_ | Path to TLS certificate file. If unset, a self-signed certificate is generated for local development. |
| `TLS_KEY` | _(none)_ | Path to TLS private key file. |

### Telemetry

| Variable | Default | Description |
|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | _(none)_ | OTLP gRPC endpoint for exporting metrics (e.g., `http://otel-collector:4317`). When set, all metric instruments are exported via gRPC every 60 seconds alongside the Prometheus scrape endpoint. See `docs/metrics.md` for the full metric list. |

## Client-Side Environment Variables

These variables are set in the **client's shell** (not the gateway process). The standard CCAG setup flow exports them automatically via `proxy_login.sh` (the `apiKeyHelper` script installed by the Connect page). You do not normally need to set them manually.

| Variable | Value | Description |
|---|---|---|
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` | `1` | Instructs Claude Code to query the gateway's `/v1/models` endpoint for the model picker, instead of using CC's hardcoded model list. Set automatically by `proxy_login.sh` on every invocation (before any early-return path, so it is always exported regardless of whether a cached token is reused or a fresh browser login occurs). Required for `[1m]` suffix variants to appear in `/model`. |

### 1M context window

CCAG supports Claude Code's 1M-context model variants via the `[1m]` suffix convention (e.g., `claude-opus-4-7[1m]`).

> **Multi-endpoint requirement.** `[1m]` variants only appear in `/v1/models` for endpoints configured in the admin portal (or via `ccag` CLI). The gateway's built-in default Bedrock client (used when no endpoint is configured) does not run capability probes, so single-endpoint deployments using only the default client will not see `[1m]` variants in CC's model picker. To enable 1M context discovery, add at least one named endpoint via `Admin → Endpoints` or `ccag endpoints add`.

**How it works end-to-end:**

1. **Advertising.** `/v1/models` emits a paired `<model-id>[1m]` entry (display name appended with `(1M context)`) for any Bedrock inference profile where the `context-1m-2025-08-07` beta has been confirmed supported. With `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` set, CC's model picker shows these variants.

2. **Auto-discovery.** The gateway health loop probes each `(profile, beta)` pair with a synthetic 1-token `InvokeModel` call every 24 hours. A 200 response marks the pair as supported; a `ValidationException` whose message names the beta marks it unsupported. Throttle/5xx responses are ignored and retried on the next tick. No operator configuration is required.

3. **Opportunistic learning.** When a real user request succeeds with a beta that had no cache entry, the gateway records it as supported. When Bedrock rejects a request naming specific betas, those betas are recorded as unsupported, the rejected betas are stripped, and the request is retried once automatically. This means a single `(profile, beta)` pair absorbs at most one round-trip penalty per 24-hour window.

4. **Self-healing.** If Bedrock changes which betas it accepts on a model — a beta promoted to GA starts returning a 400, or a new model starts accepting a beta — the cache corrects itself on the next health-loop tick or the next rejected request. No gateway binary release is required.

5. **Admin override.** Operators can force a `(endpoint, profile, beta) → supported` value via `ccag betas override <endpoint-id> <profile-id> <beta-name> true|false [--reason "..."]` — sets a permanent capability override that ignores TTL and survives restarts. Useful when the auto-discovery is wrong (Bedrock returns a misleading error string, transient outage misclassified as `unsupported`, or a new beta needs to be force-enabled before our parser knows about it).

**Bootstrap window.** After a gateway restart, the capability cache is empty. The first health-loop tick completes within seconds and populates entries. During that window, no `[1m]` variants are advertised by `/v1/models`, and betas are forwarded optimistically on real requests (with rejection-retry protection).

**No operator maintenance.** Self-deployers do not need a new CCAG binary release when Anthropic ships a new beta header that Bedrock accepts, or when AWS launches a new 1M-capable Claude model. The health-loop probe and rejection-learning paths handle both cases automatically.

### Capability probes on AIP-mapped models

When an endpoint has [AIP overrides](endpoints.md#aip-overrides) configured, the health loop's seed probes extend to AIP-mapped model entries by default (`CAPABILITY_PROBE_AIP=true`). Each AIP-mapped `(endpoint, profile, beta)` pair receives the same synthetic probe as a CRI-backed profile: approximately 50 input tokens, ≤5 output tokens, once per `CAPABILITY_TTL` (24 hours). Probes from the health-loop are separate from `GetInferenceProfile` calls (which are control-plane and always run regardless of this setting).

**When to disable (`CAPABILITY_PROBE_AIP=false`):** operators using AIPs for strict cost attribution who do not want synthetic-probe spend appearing against tagged AIP profiles. Set the env var at deploy time or toggle `capability_probe_aip` in `proxy_settings` at runtime (no restart needed).

**Consequences of disabling:** the capability cache for AIP-mapped entries stays empty until rejection-learning populates it from real traffic. As a result:

- `/v1/models` does not emit beta-suffixed variants (e.g., `claude-sonnet-4-5[1m]`) for AIP-only models on this endpoint until the first real request triggers learning.
- The first user request for an unsupported beta absorbs one round-trip penalty (Bedrock rejects, the gateway strips the beta and retries). Subsequent requests for that `(profile, beta)` pair use the cached result.

**Pre-populating the cache manually:** operators who disable probing can force capability flags via `ccag betas override <endpoint-id> <profile-id> <beta-name> true|false [--reason "..."]`. Admin overrides ignore TTL and survive restarts.

### Pricing

| Variable | Default | Description |
|---|---|---|
| `PRICING_REFRESH_INTERVAL` | `86400` | Seconds between automatic refreshes of model pricing from the AWS Price List API (default 86400 = 24 hours). |
| `PRICING_REFRESH_ENABLED` | `true` | Set to `false` or `0` to disable the automatic pricing refresh background loop. Useful when AWS credentials lack Pricing API access or for air-gapped environments. |
| `CAPABILITY_PROBE_AIP` | `true` | Set to `false` to skip seed-probe invocations against AIP-mapped profiles (saves AIP throttle quota); CRI-backed profiles are still probed. Can also be toggled at runtime via the `capability_probe_aip` key in `proxy_settings`. |

### Infrastructure Alarms

CloudWatch Alarms for infrastructure monitoring (ALB 5xx errors, unhealthy targets, RDS CPU/storage) are configured in the CDK stack. Alarms route to an SNS topic (`AlarmTopicArn`, available as a CDK output). Subscribe after deployment:

```bash
aws sns subscribe --topic-arn <AlarmTopicArn> --protocol email --notification-endpoint you@example.com
```

> **Note:** These are infrastructure-level alarms (CDK/CloudWatch). For application-level event notifications (budget alerts, rate limit events), see the [Notifications](#notifications) section below.

## Runtime Settings (Admin API / Portal)

These settings are stored in the database and can be changed at any time through the admin portal or the `PUT /admin/settings/{key}` API. Changes propagate to all gateway instances within 5 seconds (via the cache version polling mechanism).

### Settings

| Setting Key | Default | Description |
|---|---|---|
| `virtual_keys_enabled` | `true` | Enable/disable virtual key authentication. When disabled, only OIDC and admin password auth work. |
| `admin_login_enabled` | `true` | Enable/disable the admin username/password login form. Useful to disable once OIDC is configured. |
| `session_token_ttl_hours` | `24` | How long session tokens (from portal login) remain valid. |
| `websearch_mode` | `enabled` | Web search behavior: `enabled` (per-user provider config), `disabled` (tool stripped, clients configured to skip), or `global` (admin-managed provider for all users). See [Web Search Mode](#web-search-mode). |

### Web Search Mode

Controls how the gateway handles Claude Code's `web_search` tool. Configurable via the portal (Settings > Web Search) or the admin API.

**Modes:**

| Mode | Behavior |
|---|---|
| `enabled` | Default. Each user configures their own search provider (DuckDuckGo, Tavily, Serper, or Custom) via the portal's Web Search page. |
| `disabled` | Web search tool is silently stripped from requests before reaching Bedrock. The setup script pushes `permissions.deny: ["WebSearch"]` to Claude Code clients so they stop requesting it. Users must re-run the setup script to pick up the client-side change. |
| `global` | Admin configures a single search provider used for all users. Per-user provider settings are ignored. The provider config (type, API key, URL, max results) is set alongside the mode. |

**API:**

```bash
# Get current mode
curl https://ccag.example.com/admin/websearch-mode \
  -H "authorization: Bearer $TOKEN"
# {"mode": "enabled"}

# Disable web search
curl -X PUT https://ccag.example.com/admin/websearch-mode \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"mode": "disabled"}'

# Set global mode with Tavily provider
curl -X PUT https://ccag.example.com/admin/websearch-mode \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "mode": "global",
    "provider": {
      "provider_type": "tavily",
      "api_key": "tvly-...",
      "max_results": 5
    }
  }'
```

When mode is `global`, the GET response includes the provider config with the API key masked (`has_api_key: true` instead of the raw key).

### Identity Providers (IDPs)

> **Bootstrap behavior:** `OIDC_ISSUER` is a one-time seed. On first startup, the gateway inserts the IDP into the database. On subsequent restarts, if an IDP with the same issuer URL already exists, the env var is silently ignored — changing `OIDC_AUDIENCE` or other OIDC env vars will have no effect. To re-bootstrap: delete the IDP from Portal > Identity Providers, then restart the gateway. After seeding, the IDP is fully editable via the portal (name, audience, auto-provision settings, etc.).

Additional OIDC providers can be configured through the portal or API (beyond the env-level `OIDC_ISSUER`):

```bash
curl -X POST https://ccag.example.com/admin/idps \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "Okta",
    "issuer_url": "https://dev-12345.okta.com",
    "audience": "ccag",
    "auto_provision": true,
    "default_role": "member"
  }'
```

### Rate Limits

Rate limits are set per virtual key:

```bash
curl -X POST https://ccag.example.com/admin/keys \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"name": "rate-limited-key", "rate_limit_rpm": 60}'
```

### Budgets

Budget limits can be configured per team or globally through the admin portal or API. See the portal's Budget section for details.

### Model Mappings

Custom Anthropic-to-Bedrock model ID mappings are configurable via the admin portal's "Model Mappings" tab and the `ccag mappings` CLI. By default, CCAG auto-detects Bedrock model IDs from the AWS region; admin-added rows act as pinned aliases when an arriving model ID needs literal pass-through.

**Strict matching (1.10.0+).** Inbound model IDs are accepted iff they (a) match a row in `model_mappings` exactly, (b) have a canonical form (`canonicalize_model_id` — date-strip + auto-prepend `claude-` + trim) that matches an existing row or live Bedrock inference profile, or (c) resolve via two-pass discovery (exact stem, then versioned stem). Anything else returns 400 with a message pointing at `GET /v1/models`. The legacy fuzzy-contains discovery pass is gone — admins use the alias mechanism for the long tail.

**AIP override lookups** also canonicalize: a request for `claude-sonnet-4-6-20250514` finds the override keyed `claude-sonnet-4-6` automatically. Admins adding override rows must use the canonical form (or a pinned alias) — non-canonical override keys are rejected with a 400 naming the canonical form when one can be derived, otherwise rejected with a 400 explaining the input could not be canonicalized.

**`ccag mappings` CLI:**

```bash
ccag mappings list                                      # table; --json for JSON
ccag mappings add <anthropic_prefix> <bedrock_suffix> [--display TEXT]
ccag mappings delete <anthropic_prefix> [--yes]
ccag mappings discover <model>                          # preview, does NOT persist
```

The `created_via` column distinguishes how a row got there: `pass1`/`pass2` (auto-discovered), `admin` (admin-added alias), `unknown` (seed/baseline rows or pre-1.10.0 rows that pre-dated the column — surface in the portal so admins can audit before pruning).

**See also:** [### Endpoints](#endpoints) for configuring AIP overrides per endpoint; `GET /v1/models` for the live list of models available to your team (use this to verify a mapping is visible before adding a manual alias).

### Endpoints

Multi-endpoint routing (e.g., routing to different AWS accounts or regions) is configured through the admin portal's Endpoints section.

### Analytics

The admin portal includes a cross-organization analytics dashboard at `/portal#/org-analytics`. Analytics queries aggregate data from the `spend_log` table (no additional tables required).

**API endpoints** (all require admin authentication):

| Endpoint | Description |
|---|---|
| `GET /admin/analytics/org/overview` | KPI summary + filter options |
| `GET /admin/analytics/org/spend` | Spend timeseries, by-team/user/model breakdowns, budget status, OLS forecast |
| `GET /admin/analytics/org/activity` | Active users over time, hourly request heatmap |
| `GET /admin/analytics/org/models` | Model mix, latency percentiles, cache hit rate, token breakdown, endpoint utilization |
| `GET /admin/analytics/org/tools` | MCP server usage, top tools, tool call totals |
| `GET /admin/analytics/org/export` | CSV export of filtered spend data |

**Query parameters** (all endpoints):

| Parameter | Example | Description |
|---|---|---|
| `days` | `7` | Relative time range (1, 7, 14, 30, 90) |
| `from` / `to` | `2026-03-01` / `2026-03-15` | Absolute date range (overrides `days`) |
| `granularity` | `hour` | Time bucket size: `auto`, `hour`, `day`, `week` |
| `team` | `Frontend,ML` | Comma-separated team filter (multi-select) |
| `user` | `alice@co.com` | Comma-separated user filter |
| `model` | `claude-opus-4-6` | Comma-separated model filter |
| `endpoint` | `Default` | Comma-separated endpoint filter |

## Notifications

CCAG can deliver event notifications (budget alerts, rate limit events) to a single destination that you configure through the admin portal.

### Destination Types

| Type | Value | Description |
|---|---|---|
| **Webhook** | `https://hooks.example.com/ccag` | HTTPS endpoint receives JSON POST with event payload |
| **SNS Topic** | `arn:aws:sns:REGION:ACCOUNT:TOPIC` | Your own AWS SNS topic. Add a resource-based policy granting `sns:Publish` |
| **EventBridge** | `arn:aws:events:REGION:ACCOUNT:event-bus/NAME` | Your own custom event bus. Add a resource-based policy granting `events:PutEvents` |

### Draft / Test / Activate Workflow

1. Select a destination type and enter the URL or ARN
2. **Save Draft**: stores the config without affecting active delivery
3. **Test**: sends a synthetic event to the draft destination
4. **Activate**: promotes the draft to active (requires a successful test)
5. **Deactivate**: removes the active config (falls back to env var if set)

### Event Categories

Each category can be independently toggled on the active config:

| Category | Events | Default |
|---|---|---|
| `budget` | `budget_warning`, `budget_shaped`, `budget_blocked`, `team_budget_*` | ON |
| `rate_limit` | `rate_limit_hit` | OFF |

### Event Payload Schema

**Webhook / SNS** — the full payload is sent as the request body (webhook) or message (SNS):

```json
{
  "source": "ccag",
  "version": "1",
  "category": "budget",
  "event_type": "budget_warning",
  "severity": "warning",
  "user_identity": "jane@corp.com",
  "team_id": "uuid",
  "team_name": "frontend",
  "detail": {
    "threshold_percent": 80,
    "spend_usd": 41.20,
    "limit_usd": 50.00,
    "percent": 82.4,
    "period": "weekly",
    "period_start": "2026-03-17T00:00:00Z"
  },
  "timestamp": "2026-03-19T14:30:00Z"
}
```

**EventBridge** — fields that duplicate the EventBridge envelope (`source`, `event_type`, `timestamp`) are omitted from `detail`. Use the envelope fields instead:

```json
{
  "source": "ccag.notifications",
  "detail-type": "budget_warning",
  "time": "2026-03-19T14:30:00Z",
  "detail": {
    "version": "1",
    "category": "budget",
    "severity": "warning",
    "user_identity": "jane@corp.com",
    "team_id": "uuid",
    "team_name": "frontend",
    "detail": {
      "threshold_percent": 80,
      "spend_usd": 41.20,
      "limit_usd": 50.00,
      "percent": 82.4,
      "period": "weekly",
      "period_start": "2026-03-17T00:00:00Z"
    }
  }
}
```

### IAM Resource Policies (BYO SNS / EventBridge)

For SNS topics and EventBridge buses in your account, add a resource-based policy allowing the CCAG task role to publish:

**SNS Topic Policy Statement:**
```json
{
  "Effect": "Allow",
  "Principal": { "AWS": "<TaskRoleArn>" },
  "Action": "sns:Publish",
  "Resource": "<this-topic-arn>"
}
```

**EventBridge Bus Policy Statement:**
```json
{
  "Effect": "Allow",
  "Principal": { "AWS": "<TaskRoleArn>" },
  "Action": "events:PutEvents",
  "Resource": "<this-bus-arn>"
}
```

The `TaskRoleArn` is available as a CDK output after deployment.

### Migration from BUDGET_NOTIFICATION_URL

The `BUDGET_NOTIFICATION_URL` environment variable continues to work as a fallback. When both a DB config and env var exist, the DB config takes precedence. To migrate:

1. Open Portal > Notifications
2. Enter the same URL/ARN currently in `BUDGET_NOTIFICATION_URL`
3. Test and activate
4. Remove the env var from your task definition (optional; it's harmless to keep)

### CDK Outputs

| Output | Purpose |
|---|---|
| `AlarmTopicArn` | SNS topic for **infrastructure** alarms (CloudWatch). Subscribe for ops alerts. |
| `TaskRoleArn` | ECS task role ARN. Use in resource-based policies for BYO SNS/EventBridge. |

### Admin API Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/admin/notifications/config` | Get active + draft config with delivery history |
| `PUT` | `/admin/notifications/config` | Save/update draft destination |
| `DELETE` | `/admin/notifications/config` | Deactivate (remove active config) |
| `DELETE` | `/admin/notifications/draft` | Discard draft |
| `POST` | `/admin/notifications/test` | Test deliver to draft |
| `POST` | `/admin/notifications/activate` | Promote draft to active |
| `PUT` | `/admin/notifications/categories` | Update event categories on active |

## ADMIN_PASSWORD as Break-Glass Recovery

The `ADMIN_PASSWORD` environment variable serves as a break-glass recovery mechanism. Even if all OIDC providers are misconfigured or the database is corrupted, you can always log in with the admin username and password to regain access.

For production deployments:

1. Set `ADMIN_PASSWORD` to a strong, unique value
2. Store it securely (e.g., AWS Secrets Manager, parameter store)
3. Optionally disable `admin_login_enabled` in the portal once OIDC is working, but keep the password documented for emergencies
4. The admin password login bypasses OIDC entirely, so it works even when the identity provider is down

## See Also

- [Getting Started](getting-started.md). Initial setup walkthrough.
- [Authentication](authentication.md). OIDC provider setup guides.
- [FAQ](faq.md). Common configuration questions.
