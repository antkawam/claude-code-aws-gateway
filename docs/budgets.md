---
title: "Budget Management for Claude Code Teams"
description: "Set per-user and per-team spending limits with daily, weekly, or monthly resets. Enforce via notifications, throttling, or blocking."
---

# Budgets

CCAG enforces spending limits per user and per team. Budgets reset on a configurable period (daily, weekly, or monthly) and support three enforcement modes: notify, shape (throttle), and block.

## Budget Hierarchy

Budgets are evaluated at two levels. Both are checked on every request, and the most restrictive result applies.

**User budget**: resolved in this order (first non-null wins):
1. Explicit limit set on the user
2. Team's default per-member limit
3. Global default budget (in gateway settings)
4. No limit

**Team budget**: the total spend of all team members counted against the team's budget. Evaluated independently from user budgets.

If a user is under their own limit but the team is over its limit, the request is blocked (or shaped, depending on the team's policy).

## Setting Budgets

### Team Budgets

Set through the admin portal (Teams section) or the API:

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

| Field | Description |
|---|---|
| `budget_amount_usd` | Total team spending limit for the period |
| `budget_period` | `daily`, `weekly`, or `monthly` |
| `budget_policy` | Enforcement policy: `standard`, `soft`, `shaped`, or a custom rule array |
| `default_user_budget_usd` | Per-member limit inherited by users without an explicit budget |

### User Budgets

Set an explicit limit on a specific user (overrides team default):

```bash
curl -X PUT https://ccag.example.com/admin/users/{user_id}/spend-limit \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"limit_usd": 100.00}'
```

Set to `null` to remove the explicit limit and fall back to the team or global default.

### Global Default Budget

Set a fallback budget for users who are not assigned to a team or whose team has no default:

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

## Budget Periods

| Period | Resets at |
|---|---|
| `daily` | 00:00 UTC each day |
| `weekly` | Monday 00:00 UTC |
| `monthly` | 1st of month 00:00 UTC |

Spend is aggregated from the `spend_log` table for the current period only. Historical spend from prior periods does not carry over.

## Enforcement Policies

A policy is a list of rules evaluated in order. Each rule specifies a threshold (percentage of the budget) and an action.

### Preset Policies

**standard** (default):
| At | Action |
|---|---|
| 80% | Notify (warning sent, request continues) |
| 100% | Block (request rejected with 429) |

**soft**:
| At | Action |
|---|---|
| 80% | Notify |
| 100% | Notify (allows overage) |
| 150% | Block |

**shaped**:
| At | Action |
|---|---|
| 80% | Notify |
| 100% | Shape (throttle to 5 RPM) |
| 150% | Block |

### Custom Policies

Pass a JSON array of rules instead of a preset name:

```bash
curl -X PUT https://ccag.example.com/admin/teams/{team_id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "budget_amount_usd": 200.00,
    "budget_period": "weekly",
    "budget_policy": [
      {"at_percent": 50, "action": "notify"},
      {"at_percent": 90, "action": "notify"},
      {"at_percent": 100, "action": {"shape": {"rpm": 3}}},
      {"at_percent": 200, "action": "block"}
    ]
  }'
```

Rules:
- Up to 5 rules per policy
- `at_percent` values must be in ascending order
- Only one `block` rule allowed (must be last)
- `shape` accepts an `rpm` parameter (requests per minute)

## How Shaping Works

When a budget evaluation returns `shape`, CCAG applies a synthetic rate limit to the user:

1. A per-user rate limit key is created (based on the user's identity)
2. The sliding-window rate limiter enforces the configured RPM
3. Requests within the RPM proceed normally
4. Requests exceeding the RPM return 429 with a `Retry-After` header

Shaping lets users continue working at a reduced rate rather than being fully blocked. The RPM resets each sliding window (60 seconds).

## Response Headers

Every proxied request includes budget status in the response headers:

| Header | Values | Description |
|---|---|---|
| `x-ccag-budget-status` | `ok`, `warning`, `shaped`, `blocked` | Current enforcement state |
| `x-ccag-budget-percent` | `82.4` | Spend as percentage of limit |
| `x-ccag-budget-remaining-usd` | `8.80` | Remaining budget in USD |
| `x-ccag-budget-resets` | `2026-03-17T00:00:00Z` | Current period start timestamp |

These headers are present on OIDC-authenticated requests where a budget is configured.

## Notifications

Budget events trigger notifications through the configured notification destination (see [Configuration: Notifications](configuration.md#notifications)).

| Event Type | Trigger |
|---|---|
| `budget_warning` | User spend crosses a `notify` threshold |
| `budget_shaped` | User spend crosses a `shape` threshold |
| `budget_blocked` | User spend crosses a `block` threshold |
| `team_budget_warning` | Team spend crosses a `notify` threshold |
| `team_budget_shaped` | Team spend crosses a `shape` threshold |
| `team_budget_blocked` | Team spend crosses a `block` threshold |

Events are deduplicated: the same threshold fires only once per user (or team) per period. Notifications are delivered asynchronously every 30 seconds.

## Token Pricing

Spend is estimated using per-model token pricing (USD per 1M tokens):

| Model | Input | Output | Cache Read | Cache Write |
|---|---|---|---|---|
| Claude Opus 4.5 / 4.6 | $5.00 | $25.00 | $0.50 | $6.25 |
| Claude Sonnet (default) | $3.00 | $15.00 | $0.30 | $3.75 |
| Claude Haiku 4.5 | $1.00 | $5.00 | $0.10 | $1.25 |

These rates are hardcoded in the `estimate_cost_usd()` database function. Spend figures are estimates, not billing-grade calculations.

## API Reference

| Method | Path | Description |
|---|---|---|
| `PUT` | `/admin/teams/{team_id}/budget` | Set team budget, period, policy, default user limit |
| `PUT` | `/admin/users/{user_id}/spend-limit` | Set explicit user spending limit |
| `GET` | `/admin/settings/default-budget` | Get global default budget |
| `PUT` | `/admin/settings/default-budget` | Set global default budget |
| `GET` | `/admin/budget/status` | Get current user's budget status, spend, and events |

## See Also

- [Configuration](configuration.md): notification destination setup
- [Authentication](authentication.md): user and team management
- [Endpoints](endpoints.md): endpoint routing interacts with team budgets
- [Metrics](metrics.md): spend-related metrics
