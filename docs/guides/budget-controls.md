---
title: "Per-User Budget Enforcement for Claude Code on AWS"
description: "Control Claude Code costs on Amazon Bedrock with per-user and per-team spending limits, real-time notifications, and automatic enforcement via CCAG."
---

# Per-User Budget Enforcement for Claude Code

Amazon Bedrock bills at the account level. There is no built-in way to attribute costs to individual developers or enforce per-user spending limits. If one developer runs an expensive extended thinking session, the entire team's bill increases with no visibility into who spent what.

CCAG tracks every request's token usage, maps it to the authenticated user, and enforces configurable spending limits.

## How It Works

Every request through CCAG is logged with:
- **Who**: the authenticated user (OIDC subject or API key)
- **What**: model, input/output tokens, thinking tokens
- **Cost**: calculated from Bedrock's per-token pricing for the model used
- **When**: timestamp for time-range queries

This data feeds three features: real-time spend tracking, budget enforcement, and analytics.

## Setting Budgets

Budgets can be set at two levels:

### User-Level Budget

```bash
curl -X PUT https://ccag.example.com/admin/users/{id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "monthly_limit_usd": 25.00,
    "notification_threshold_pct": 80,
    "action_on_limit": "throttle"
  }'
```

### Team-Level Budget

```bash
curl -X PUT https://ccag.example.com/admin/teams/{id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "monthly_limit_usd": 500.00,
    "notification_threshold_pct": 90,
    "action_on_limit": "block"
  }'
```

Both user and team budgets are evaluated. If either limit is reached, the configured action applies.

## Enforcement Actions

| Action | Behavior |
|---|---|
| `notify` | Send an alert but allow requests to continue |
| `throttle` | Reduce the user's rate limit (e.g., from 60 RPM to 10 RPM) |
| `block` | Reject requests with a 429 until the next billing period |

The enforcement action is evaluated on every request, so budget changes take effect immediately — no restart or cache flush required.

## Notifications

When a user or team hits a notification threshold, CCAG sends an alert to your configured notification channel:

- **Webhook**: POST to any URL with a JSON payload
- **SNS**: Publish to an SNS topic (useful for routing to Slack, PagerDuty, email, etc.)
- **EventBridge**: Put event to EventBridge for routing to Lambda, Step Functions, etc.

Configure the notification target:

```bash
# Environment variable (applies to all notifications)
BUDGET_NOTIFICATION_URL=https://hooks.slack.com/services/xxx/yyy/zzz

# Or an SNS topic ARN
BUDGET_NOTIFICATION_URL=arn:aws:sns:us-east-1:123456789:ccag-budget-alerts
```

Notification payloads include the user, team, current spend, limit, and percentage consumed.

## Developer Self-Service

Developers can see their own spending in the portal's personal dashboard without admin access. This includes:

- Current month spend vs. budget
- Daily spend trend
- Breakdown by model (e.g., Opus vs. Sonnet usage)
- Remaining budget

This visibility alone reduces overspend — developers naturally adjust behavior when they can see their costs in real time.

## Analytics for Admins

The admin portal's Spend tab provides:

- **Time-series charts**: spend over time with hourly/daily/weekly granularity
- **Filters**: by team, user, model, endpoint
- **Forecast**: OLS trend line projecting end-of-month spend
- **Export**: CSV download for finance teams

Example questions the dashboard answers:
- Which team is spending the most this month?
- Is Opus usage growing faster than Sonnet?
- Which users are approaching their budget limits?
- What is our projected monthly Bedrock bill?

## Spend Tracking Architecture

CCAG uses an async spend tracker to minimize request latency. Token counts from each response are buffered in memory and flushed to Postgres in batches. This adds zero latency to the request path — budget checks use cached values that are updated within seconds of the actual spend.

The `spend_log` table stores one row per request with full attribution. The analytics queries aggregate this table — no separate data pipeline or warehouse is needed.

## See Also

- [Configuration](../configuration.md): notification and budget environment variables
- [Enterprise Deployment](enterprise-deployment.md): full team setup guide
- [Comparison](../comparison.md): budget features across CCAG, LiteLLM, and Direct Bedrock
