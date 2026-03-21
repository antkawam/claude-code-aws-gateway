# Getting Started

This guide walks you through deploying Claude Code AWS Gateway (CCAG) and connecting Claude Code to it.

## Prerequisites

- **AWS account** with Bedrock model access enabled (Claude models must be enabled in the Bedrock console)
- **AWS CLI v2** configured with credentials
- **Docker** installed and running

## Deployment Options

CCAG supports two deployment models:

| | Docker Compose | AWS CDK |
|---|---|---|
| **Best for** | Solo users, small teams, evaluation | Teams needing managed infrastructure |
| **Infrastructure** | Single host (Docker) | ECS Fargate + RDS + ALB |
| **Database** | Containerized Postgres | RDS Postgres (managed) |
| **Load balancing** | None (single instance) | ALB with health checks |
| **Autoscaling** | Manual | CPU/memory-based |
| **TLS** | Self-signed or bring your own | ACM certificate (auto-provisioned) |
| **Custom domain** | Manual DNS | Route53 (automatic) |
| **Prerequisites** | Docker | Docker, Node.js 18+, AWS CDK |

## Option A: Docker Compose

```bash
cd claude-code-aws-gateway
cp .env.example .env
```

Edit `.env`. At minimum, set your AWS region:

```bash
AWS_REGION=us-east-1
# AWS_PROFILE=default        # uncomment if using named profiles
ADMIN_PASSWORD=changeme       # change the default admin password
```

AWS credentials are passed through from your host via the `~/.aws` volume mount in `docker-compose.yml`. The gateway needs Bedrock access, so ensure your credentials are configured.

**Credential sources** (in order of precedence):
- Environment variables (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`) in `.env`
- `~/.aws/credentials` (static keys or `credential_process`)
- `~/.aws/config` with SSO (`aws sso login` — ensure the SSO cache at `~/.aws/sso/cache/` is readable)

**Common pitfalls:**
- File permissions: the container runs as user `proxy`, so `~/.aws` files must be world-readable (`chmod 644`)
- SSO tokens: `aws sso login` stores tokens in `~/.aws/sso/cache/`, which is included in the mount
- Named profiles: set `AWS_PROFILE=your-profile` in `.env` if not using the default profile

Start the stack:

```bash
docker compose up -d
```

The gateway is now running at `http://localhost:8080`. Open `http://localhost:8080/portal` to access the admin portal.

## Option B: AWS CDK (ECS Fargate + RDS)

See [`infra/README.md`](../infra/README.md) for the complete 9-step production deployment guide covering environment configuration, ECR setup, image builds, and CDK deploy.

This creates: VPC, ALB, ECS Fargate (ARM64/Graviton), RDS Postgres, autoscaling, CloudWatch alarms, and optional Route53/TLS.

## First Login

### Access the Admin Portal

Navigate to your gateway URL:

```
http://localhost:8080/portal            # Docker Compose
https://your-domain.com/portal          # CDK deployment
```

### Bootstrap Admin Credentials

Default credentials:

- **Username:** `admin`
- **Password:** `admin` (or whatever you set in `.env` / CDK config)

Change the admin password immediately after first login.

### Create Your First Virtual Key

1. Log in to the portal
2. Navigate to **Keys**
3. Click **Create Key**
4. Copy the generated key. It is shown only once.

Or via the API:

```bash
GATEWAY=http://localhost:8080

# Get a session token
TOKEN=$(curl -sf -X POST $GATEWAY/auth/login \
  -H "content-type: application/json" \
  -d '{"username":"admin","password":"admin"}' | grep -o '"token":"[^"]*"' | cut -d'"' -f4)

# Create a virtual key
curl -sf -X POST $GATEWAY/admin/keys \
  -H "content-type: application/json" \
  -H "authorization: Bearer $TOKEN" \
  -d '{"name":"my-key"}'
```

## Connecting Claude Code

### Environment Variables

```bash
export ANTHROPIC_BASE_URL=http://localhost:8080   # or your production URL
export ANTHROPIC_API_KEY=sk-proxy-...              # your virtual key
claude
```

### Claude Code Settings

Add to `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:8080",
    "ANTHROPIC_API_KEY": "sk-proxy-..."
  }
}
```

Set `ANTHROPIC_BASE_URL`, not `CLAUDE_CODE_USE_BEDROCK`. CCAG presents as the Anthropic Messages API, enabling extended thinking and web search.

### OIDC/SSO Auth

For browser-based SSO login (no static API keys), see [Authentication](authentication.md).

## Verifying It Works

```bash
claude -p "Say exactly: gateway test successful" --max-turns 1
```

Check the gateway logs to confirm the request was proxied to Bedrock:

```bash
docker compose logs gateway    # Docker Compose
```

## CLI Management Tool

CCAG includes a CLI (`ccag`) for managing a running gateway:

```bash
cargo build --bin ccag

# Manage keys
ccag --url http://localhost:8080 keys list
ccag --url http://localhost:8080 keys create --name "dev-key"

# Manage settings
ccag --url http://localhost:8080 config list

# Check health
ccag --url http://localhost:8080 status
```

Set `CCAG_URL` to avoid repeating the `--url` flag. See [CLI Reference](cli-reference.md) for all commands.

## Upgrading

```bash
git pull                       # Get latest code
docker compose up -d --build   # Docker Compose: rebuild and restart
# OR
npx cdk deploy                 # CDK: build + deploy (see infra/README.md)
```

Database migrations run automatically on gateway startup. See [Upgrading](upgrading.md) for details.

## Next Steps

- [Configuration](configuration.md): all configuration options
- [Authentication](authentication.md): set up OIDC SSO
- [CLI Reference](cli-reference.md): manage keys, users, settings
- [FAQ](faq.md): common questions and troubleshooting
