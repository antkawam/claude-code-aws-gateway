---
title: "Deploying Claude Code for Teams: Budget Controls, SSO, and Audit Trails"
description: "Deploy Claude Code for your engineering team on AWS with per-user budgets, OIDC SSO, SCIM provisioning, and a centralized admin portal."
---

# Deploying Claude Code for Teams

Running Claude Code for a single developer is straightforward. Running it for a team of 20, 50, or 200 developers on AWS — with cost visibility, access control, and audit trails — requires infrastructure that neither Anthropic Direct nor Bedrock provide out of the box.

CCAG provides that infrastructure as a single deployable service.

## What Teams Need

| Requirement | Anthropic Direct | Bedrock Direct | CCAG |
|---|---|---|---|
| Centralized billing | Per-seat subscription | AWS bill (no per-user breakdown) | Per-user/team spend tracking |
| Access control | API keys | IAM roles | OIDC SSO + virtual keys |
| Budget enforcement | None | None | Notify, throttle, or block per user/team |
| User provisioning | Manual | Manual | SCIM 2.0 (Okta, Entra ID, etc.) |
| Usage analytics | None | CloudWatch (aggregate) | Per-user, per-model, per-tool dashboard |
| Audit trail | None | CloudTrail (API-level) | Request-level spend log with user attribution |

## Architecture

```
Developer laptops                    Your AWS account
┌──────────────┐                    ┌──────────────────────┐
│ Claude Code   │─── HTTPS ────────▶│  CCAG (ECS Fargate)  │
│ (ANTHROPIC_   │                   │  ┌────────────────┐  │
│  BASE_URL)    │                   │  │ Auth (OIDC/keys)│  │
└──────────────┘                    │  │ Budgets         │  │
                                    │  │ Rate limits     │  │
                                    │  │ Analytics       │  │
                                    │  └───────┬────────┘  │
                                    │          │           │
                                    │  ┌───────▼────────┐  │
                                    │  │ Amazon Bedrock  │  │
                                    │  └────────────────┘  │
                                    │  ┌────────────────┐  │
                                    │  │ Postgres (RDS)  │  │
                                    │  └────────────────┘  │
                                    └──────────────────────┘
```

## Developer Onboarding

With CCAG, onboarding a new developer takes one command — no AWS credentials, no IAM roles, no config files.

### Option 1: Browser-Based Login (OIDC)

Developers authenticate through your existing identity provider:

```bash
# Install the CLI
cargo install ccag-cli

# Log in via browser (opens your SSO provider)
ccag login https://ccag.example.com

# That's it — Claude Code is configured
claude
```

The `ccag login` command opens a browser for OIDC authentication, receives a session token, and writes `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` to the developer's Claude Code configuration.

### Option 2: Virtual API Key

For CI/CD pipelines or developers who prefer manual setup:

```bash
export ANTHROPIC_BASE_URL=https://ccag.example.com
export ANTHROPIC_API_KEY=sk-proxy-xxxxx  # from the portal
```

Admins create keys in the portal or via the API, scoped to a team with a specific rate limit and budget.

## OIDC SSO Setup

CCAG supports any OIDC provider that exposes a `.well-known/openid-configuration` endpoint. Multiple providers can be active simultaneously.

### Configure via Environment Variable (Bootstrap)

```bash
OIDC_ISSUER=https://login.microsoftonline.com/{tenant}/v2.0
```

### Configure via Admin Portal (Runtime)

Go to **Settings > Identity Providers > Add Provider**. Enter the issuer URL, client ID, and optional audience. CCAG auto-discovers the JWKS endpoint.

Supported providers include Okta, Azure AD (Entra ID), Google Workspace, Auth0, Keycloak, AWS IAM Identity Center, and any standard OIDC-compliant provider.

## SCIM 2.0 Provisioning

For organizations using Okta or Entra ID, CCAG supports SCIM 2.0 for automatic user and group provisioning. When a user is added to your IdP, they are automatically created in CCAG. When removed, their access is revoked.

Configure SCIM in your IdP pointing to `https://ccag.example.com/scim/v2` with a bearer token generated from the admin portal.

## Budget Enforcement

Set spending limits at the user or team level. When a threshold is reached, CCAG can:

- **Notify**: Send an alert via webhook, SNS, or EventBridge
- **Throttle**: Reduce the user's rate limit
- **Block**: Reject requests until the next billing period

```bash
# Set a $50/month budget for a team with notification at 80%
curl -X PUT https://ccag.example.com/admin/teams/{id}/budget \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "monthly_limit_usd": 50.00,
    "notification_threshold_pct": 80,
    "action_on_limit": "block"
  }'
```

Budget data is visible to developers in the portal's personal dashboard, so they can self-manage their usage.

## Analytics Dashboard

The built-in admin portal provides four analytics views:

- **Spend**: Total and per-user cost over time, with forecast
- **Activity**: Request volume, latency, error rates
- **Models**: Usage breakdown by model (which teams use which models)
- **Tools**: Tool use frequency (how often web search, file editing, etc. are invoked)

All views support multi-select filters (team, user, model, endpoint) and adjustable time ranges with hourly/daily/weekly granularity. Data can be exported as CSV.

## Deployment

CCAG deploys as a single container with a Postgres database. The included AWS CDK stack provisions ECS Fargate, RDS, ALB, and CloudWatch in one command:

```bash
cd infra
npx cdk deploy -c environment=prod
```

Or use Docker Compose for smaller deployments:

```bash
docker compose up -d
```

See [Getting Started](../getting-started.md) for the full deployment guide.

## See Also

- [Configuration](../configuration.md): all environment variables
- [SCIM Provisioning](../scim.md): SCIM 2.0 setup guide
- [Endpoint Routing](../endpoints.md): multi-account/region setup
- [Comparison](../comparison.md): CCAG vs LiteLLM vs Direct Bedrock
