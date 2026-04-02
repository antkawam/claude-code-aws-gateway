---
title: "Eliminate 429 Rate Limit Errors in Claude Code on Bedrock"
description: "Claude Code hitting 429 rate limits on Amazon Bedrock? CCAG's multi-endpoint pool spans accounts and regions with automatic failover, so quota limits no longer block your team."
---

# Eliminate 429 Rate Limit Errors in Claude Code on Bedrock

If your Claude Code session drops mid-task with a `429 Too Many Requests` or a message like "Claude is currently unavailable, please try again," you have hit Bedrock's per-region quota. This is one of the most common pain points when running Claude Code through Amazon Bedrock.

## Why Bedrock Returns 429 Errors

Amazon Bedrock enforces per-account, per-region, per-model token-rate quotas. The defaults are conservative and were not designed for interactive coding workloads — a single heavy Claude Code session can saturate the default quota in minutes. If your team shares one account and region, a quota burst from one developer blocks everyone else.

The default quotas vary by region and model, but even after requesting increases from AWS Support, a single account/region pair has a ceiling. Once that ceiling is hit, every additional request gets a 429 until the token bucket refills.

## How CCAG Solves It

CCAG maintains a pool of Bedrock endpoints. Each endpoint is an independent Bedrock runtime client with its own credentials, region, and quota. When one endpoint returns a 429, CCAG automatically retries the request on the next healthy endpoint in priority order — transparently to Claude Code.

This means you can:

- **Pool quota across multiple accounts**: Add endpoints that assume IAM roles in separate AWS accounts. Each account has its own quota.
- **Pool quota across regions**: Add endpoints in `us-east-1`, `us-west-2`, and `eu-west-1`. Claude's cross-region inference profiles let Bedrock route within each geographic scope for latency optimization.
- **Isolate teams**: Assign high-volume teams to dedicated endpoints so one team's burst does not affect another.

Claude Code sees none of this — it connects to CCAG's Anthropic-compatible endpoint and the gateway handles failover internally.

## Setting Up a Multi-Endpoint Pool

### Step 1: Deploy CCAG

If you have not deployed CCAG yet, see [Getting Started](../getting-started.md). The gateway connects to Bedrock using your gateway's IAM role by default. The first endpoint is created automatically on startup.

### Step 2: Add Additional Endpoints

**Via the admin portal:**

Log in to the portal, go to **Endpoints**, and click **Add Endpoint**.

**Via the API:**

```bash
# Add a second endpoint in a different region (same account)
curl -X POST https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "US East Overflow",
    "region": "us-east-1",
    "routing_prefix": "us",
    "priority": 10,
    "enabled": true
  }'
```

Lower `priority` values are preferred. The primary endpoint at priority 0 is used first; the overflow endpoint at priority 10 is only used when the primary returns 429 or 5xx.

### Step 3: Add a Cross-Account Endpoint

If you have Bedrock quota in a second AWS account, add an endpoint that assumes a role there:

```bash
# In the target account: create a role trusting your gateway's task role
# Then add the endpoint:
curl -X POST https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "Account B - US",
    "region": "us-west-2",
    "routing_prefix": "us",
    "role_arn": "arn:aws:iam::222222222222:role/CCAGBedrockAccess",
    "external_id": "ccag-prod",
    "priority": 20
  }'
```

The target account needs a role trust policy pointing to your gateway's task role. See [Endpoint Routing](../endpoints.md) for the full IAM policy examples.

### Step 4: Choose a Routing Strategy

Assign a routing strategy per team. The default is `sticky_user`, which keeps a user on the same endpoint for up to 30 minutes to preserve Bedrock prompt cache hits. When the sticky endpoint returns 429, CCAG automatically moves the user to the next healthy endpoint.

```bash
curl -X PUT https://ccag.example.com/admin/teams/{team_id}/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "routing_strategy": "sticky_user",
    "endpoints": [
      { "endpoint_id": "uuid-primary", "priority": 0 },
      { "endpoint_id": "uuid-overflow", "priority": 10 },
      { "endpoint_id": "uuid-account-b", "priority": 20 }
    ]
  }'
```

## Routing Strategies

| Strategy | Behavior | Best For |
|---|---|---|
| `sticky_user` | User stays on same endpoint; fails over on 429/5xx | Default; preserves prompt cache |
| `primary_fallback` | Always tries highest-priority first | Simple active/standby |
| `round_robin` | Rotates across all healthy endpoints | Maximum throughput distribution |

## Monitoring Throttle Events

CCAG tracks per-endpoint throttle counts over a 1-hour rolling window. View them in the portal's Endpoints section or via:

```bash
curl https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN"
```

Each endpoint in the response includes `throttle_count`, `error_count`, and `request_count` over the last hour. Use this to size your pool — if `throttle_count` is high on all endpoints simultaneously, you need more quota, not more regions.

You can also view Bedrock service quotas per endpoint (requires `servicequotas:ListServiceQuotas`):

```bash
curl https://ccag.example.com/admin/endpoints/{endpoint_id}/quotas \
  -H "authorization: Bearer $TOKEN"
```

## Per-Key Rate Limits

In addition to endpoint-level failover, CCAG supports per-key RPM limits. These are enforced by CCAG before requests even reach Bedrock, so a runaway process cannot exhaust Bedrock quota for the entire team:

```bash
curl -X POST https://ccag.example.com/admin/keys \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"name": "ci-pipeline", "rate_limit_rpm": 60}'
```

This means you can give CI pipelines a lower RPM ceiling while interactive sessions get higher limits.

## See Also

- [Endpoint Routing](../endpoints.md): complete reference for endpoints, IAM policies, and routing strategies
- [Configuration](../configuration.md): environment variables
- [Metrics](../metrics.md): Prometheus metrics for throttle and error rates
