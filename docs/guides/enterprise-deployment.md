---
title: "Self-Hosted Claude Code for Enterprises on AWS"
description: "Deploy Claude Code for your entire team on Amazon Bedrock with centralized budget controls, OIDC SSO, SCIM provisioning, audit trails, and a built-in admin portal."
---

# Self-Hosted Claude Code for Enterprises on AWS

Amazon Bedrock lets you run Claude inference in your AWS account, but it provides no team management layer. You can run Claude Code on Bedrock today — but every developer needs their own AWS credentials, there is no per-user spend visibility, no budget enforcement, no SSO, and no centralized audit trail. As a team scales from 5 to 50 to 500 engineers, managing this manually becomes untenable.

CCAG is a self-hosted API gateway that sits between Claude Code and Bedrock. Developers connect to CCAG with a virtual API key; the gateway handles authentication, authorization, budget enforcement, and request routing. Infrastructure teams get a single deployment to manage instead of per-developer AWS credential configurations.

## What Enterprises Need

| Requirement | Direct Bedrock | Through CCAG |
|---|---|---|
| Single sign-on (Okta, Azure AD, Google) | No | OIDC with any compliant IdP |
| Automated user provisioning/deprovisioning | No | SCIM 2.0 (Okta, Entra ID, authentik) |
| Per-user and per-team spending limits | No | Configurable budgets (notify, throttle, block) |
| Audit trail of requests and spend | No | Full spend log with user, model, token counts |
| Centralized API key management | No | Virtual keys with rate limits and expiry |
| Multi-account quota pooling | No | Multi-endpoint pool with automatic failover |
| Data residency control | Partial | Endpoint-level region assignment per team |
| Developer self-service onboarding | No | One-command setup from Connect page |

## Architecture

```
Developer Machine
  └── Claude Code
        └── ANTHROPIC_BASE_URL → CCAG (ECS Fargate or self-hosted)
                                    ├── Auth (virtual keys, OIDC JWTs, session tokens)
                                    ├── Budget enforcement (per-user, per-team)
                                    ├── Rate limiting (per-key sliding window)
                                    ├── Endpoint routing (multi-account, multi-region)
                                    └── Bedrock Runtime (your AWS account)
```

CCAG presents as the Anthropic Messages API. Claude Code connects to it with `ANTHROPIC_BASE_URL` and gets full feature access including extended thinking, tool use, and web search — none of which are available when connecting to Bedrock directly with `CLAUDE_CODE_USE_BEDROCK=1`.

## Identity: OIDC SSO and SCIM Provisioning

### OIDC Single Sign-On

CCAG supports any OIDC-compliant identity provider. Developers log in to the portal using their corporate identity; no separate password is required. Multiple IDPs can be configured simultaneously — useful during migrations or for organizations with multiple identity systems.

Supported providers include Okta, Azure AD / Entra ID, Google Workspace, Ping Identity, Keycloak, and any provider that implements the OIDC discovery spec.

Register an IDP through the admin portal or API:

```bash
curl -X POST https://ccag.example.com/admin/idps \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "Corporate Okta",
    "issuer": "https://your-org.okta.com",
    "client_id": "0oa...",
    "client_secret": "...",
    "auto_provision": true
  }'
```

With `auto_provision: true`, users are created in CCAG on first login without any admin action.

### SCIM 2.0 Provisioning

For larger organizations, managing user lifecycle manually does not scale. CCAG implements SCIM 2.0, allowing your IdP to push user and group changes directly:

- Users are created when onboarded in Okta or Entra ID
- Users are deactivated (soft-deleted, spend history preserved) when offboarded
- Role assignments (`admin` / `member`) are managed via SCIM Groups
- No CCAG admin action required for routine user lifecycle events

Configure SCIM in the portal under **Identity Providers**. Generate a SCIM bearer token, then paste the base URL (`https://ccag.example.com/scim/v2`) and token into your IdP's application settings.

See [SCIM 2.0 API](../scim-spec.md) for the full provisioning spec.

## Budget Controls

Every request goes through budget enforcement before reaching Bedrock. Budgets operate at two levels simultaneously — the more restrictive one applies.

### Setting a Team Budget

```bash
curl -X PUT https://ccag.example.com/admin/teams/{team_id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "budget_amount_usd": 2000.00,
    "budget_period": "monthly",
    "budget_policy": "standard",
    "default_user_budget_usd": 100.00
  }'
```

`default_user_budget_usd` sets a per-member limit inherited by all team members unless overridden individually.

### Enforcement Modes

| Mode | Behavior |
|---|---|
| `notify` | Alert is sent; request is allowed |
| `shape` | Request is allowed; RPM is throttled to a configured rate |
| `block` | Request is rejected with HTTP 402 |

The `standard` policy notifies at 80% spend and blocks at 100%. Custom rule arrays give you fine-grained control:

```bash
curl -X PUT https://ccag.example.com/admin/teams/{team_id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "budget_amount_usd": 1000.00,
    "budget_period": "monthly",
    "budget_policy": [
      {"at_percent": 70, "action": "notify"},
      {"at_percent": 90, "action": "shape", "shaped_rpm": 10},
      {"at_percent": 100, "action": "block"}
    ]
  }'
```

Budget events are delivered via webhook, SNS, or EventBridge. Configure notification channels in the portal under **Settings > Notifications**.

See [Budget Controls](budget-controls.md) and [Budget Management](../budgets.md) for the full reference.

## Audit Trail and Analytics

The admin portal includes a cross-org analytics dashboard with four tabs: Spend, Activity, Models, and Tools. All data comes from the `spend_log` table — no additional infrastructure required.

Filters include team, user, model, and endpoint. Time range and granularity (hourly, daily, weekly) are adjustable. All charts can be exported as CSV for reporting.

For compliance requirements, the spend log records:

- Timestamp
- User identity (subject claim from OIDC JWT or virtual key name)
- Team assignment
- Model ID and version
- Input and output token counts
- Estimated USD cost
- Endpoint used (region and account)
- MCP tools invoked

The log is stored in your Postgres database. Retention is managed by your standard database backup and rotation policies.

## Developer Onboarding

Once CCAG is deployed and configured, developer onboarding is a single command. The portal's **Connect** page generates a setup script that installs Claude Code (if needed), creates a virtual API key, and sets `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` in the shell profile:

```bash
curl -fsSL https://ccag.example.com/setup | sh
```

No AWS credentials, no region configuration, no manual environment variables. Developers are productive immediately.

For CLI-based onboarding (recommended for OIDC deployments):

```bash
cargo install ccag-cli
ccag login https://ccag.example.com  # opens browser for SSO
```

After login, Claude Code is automatically configured to use the gateway.

## Deployment Options

### Docker Compose (Evaluation or Small Teams)

```bash
cp .env.example .env
# Set AWS_REGION and AWS credentials in .env
docker compose up -d
```

The gateway starts at `http://localhost:8080`. Suitable for evaluation, solo use, or small teams where high availability is not required.

### AWS CDK (Production)

For teams that need managed infrastructure:

```bash
cd infra && npm install
# Configure environments.json with account ID, region, domain
npx cdk deploy
```

This provisions a production stack: VPC, Application Load Balancer, ECS Fargate (ARM64/Graviton), RDS Postgres, autoscaling, and CloudWatch alarms. See [`infra/README.md`](../../infra/README.md) for the full deployment guide.

## Data Residency

If your organization has data residency requirements, pin each team to a specific Bedrock region by assigning team-specific endpoints:

```bash
# Create an EU-only endpoint
curl -X POST https://ccag.example.com/admin/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "EU Compliance",
    "region": "eu-west-1",
    "routing_prefix": "eu",
    "priority": 0
  }'

# Assign the EU team to the EU endpoint only
curl -X PUT https://ccag.example.com/admin/teams/{eu_team_id}/endpoints \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "routing_strategy": "primary_fallback",
    "endpoints": [{"endpoint_id": "eu-endpoint-uuid", "priority": 0}]
  }'
```

Teams assigned to EU-only endpoints will never have requests routed to US or APAC regions.

## See Also

- [Getting Started](../getting-started.md): full deployment guide
- [Authentication](../authentication.md): OIDC SSO setup and virtual key management
- [SCIM 2.0 API](../scim-spec.md): automated user provisioning reference
- [Budget Controls](budget-controls.md): per-user and per-team spending limits
- [Endpoint Routing](../endpoints.md): multi-region and multi-account routing
