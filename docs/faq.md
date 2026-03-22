# Frequently Asked Questions

## General

### What is Claude Code AWS Gateway (CCAG)?

CCAG is a self-hosted API gateway that routes Claude Code through your Amazon Bedrock account. It translates the Anthropic Messages API into Bedrock API calls, presenting as the Anthropic Direct API so Claude Code enables extended thinking and web search. It also provides centralized management for teams: virtual API keys, budgets, rate limits, OIDC SSO, and an admin portal for observability.

### How is CCAG different from using Bedrock directly?

When you set `CLAUDE_CODE_USE_BEDROCK=1`, Claude Code connects to Bedrock directly. In this mode, extended thinking and web search are not available. CCAG sits between Claude Code and Bedrock, translating requests so Claude Code thinks it is talking to the Anthropic API. This means those features work normally. CCAG also adds team management, spend tracking, and centralized configuration that direct Bedrock usage does not provide.

### What features does CCAG unlock that direct Bedrock does not have?

By presenting as the Anthropic Direct API, CCAG enables extended thinking and web search in Claude Code. Additionally, it provides:

- Virtual key management with per-key rate limits
- Spend tracking and analytics per user/team
- OIDC SSO authentication (Okta, Azure AD, Google, etc.)
- Web search with per-user configurable providers (DuckDuckGo, Tavily, Serper, or custom)
- Budget enforcement per team
- Admin portal for self-service management
- Multi-endpoint routing across AWS accounts

### What Claude models does CCAG support?

CCAG supports Claude 4+ models on Bedrock, with hardcoded mappings for Claude Opus 4.6, Claude Sonnet 4.6, Claude Opus 4.5, Claude Sonnet 4.5, Claude Sonnet 4, and Claude Haiku 4.5. Additional models can be mapped through the admin portal. Model IDs are automatically mapped from Anthropic format (e.g., `claude-sonnet-4-20250514`) to Bedrock format (e.g., cross-region inference profile IDs).

### Is CCAG open source?

Yes. CCAG is released under the MIT license.

---

## Setup and Deployment

### What are the prerequisites for deploying CCAG?

You need:
- An AWS account with Bedrock model access enabled
- AWS CLI v2 configured with credentials
- Docker installed and running
- Rust toolchain (for building from source)
- Node.js 18+ (for CDK deployment)

See [Getting Started](getting-started.md) for the full walkthrough.

### How much does it cost to run CCAG on AWS?

The primary costs are:
- **ECS Fargate** (ARM64/Graviton): ~$15-30/month for a single task (0.25 vCPU, 0.5 GB)
- **RDS Postgres** (db.t4g.small): ~$25-30/month
- **ALB**: ~$20/month
- **NAT Gateway**: ~$35/month
- **Bedrock API calls**: Pay-per-token, same pricing as direct Bedrock usage

Total infrastructure overhead is approximately $100-120/month for a minimal staging deployment. Production with 2 tasks adds roughly $15-30/month more.

### Which AWS regions are supported?

CCAG works in any region where Bedrock is available. Model routing prefixes are auto-detected from the region:

| Region Pattern | Routing Prefix | Example |
|---|---|---|
| `us-*`, `ca-*` | `us` | US cross-region inference |
| `eu-*` | `eu` | EU cross-region inference |
| `ap-southeast-2`, `ap-southeast-4` | `au` | Australia |
| `ap-*`, `me-*` | `apac` | Asia Pacific |
| `us-gov-*` | `us-gov` | GovCloud |

### Can I deploy CCAG in multiple AWS accounts?

Yes. CCAG supports multi-endpoint routing, where a single gateway instance can route requests to Bedrock in different AWS accounts or regions. Configure endpoints through the admin portal's Endpoints section.

### Does CCAG require a database?

Yes. CCAG requires a PostgreSQL database for virtual keys, spend tracking, settings, and user management. In production, the CDK stack creates an RDS instance automatically. For local development, use Docker: `docker run -d --name ccag-db -e POSTGRES_DB=proxy -e POSTGRES_USER=proxy -e POSTGRES_PASSWORD=devpass -p 5432:5432 postgres:16`.

### How do I deploy to a custom domain?

Edit `~/.ccag/config.json` to set `domain_name` and `hosted_zone_name`, then redeploy. The CDK stack creates a Route53 DNS record and an ACM certificate (auto-validated via DNS). If you manage certificates externally, set `certificate_arn` to your existing certificate.

---

## Authentication

### Which OIDC providers does CCAG support?

CCAG supports any OpenID Connect provider that issues RS256-signed JWTs. Tested providers include Okta, Azure AD (Microsoft Entra ID), Google Workspace, Auth0, and Keycloak. See [Authentication](authentication.md) for provider-specific setup guides.

### Can I use multiple OIDC providers simultaneously?

Yes. You can configure one provider via environment variables (`OIDC_ISSUER`) and additional providers through the admin API or portal. All configured providers are active simultaneously, and each JWT is validated against the matching issuer.

### How do virtual keys work?

Virtual keys are API keys managed by CCAG (prefixed with `sk-proxy-`). They are stored as SHA-256 hashes in the database and cached in memory for fast validation. Each key can have a name, rate limit, user assignment, and team assignment. The gateway uses its own AWS credentials for Bedrock; virtual keys only authenticate the client to CCAG.

### How do I set up SSO login for the CLI?

Use the `apiKeyHelper` mechanism in Claude Code. Add to `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://ccag.example.com",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS": "840000"
  },
  "apiKeyHelper": "bash ~/.claude/proxy-login.sh"
}
```

This opens a browser for SSO login and passes the resulting token back to Claude Code. The `proxy-login.sh` script is served by the gateway at `/auth/setup/token-script`. See [Authentication](authentication.md) for details.

### What is the ADMIN_PASSWORD used for?

`ADMIN_PASSWORD` is the bootstrap and break-glass recovery credential. It lets you log in to the admin portal even if OIDC is misconfigured or the database is corrupted. Set it to a strong value in production and store it securely. See [Configuration](configuration.md) for more.

---

## Features

### Does CCAG support streaming?

Yes. Streaming is fully supported. CCAG translates Bedrock's binary event stream into Anthropic's SSE (Server-Sent Events) format in real time:

```
Claude Code --[POST stream:true]--> CCAG --[InvokeModelWithResponseStream]--> Bedrock
Claude Code <--[SSE text/event-stream]-- CCAG <--[AWS binary event stream]--- Bedrock
```

### How does web search work through CCAG?

Bedrock does not support Anthropic's `web_search` server tool. CCAG intercepts web search tool use requests, executes the search via DuckDuckGo, and translates the results back into the `server_tool_use`/`web_search_tool_result` format that Claude Code expects. This is transparent to the client.

### Does CCAG support the count_tokens endpoint?

Yes. The `POST /v1/messages/count_tokens` endpoint is supported and proxied to Bedrock.

### What about extended thinking?

Extended thinking works through CCAG. The gateway handles the `thinking` beta flag and passes it through to Bedrock where supported.

---

## Operations

### How do I monitor CCAG?

CCAG exposes Prometheus metrics at `/metrics` and supports OTLP export. Key metrics include:

- Request counts and latency (by model, status code)
- Token usage (input/output/cache)
- Active connections
- Rate limit hits
- Error rates

Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export metrics to your observability stack (Grafana, Datadog, etc.).

### How do I view logs?

In ECS deployments, logs are sent to CloudWatch Logs. The CDK stack configures a log group automatically. Use the AWS Console or CLI:

```bash
aws logs tail /ecs/CCAG --follow
```

Locally, logs are written to stderr. Set `RUST_LOG=debug` for verbose logging including request/response bodies.

### How does CCAG scale?

CCAG is stateless (aside from the in-memory key cache, which is rebuilt on startup and synced via database polling). Scale by editing `desired_count` in `~/.ccag/config.json` and redeploying. The ALB distributes traffic across tasks.

The in-memory key cache and settings are eventually consistent across instances (5-second polling interval). Rate limiting is per-instance (in-memory sliding window), not distributed. With N instances, the effective rate limit is approximately N times the configured limit.

### How do I track spending?

CCAG tracks token usage per request and aggregates it by user, team, and model. View spend data in the admin portal's Analytics section or via the API:

```bash
curl https://ccag.example.com/admin/analytics \
  -H "authorization: Bearer $TOKEN"
```

Export spend data as CSV:

```bash
curl https://ccag.example.com/admin/analytics/export \
  -H "authorization: Bearer $TOKEN" > spend.csv
```

### How do I upgrade CCAG?

Pre-built images and binaries are published to [GitHub Releases](https://github.com/antkawam/claude-code-aws-gateway/releases) on every release. No compilation required.

- **Docker Compose:** `docker compose pull && docker compose up -d` (or pin with `CCAG_VERSION=1.0.2`)
- **CDK:** `npx cdk deploy -c environment=prod -c imageTag=1.0.2`
- **CLI:** `ccag update`

Database migrations run automatically on startup. See [Upgrading](upgrading.md) for details.

### What is the performance overhead of CCAG?

CCAG adds 1-5ms of latency for request translation. The gateway is written in Rust (axum/tokio) and processes requests asynchronously. The primary latency is Bedrock's inference time (typically hundreds of milliseconds); the gateway adds 1-5ms.

---

## Troubleshooting

### Claude Code says "authentication failed" or returns 401

Common causes:
1. **Expired token:** Regenerate via the portal or `proxy-login.sh`
2. **Wrong ANTHROPIC_BASE_URL:** Ensure it points to your CCAG instance, not `api.anthropic.com`
3. **Invalid virtual key:** Check the key is not revoked in the portal
4. **OIDC misconfiguration:** Check the issuer URL and audience match your IDP settings

### I see "Bedrock connectivity check failed" in the logs

This is a non-blocking warning at startup. It means the gateway could not reach Bedrock. Check:
1. AWS credentials are configured correctly
2. Bedrock model access is enabled in the AWS console
3. The ECS task role has `bedrock:InvokeModel*` permissions (the CDK stack configures this automatically)

### Requests return "model not found" or "access denied" from Bedrock

1. Ensure the requested Claude model is enabled in your AWS account's Bedrock console
2. Check that cross-region inference profiles are enabled (required for Claude 4.5+ models)
3. Verify the gateway's AWS region matches where you enabled the model

### The portal shows a blank page or won't load

The portal is embedded in the binary at compile time (`include_str!`). If you see a blank page:
1. Check the browser console for JavaScript errors
2. Ensure the gateway is running (check `/health`)
3. Try clearing browser cache

### Database connection fails on startup

Check:
1. `DATABASE_URL` is set correctly (or `DATABASE_HOST` + `DB_PASSWORD`)
2. The Postgres instance is running and accessible
3. Network connectivity (security groups in ECS, Docker networking locally)
4. The database exists and the user has permissions

### Rate limiting seems inconsistent across instances

Rate limiting is per-instance (in-memory sliding window). With multiple ECS tasks, the effective limit per key is approximately `configured_rpm * number_of_tasks` because requests are distributed across instances by the ALB. For strict rate limiting, set the per-key limit to `desired_limit / desired_count`.

### How do I reset the admin password?

The admin password is set via the `ADMIN_PASSWORD` environment variable. To reset it:
1. Update the ECS task definition with the new password value
2. Redeploy or force a new deployment in ECS

There is no password stored in the database. The admin password is always read from the environment variable at startup.

### How do I debug request translation issues?

Set `RUST_LOG=debug` to see full request and response bodies in the logs. This shows the original Anthropic-format request, the translated Bedrock request, and the translated response.

```bash
RUST_LOG=debug cargo run
```

In ECS, update the `RUST_LOG` environment variable in the task definition and redeploy.

---

## See Also

- [Getting Started](getting-started.md): initial setup
- [Configuration](configuration.md): all configuration options
- [Authentication](authentication.md): OIDC setup guides
- [Upgrading](upgrading.md): upgrade and rollback procedures
