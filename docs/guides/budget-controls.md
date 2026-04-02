---
title: "Claude Code Cost Management on AWS: Per-User Budgets and Alerts"
description: "Amazon Bedrock has no per-user spend visibility or limits. CCAG tracks every request, attributes costs to users and teams, and enforces configurable budgets with webhook, SNS, and EventBridge notifications."
---

# Claude Code Cost Management on AWS

Amazon Bedrock bills at the account level. There is no native way to attribute costs to individual developers, enforce per-user spending limits, or receive alerts when a team approaches its budget. When one developer runs a long extended thinking session or an automated pipeline makes an unexpectedly large number of requests, the cost is invisible until the monthly AWS bill arrives.

CCAG tracks every request by user, maps token usage to estimated cost, and enforces configurable spending limits before requests reach Bedrock.

## The Problem with Direct Bedrock

When Claude Code connects to Bedrock directly:

- All costs appear on your AWS bill as `Amazon Bedrock - On-Demand Tokens`
- There is no per-user breakdown without building a custom tagging and attribution pipeline
- There is no mechanism to limit how much any individual developer spends
- You cannot automatically throttle or block a user who is burning through budget
- You find out about overruns after the fact, not in time to act

CCAG adds a per-user budget layer on top of Bedrock without changing the inference path.

## How Spend Tracking Works

Every request CCAG forwards to Bedrock is logged with:

- User identity (OIDC subject claim or virtual key name)
- Team assignment
- Model ID
- Input token count, output token count, cache read tokens, cache write tokens
- Estimated cost in USD (based on per-model pricing)
- Timestamp, endpoint, and MCP tools used

Token counts come from the Bedrock response (`amazon-bedrock-invocationMetrics`). Cost estimation uses per-model pricing stored in the database and updated as Bedrock pricing changes.

Spend tracking is asynchronous — token counts are buffered and flushed to Postgres in batches so it adds no measurable latency to the request path.

## Budget Hierarchy

Budgets are evaluated at two levels. Both are checked on every request, and the most restrictive result applies.

**User budget** (resolved in order — first non-null wins):
1. Explicit limit on the user
2. Team's default per-member limit
3. Global default budget
4. No limit

**Team budget**: total spend of all team members against the team's aggregate limit.

If a user is under their own limit but their team has hit its limit, requests are blocked (or shaped) based on the team's policy.

## Setting Budgets

### Team Budget with Per-Member Default

```bash
curl -X PUT https://ccag.example.com/admin/teams/{team_id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "budget_amount_usd": 500.00,
    "budget_period": "monthly",
    "budget_policy": "standard",
    "default_user_budget_usd": 50.00
  }'
```

This sets a $500/month team limit and a $50/month per-member default. New team members automatically inherit the $50 limit — no per-user setup required.

### Explicit Per-User Limit

Override the team default for a specific user:

```bash
curl -X PUT https://ccag.example.com/admin/users/{user_id}/spend-limit \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"limit_usd": 150.00}'
```

### Global Default

Set a fallback for users not assigned to any team:

```bash
curl -X PUT https://ccag.example.com/admin/settings/default-budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "default_budget_usd": 25.00,
    "default_budget_period": "monthly",
    "default_budget_policy": "standard"
  }'
```

## Enforcement Modes

| Mode | What Happens |
|---|---|
| `notify` | Alert is sent; request continues normally |
| `shape` | Request is allowed; user's RPM is throttled to a configured rate |
| `block` | Request is rejected with HTTP 402 until period resets |

The `standard` preset sends a notification at 80% and blocks at 100%. The `soft` preset allows overage up to 150% before blocking. You can define custom rules with multiple thresholds:

```bash
curl -X PUT https://ccag.example.com/admin/teams/{team_id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "budget_amount_usd": 200.00,
    "budget_period": "monthly",
    "budget_policy": [
      {"at_percent": 50, "action": "notify"},
      {"at_percent": 80, "action": "notify"},
      {"at_percent": 100, "action": "shape", "shaped_rpm": 5},
      {"at_percent": 150, "action": "block"}
    ]
  }'
```

Rules must be in ascending `at_percent` order. The `shape` action accepts `shaped_rpm` to set the throttle rate. Only one `block` rule is allowed and it must be last.

## Budget Periods

| Period | Resets at |
|---|---|
| `daily` | 00:00 UTC each day |
| `weekly` | Monday 00:00 UTC |
| `monthly` | 1st of month 00:00 UTC |

Daily budgets are useful for CI/CD pipelines where a runaway job should be caught within hours. Monthly budgets match most AWS billing cycles.

## Notifications

When a threshold is crossed, CCAG delivers a notification to your configured destination. Three delivery methods are supported:

**Webhook** — POST to any HTTPS URL:
```bash
# Set via environment variable
BUDGET_WEBHOOK_URL=https://hooks.slack.com/services/T.../B.../xxx
```

**SNS** — publish to an SNS topic:
```bash
BUDGET_NOTIFICATION_SNS_ARN=arn:aws:sns:us-east-1:123456789012:ccag-budget-alerts
```

**EventBridge** — put an event to a custom event bus:
```bash
BUDGET_NOTIFICATION_EVENTBRIDGE_BUS=ccag-events
```

All three can be configured simultaneously. Notification payloads include:

```json
{
  "source": "ccag",
  "category": "budget",
  "event_type": "budget_warning",
  "severity": "warning",
  "user_identity": "user@example.com",
  "team_name": "platform-team",
  "detail": {
    "threshold_percent": 80,
    "spend_usd": 41.20,
    "limit_usd": 50.00,
    "percent": 82.4,
    "period": "monthly",
    "period_start": "2026-04-01T00:00:00Z"
  }
}
```

Event types: `budget_warning`, `budget_shaped`, `budget_blocked`, `team_budget_warning`, `team_budget_shaped`, `team_budget_blocked`, `rate_limit_hit`.

Notifications are deduplicated — the same threshold fires only once per user per period, not on every request after the threshold is crossed.

## Response Headers

Every proxied response includes budget status headers visible to the client:

| Header | Example | Description |
|---|---|---|
| `x-ccag-budget-status` | `warning` | `ok`, `warning`, `shaped`, or `blocked` |
| `x-ccag-budget-percent` | `82.4` | Spend as % of limit |
| `x-ccag-budget-remaining-usd` | `8.80` | Remaining budget in USD |
| `x-ccag-budget-resets` | `2026-04-01T00:00:00Z` | Period start timestamp |

Claude Code does not display these headers, but they are available for tooling built on top of the API.

## Developer Self-Service

Developers can view their own spend in the portal's personal dashboard — no admin access required. The dashboard shows current period spend, daily trend, model breakdown, and remaining budget.

This visibility alone tends to reduce overruns: developers naturally adjust behavior when they can see costs in real time rather than waiting for the monthly bill.

## Analytics for Admins

The admin portal's Spend tab shows:

- Time-series spend charts with hourly, daily, or weekly granularity
- Multi-select filters (team, user, model, endpoint)
- OLS trend forecast projecting end-of-period spend
- CSV export for finance reporting

All data is aggregated from the `spend_log` table — no separate data pipeline or data warehouse is required.

## Token Pricing

Spend is estimated using per-model pricing stored in the database. The defaults follow Bedrock's list prices:

| Model | Input (per 1M) | Output (per 1M) | Cache Read | Cache Write |
|---|---|---|---|---|
| Claude Opus 4.5 / 4.6 | $5.00 | $25.00 | $0.50 | $6.25 |
| Claude Sonnet (default) | $3.00 | $15.00 | $0.30 | $3.75 |
| Claude Haiku 4.5 | $1.00 | $5.00 | $0.10 | $1.25 |

Pricing is updated in the database, not in code. Adjust pricing in the admin portal under **Settings > Pricing** if your negotiated rates differ from list price.

Spend figures are estimates for budget enforcement and attribution purposes, not billing-grade calculations.

## See Also

- [Budget Management](../budgets.md): complete API reference for budget endpoints and policies
- [Configuration](../configuration.md): notification environment variables
- [Enterprise Deployment](enterprise-deployment.md): full team setup including SSO and SCIM
- [Metrics](../metrics.md): Prometheus metrics for spend and budget events
