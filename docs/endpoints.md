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

### Cross-Region Inference (default)

An endpoint is a **deployment target**: an AWS account, region, credential set, and routing scope. By default, all model requests route via Bedrock's cross-region inference (CRI) — Bedrock selects the nearest healthy region within the chosen geographic scope and invokes the appropriate system inference profile (e.g., `us.anthropic.claude-sonnet-4-5-20250929-v1:0`). No additional configuration is needed.

### AIP Overrides

Application Inference Profiles (AIPs) let you attach cost-allocation tags, custom throttle limits, or cross-account resource policies to specific model invocations. In CCAG, AIPs are configured **per-model** on each endpoint via the `endpoint_aip_overrides` table. Each override row maps a logical model (e.g., `claude-sonnet-4-5`) to a specific AIP ARN. Models without an override fall through to the CRI path automatically.

This means one endpoint can cover many models: some tagged via AIP, others served by CRI, all from the same AWS credentials.

See [AIP Overrides](#aip-overrides-1) below for the management API and a worked example.

### Geographic Scope

The `routing_prefix` determines which cross-region inference scope Bedrock uses:

| Prefix | Scope | Regions |
|---|---|---|
| `us` | North America | Virginia, Oregon, Ohio, N. California, Canada |
| `eu` | Europe | Frankfurt, Paris, London, Stockholm, Ireland |
| `apac` | Asia Pacific | Tokyo, Mumbai, Singapore, Seoul, Osaka |
| `au` | Australia | Sydney, Melbourne |
| `us-gov` | GovCloud | GovCloud West, GovCloud East |
| `global` | All participating regions | Managed by Bedrock; use with CRI only |

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
| `bedrock:GetInferenceProfile` | Yes (AIP endpoints) | AIP health checks and auto-migration |
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
        "bedrock:ListInferenceProfiles",
        "bedrock:GetInferenceProfile"
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

## AIP Overrides

### Worked Example: Tagging Sonnet and Opus on One Endpoint

Create the endpoint (no `inference_profile_arn` needed):

```bash
curl -X POST https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "US Production",
    "region": "us-east-1",
    "routing_prefix": "us",
    "priority": 0,
    "enabled": true
  }'
# {"id": "11111111-1111-1111-1111-111111111111", "name": "US Production", ...}
```

Add a Sonnet AIP override:

```bash
curl -X POST https://ccag.example.com/admin/endpoints/11111111-1111-1111-1111-111111111111/aip-overrides \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "model_id": "claude-sonnet-4-5",
    "aip_arn": "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123sonnet",
    "reason": "cost allocation tag: team=platform"
  }'
# {"endpoint_id": "11111111-...", "model_id": "claude-sonnet-4-5", "aip_arn": "arn:aws:bedrock:...", "set_by": "admin", ...}
```

Add an Opus AIP override:

```bash
curl -X POST https://ccag.example.com/admin/endpoints/11111111-1111-1111-1111-111111111111/aip-overrides \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "model_id": "claude-opus-4-5",
    "aip_arn": "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/def456opus",
    "reason": "cost allocation tag: team=platform"
  }'
```

After both are configured, `GET /admin/endpoints/{id}` returns:

```json
{
  "id": "11111111-1111-1111-1111-111111111111",
  "name": "US Production",
  "region": "us-east-1",
  "routing_prefix": "us",
  "inference_profile_arn": null,
  "aip_overrides": [
    {
      "model_id": "claude-sonnet-4-5",
      "aip_arn": "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc123sonnet",
      "set_by": "admin",
      "reason": "cost allocation tag: team=platform"
    },
    {
      "model_id": "claude-opus-4-5",
      "aip_arn": "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/def456opus",
      "set_by": "admin",
      "reason": "cost allocation tag: team=platform"
    }
  ],
  "health_status": "healthy"
}
```

**Request routing for this endpoint:**

| Requested model | Invocation target | Reason |
|---|---|---|
| `claude-sonnet-4-5` | `arn:aws:bedrock:...:application-inference-profile/abc123sonnet` | AIP override match |
| `claude-opus-4-5` | `arn:aws:bedrock:...:application-inference-profile/def456opus` | AIP override match |
| `claude-haiku-4-5` | `us.anthropic.claude-haiku-4-5-20251001-v1:0` | No override → CRI |

Haiku has no override configured, so the gateway falls through to the system CRI profile for the endpoint's `routing_prefix`. No configuration needed for Haiku — it just works.

### Managing AIP Overrides

**List overrides for an endpoint:**

```bash
curl https://ccag.example.com/admin/endpoints/{id}/aip-overrides \
  -H "authorization: Bearer $TOKEN"
```

**Add or update an override (`model_id` is the upsert key):**

```bash
curl -X POST https://ccag.example.com/admin/endpoints/{id}/aip-overrides \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"model_id": "claude-sonnet-4-5", "aip_arn": "arn:aws:bedrock:...:application-inference-profile/abc123", "reason": "..."}'
```

**Remove an override:**

```bash
curl -X DELETE https://ccag.example.com/admin/endpoints/{id}/aip-overrides/claude-sonnet-4-5 \
  -H "authorization: Bearer $TOKEN"
```

CLI equivalents:

```bash
ccag endpoints aip-overrides list <endpoint-name-or-id>
ccag endpoints aip-overrides add <endpoint-name-or-id> --model claude-sonnet-4-5 --arn arn:aws:bedrock:... [--reason "..."]
ccag endpoints aip-overrides remove <endpoint-name-or-id> claude-sonnet-4-5
```

### Backwards Compatibility

The `inference_profile_arn` field on `POST /admin/endpoints` continues to work for one release cycle. When set, CCAG automatically creates a single `aip_overrides` row (using `GetInferenceProfile` to identify the underlying model) and returns the response with both `aip_overrides: [...]` and `inference_profile_arn` populated, plus `Deprecation: true` and `Sunset` response headers (RFC 8594). Migrate to the `aip-overrides` endpoints for new tooling.

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

1. **CRI check:** calls `ListInferenceProfiles` to validate credentials and discover available models. Profiles matching the endpoint's `routing_prefix` are added to `available_models`.
2. **AIP override check:** for each row in `endpoint_aip_overrides`, calls `GetInferenceProfile` to verify the AIP is reachable and reads its underlying foundation model. The resolved model ID is added to `available_models`.

The endpoint is marked **healthy** only if both steps succeed. A single unresolvable AIP ARN marks the entire endpoint unhealthy until the override is corrected or removed.

`available_models` is the deduplicated union from both steps. This union feeds the model-availability check on each request (`filter_by_model`) and the 1M-context capability-probe loop.

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

## Migration from a Pre-1.8.0 Release

**Zero operator action required.** On first startup after upgrading to 1.8.0, CCAG runs an auto-migration for every endpoint that has a legacy `inference_profile_arn` populated and no rows yet in `endpoint_aip_overrides`:

1. Calls `GetInferenceProfile` on the legacy ARN.
2. Parses the underlying foundation model from the ARN tail.
3. Inserts an `endpoint_aip_overrides` row with `set_by = "auto-migration"`.
4. Leaves the legacy `inference_profile_arn` column intact for one release.

If `GetInferenceProfile` fails (transient credential issue, AIP deleted), startup still completes and the endpoint continues to serve traffic via the legacy code path. The migration retries on the next restart.

**Verifying the migration:**

```bash
# CLI
ccag endpoints aip-overrides list <endpoint-name>

# API
curl https://ccag.example.com/admin/endpoints/{id} \
  -H "authorization: Bearer $TOKEN"
# Look for "aip_overrides": [...] and "inference_profile_arn": "<legacy-arn>"
# Both will be populated immediately after the migration.
```

**Behaviour change to be aware of:** before 1.8.0, an endpoint with `inference_profile_arn` set would route **every** request through that ARN regardless of which model the client requested (silent substitution). After the auto-migration, only the specific model the AIP is tagged with routes through the AIP; other models route via CRI.

If you relied on the old "force every request through one AIP regardless of model" behaviour, replicate it by adding an AIP override row for each model you want to tag. To keep the exact old behaviour for a single-model AIP, no action is needed — the auto-migrated row handles it.

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
| `GET` | `/admin/endpoints/{id}/aip-overrides` | List AIP overrides for an endpoint |
| `POST` | `/admin/endpoints/{id}/aip-overrides` | Add or update an AIP override |
| `DELETE` | `/admin/endpoints/{id}/aip-overrides/{model_id}` | Remove an AIP override |
| `GET` | `/admin/teams/{team_id}/endpoints` | Get team endpoint assignments and routing strategy |
| `PUT` | `/admin/teams/{team_id}/endpoints` | Set team endpoint assignments and routing strategy |

## See Also

- [Configuration](configuration.md): environment variables and runtime settings
- [Getting Started](getting-started.md): initial deployment
- [Metrics](metrics.md): per-endpoint error and throttle metrics
- [Budgets](budgets.md): budget enforcement interacts with endpoint routing
