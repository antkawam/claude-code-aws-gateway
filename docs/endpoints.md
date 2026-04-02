---
title: "Multi-Account Endpoint Routing"
description: "Route Claude Code requests across multiple AWS accounts and regions for quota pooling, workload isolation, and regional failover."
---

# Endpoint Routing

CCAG can route requests across multiple AWS accounts and regions through endpoint configuration. Each endpoint is a Bedrock runtime client with its own credentials, region, and routing prefix. Adding multiple endpoints lets you pool quota, isolate workloads, or provide regional failover.

## Adding an Endpoint

Create endpoints through the admin portal (Endpoints section) or the API:

```bash
curl -X POST https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "US Production",
    "region": "us-west-2",
    "routing_prefix": "us",
    "priority": 0,
    "enabled": true
  }'
```

### Routing Target

Each endpoint uses one of two routing modes:

**Cross-region inference** (default): Bedrock routes the request to the nearest healthy region within the chosen geographic scope (US, EU, APAC, etc.) using system-defined inference profiles. Set the `region` (API connection region) and `routing_prefix`.

**Application inference profile**: Invokes a specific profile ARN directly. Use this for custom profiles with cost-tracking tags, custom throttle limits, or cross-account access granted via a resource policy on the inference profile.

```bash
curl -X POST https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "Tagged Profile",
    "region": "us-west-2",
    "routing_prefix": "us",
    "inference_profile_arn": "arn:aws:bedrock:us-west-2:123456789012:inference-profile/my-profile",
    "priority": 0
  }'
```

### Geographic Scope

The `routing_prefix` determines which cross-region inference scope Bedrock uses:

| Prefix | Scope | Regions |
|---|---|---|
| `us` | North America | Virginia, Oregon, Ohio, N. California, Canada |
| `eu` | Europe | Frankfurt, Paris, London, Stockholm, Ireland |
| `apac` | Asia Pacific | Tokyo, Mumbai, Singapore, Seoul, Osaka |
| `au` | Australia | Sydney, Melbourne |
| `us-gov` | GovCloud | GovCloud West, GovCloud East |

Global scope (all participating regions) is available through application inference profiles.

### Credentials

**Gateway's own account** (default): The endpoint uses the gateway's IAM role or access keys. This works for same-account Bedrock access and for cross-account access where the target account grants access via a resource policy on the inference profile.

**Assume an IAM role**: The gateway calls STS AssumeRole before each Bedrock call. Use this when the Bedrock quota lives in a different account.

```bash
curl -X POST https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "Cross-Account",
    "region": "us-east-1",
    "routing_prefix": "us",
    "role_arn": "arn:aws:iam::222222222222:role/CCAGBedrockAccess",
    "external_id": "ccag-prod-2026",
    "priority": 10
  }'
```

Add an External ID if the role trust policy includes an `sts:ExternalId` condition. This prevents confused-deputy attacks.

### Required IAM Permissions

The endpoint's credentials (gateway role or assumed role) need:

| Permission | Required | Purpose |
|---|---|---|
| `bedrock:InvokeModel` | Yes | Inference |
| `bedrock:InvokeModelWithResponseStream` | Yes | Streaming inference |
| `bedrock:ListInferenceProfiles` | Yes | Model discovery and health checks |
| `servicequotas:ListServiceQuotas` | No | Quota visibility in the admin portal |

Example policy:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "bedrock:InvokeModel",
        "bedrock:InvokeModelWithResponseStream",
        "bedrock:ListInferenceProfiles"
      ],
      "Resource": "*"
    },
    {
      "Effect": "Allow",
      "Action": "servicequotas:ListServiceQuotas",
      "Resource": "*"
    }
  ]
}
```

For cross-account access, the target account's role trust policy must trust the gateway's task role:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": { "AWS": "arn:aws:iam::111111111111:role/CCAGTaskRole" },
      "Action": "sts:AssumeRole",
      "Condition": {
        "StringEquals": { "sts:ExternalId": "ccag-prod-2026" }
      }
    }
  ]
}
```

## Assigning Endpoints to Teams

By default, all requests route through the default endpoint. To assign specific endpoints to a team:

```bash
curl -X PUT https://ccag.example.com/admin/teams/{team_id}/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "routing_strategy": "sticky_user",
    "endpoints": [
      { "endpoint_id": "uuid-1", "priority": 0 },
      { "endpoint_id": "uuid-2", "priority": 10 }
    ]
  }'
```

Team-level priorities override the endpoint's global priority. Lower values have higher priority.

## Routing Strategies

Set the routing strategy per team. The strategy determines how CCAG selects among the team's assigned endpoints.

### sticky_user (default)

Maintains user-to-endpoint affinity for prompt cache reuse. Each user's requests go to the same endpoint for up to 30 minutes of inactivity, then re-evaluate.

Prompt caching on Bedrock has a 5-minute sliding TTL (extended to 1 hour on subsequent hits). Switching endpoints mid-conversation invalidates the cache, which means the next request pays the full cache write cost (1.25x input token price) instead of the cache read cost (0.1x). Sticky routing avoids this.

Failover: if the sticky endpoint returns 429 or 5xx, CCAG retries the request on the next healthy endpoint in priority order and updates the user's affinity to the new endpoint.

### primary_fallback

Routes to the highest-priority healthy endpoint. Falls back to the next endpoint in priority order on 429 or 5xx responses. No user affinity tracking.

### round_robin

Distributes requests across all healthy endpoints using a rotating counter. Falls back to the next endpoint on 429 or 5xx.

## Setting the Default Endpoint

Exactly one endpoint serves as the default for teams with no explicit assignment:

```bash
curl -X PUT https://ccag.example.com/admin/endpoints/{endpoint_id}/default \
  -H "authorization: Bearer $TOKEN"
```

On first startup with no endpoints configured, CCAG auto-creates one from the gateway's AWS region and sets it as default.

## Health Checks

CCAG health-checks every enabled endpoint every 60 seconds:

- Endpoints with an `inference_profile_arn`: calls `GetInferenceProfile` with that ARN
- Other endpoints: calls `ListInferenceProfiles` (validates credentials and region reachability)

Unhealthy endpoints are excluded from routing. When an endpoint recovers, CCAG pre-warms its quota cache.

Health status is visible in the portal and the list endpoints API:

```bash
curl https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN"
```

Each endpoint includes `health_status`: `healthy`, `unhealthy`, or `unknown` (never checked).

## Failover

When the selected endpoint returns HTTP 429 (throttled) or any 5xx status, CCAG automatically retries the request on the next healthy endpoint in priority order.

During failover, the model ID is re-prefixed to match the fallback endpoint's routing prefix (e.g., `us.anthropic.claude-...` becomes `eu.anthropic.claude-...`). This is transparent to the client.

If all endpoints fail, CCAG returns the last endpoint's error response.

## Quota Visibility

View an endpoint's Bedrock service quotas (requires `servicequotas:ListServiceQuotas` permission on the endpoint):

```bash
curl https://ccag.example.com/admin/endpoints/{endpoint_id}/quotas \
  -H "authorization: Bearer $TOKEN"
```

Quota data is cached for 5 minutes per endpoint.

## Endpoint Stats

CCAG tracks per-endpoint statistics over a 1-hour rolling window:

- Throttle count (429 responses from Bedrock)
- Error count (5xx responses from Bedrock)
- Total request count

These are visible in the admin portal's endpoint list. Stats are observational and do not affect routing decisions.

## API Reference

| Method | Path | Description |
|---|---|---|
| `GET` | `/admin/endpoints` | List all endpoints with health status and stats |
| `POST` | `/admin/endpoints` | Create an endpoint |
| `PUT` | `/admin/endpoints/{id}` | Update an endpoint |
| `DELETE` | `/admin/endpoints/{id}` | Delete an endpoint |
| `PUT` | `/admin/endpoints/{id}/default` | Set as default endpoint |
| `GET` | `/admin/endpoints/{id}/quotas` | Get Bedrock service quotas |
| `GET` | `/admin/endpoints/{id}/models` | List available inference profiles |
| `GET` | `/admin/teams/{team_id}/endpoints` | Get team endpoint assignments and routing strategy |
| `PUT` | `/admin/teams/{team_id}/endpoints` | Set team endpoint assignments and routing strategy |

## See Also

- [Configuration](configuration.md): environment variables and runtime settings
- [Getting Started](getting-started.md): initial deployment
- [Metrics](metrics.md): per-endpoint error and throttle metrics
- [Budgets](budgets.md): budget enforcement interacts with endpoint routing
