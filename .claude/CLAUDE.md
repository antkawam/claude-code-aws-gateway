# Internal Scripts & Workflow

## Documentation

Public docs live in `docs/`, `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, and `infra/README.md`. All are committed and published to GitHub.

**Before editing any public doc**, use `/docs` or read `.claude/skills/docs/SKILL.md`. It contains the full writing standards, source-of-truth mappings (code + portal), doc inventory, and post-edit verification checklist.

Key facts that docs must match code:
- Virtual key prefix: `sk-proxy-` (not `ccag-sk-`)
- CLI commands: login, config, keys, users, teams, endpoints, idps, status, logs, update (no init/deploy/upgrade/destroy)
- Cache behavior: CCAG preserves `cache_control` (does not strip it)
- Model list: check `src/translate/models.rs` for current mappings

## Scripts (in `.claude/scripts/`)

```
.claude/scripts/dev.sh                # Local dev: Postgres + gateway (build, seed, reset)
.claude/scripts/deploy.sh             # Build Docker image → push ECR → CDK deploy (or fast ECS update)
.claude/scripts/merge.sh              # Checks → rebase → fast-forward merge → push
.claude/scripts/ops.sh                # Unified ops helper (logs, metrics, health, ecs, diff-deploy)
.claude/scripts/bastion.sh            # DB access via ECS Exec (psql, query, shell)
.claude/scripts/e2e-notifications.sh  # E2E notification testing (setup/test/verify/teardown)
.claude/scripts/headless-token.sh     # Headless OIDC JWT via AWS STS (no browser needed)
```

### Dev

```bash
.claude/scripts/dev.sh                # Start Postgres + gateway (default ports)
.claude/scripts/dev.sh --port 8081    # Override gateway port
.claude/scripts/dev.sh --build        # Force cargo build before starting
.claude/scripts/dev.sh --seed         # Seed mock data (3 teams, 7 users, 2000 spend_log rows)
.claude/scripts/dev.sh --reset        # Wipe Postgres volume and start fresh
.claude/scripts/dev.sh --bg           # Start in background, wait for healthcheck, exit
.claude/scripts/dev.sh stop           # Stop Postgres container
```

- Starts Postgres via `docker compose up -d postgres` (idempotent — skips if already running)
- Builds `ccag-server` if binary missing or `--build` passed
- `--seed` inserts diverse mock data for org analytics testing (teams, users, models, MCP tools, projects across 90 days)
- `--reset` wipes the Docker volume and recreates clean Postgres
- Gateway runs in foreground (Ctrl-C to stop); Postgres stays running in background
- `--bg` starts gateway in background, polls `/health` until ready, then exits — use this for scripted workflows and tests (no manual healthcheck needed)
- Auto-detects free port starting at 8080 if default is busy (prints chosen port on startup)
- Default login: `admin` / `admin` at `http://localhost:<port>/portal`

### Deploy

```bash
.claude/scripts/deploy.sh --staging              # Build + CDK deploy (code + infra)
.claude/scripts/deploy.sh --prod                 # Production
.claude/scripts/deploy.sh --staging --fast       # Skip CDK, direct ECS task def update (code-only)
.claude/scripts/deploy.sh --staging --skip-build # Re-deploy last built image
.claude/scripts/deploy.sh --prod --release       # Deploy + cut GitHub release (tag + push)
```

- Reads environment config from `environments.json` (not checked in — gitignored)
- Authenticates via AWS profile (credential_process in `~/.aws/config`, refreshed by isengardcli)
- Builds ARM64 image, pushes to ECR, deploys via CDK, waits for ECS rollout
- `--fast` warns if CloudFormation stack is in rollback state (IAM/infra changes won't apply)
- Post-deploy: verifies running image digest matches deployed, tails logs for ERROR entries
- **Requires main branch** — merge feature branches first via `merge.sh`, or use `--force` to override
- `environments.json` must have `stack_name` matching the existing CloudFormation stack in each account (currently `"CCAG"` for both staging and prod). A wrong or missing `stack_name` will create a duplicate stack instead of updating.

### Merge

```bash
.claude/scripts/merge.sh              # make check → rebase → merge main → push
.claude/scripts/merge.sh --skip-checks
```

- Must be on a feature branch, not main
- Works from worktrees (finds main repo and merges there directly)
- Retries push up to 3× on concurrent merge conflicts

## Workflow

1. Work on feature branch
2. `make check` (or let merge.sh run it)
3. `.claude/scripts/merge.sh` → merges to main
4. `.claude/scripts/deploy.sh --staging` → verify
5. `.claude/scripts/deploy.sh --prod --release` → production + cut GitHub release

**Rules:** Never deploy without user approval. Never run `--prod` without explicit confirmation.

**Security:** Admin password login (`ADMIN_PASSWORD`) must always be disabled in staging and production. Leaving it enabled will flag a security review. Use OIDC SSO for all admin access. The `ADMIN_PASSWORD_ENABLE=true` env var is a recovery-only mechanism — remove it immediately after use.

### Headless Auth (staging/prod)

For scripted or automated access to staging/prod admin APIs, use `headless-token.sh` instead of browser-based OIDC login:

```bash
.claude/scripts/headless-token.sh --env staging   # Get JWT for staging
.claude/scripts/headless-token.sh --env prod       # Get JWT for prod

# Use directly as Bearer token
JWT=$(.claude/scripts/headless-token.sh --env staging)
curl -H "Authorization: Bearer $JWT" https://staging.ccag.antkawa.people.aws.dev/admin/teams
```

- Uses AWS STS Outbound Identity Federation (`sts:GetWebIdentityToken`) to exchange IAM/SSO credentials for a standard OIDC JWT
- JWT is validated by CCAG's existing multi-IDP OIDC pipeline — no special code paths
- Requires outbound federation enabled on the target account and the STS issuer registered as an IDP in CCAG
- JWT has 5-minute TTL (default) — call at the start of each script run, not cached
- `sub` claim is the IAM role ARN (e.g., `arn:aws:iam::...:role/Admin`), not an email

## Operations & Investigations

### Scripts

```
.claude/scripts/ops.sh       # Unified ops helper (logs, metrics, health, ecs, diff-deploy)
.claude/scripts/bastion.sh   # DB access via ECS Exec (psql, query, shell)
```

### ops.sh

```bash
.claude/scripts/ops.sh creds   [--env staging|prod]   # Validate/refresh AWS credentials
.claude/scripts/ops.sh logs    [--env staging|prod] [--pattern <filter>] [--hours 2] [--minutes 10] [--insights <query>]
.claude/scripts/ops.sh metrics [--env staging|prod] [--namespace Bedrock|ECS|RDS|All] [--hours 48]
.claude/scripts/ops.sh auth-failures [--env staging|prod] [--hours 24]
.claude/scripts/ops.sh health  [--env staging|prod]
.claude/scripts/ops.sh ecs     [--env staging|prod]
.claude/scripts/ops.sh diff-deploy [--env staging|prod]
```

- Reads `environments.json` for account aliases, profiles, regions
- Auto-resolves CW log group name (picks most-recently-active AppLogGroup)
- All output structured as JSON for agent consumption
- Read-only operations only — no mutations
- Default environment is `prod` if `--env` not specified
- `--pattern` supports `|` for OR: `--pattern "ERROR|WARN"` runs parallel queries and merges
- `--hours` supports fractional values: `--hours 0.5` for 30 minutes
- `--minutes` can combine with `--hours`: `--hours 1 --minutes 30`
- `ecs` shows running tasks, image digests, stopped task reasons, deployment status
- `diff-deploy` compares running revision vs latest — shows image, env var, and policy differences

### bastion.sh

```bash
.claude/scripts/bastion.sh connect [--env staging|prod]      # Interactive psql via ECS Exec
.claude/scripts/bastion.sh query   [--env staging|prod] <sql> # Run query, return results
.claude/scripts/bastion.sh shell   [--env staging|prod]       # Interactive shell on container
```

- Auto-discovers ECS cluster/service/task from CloudFormation stack name
- Uses ECS Exec (`aws ecs execute-command`) — runs psql inside the container
- No bastion instance, no SSM tunnel — container already has VPC access to RDS
- Credentials fetched locally (operator machine), not from inside the container:
  - IAM auth (`rds_iam_auth: true`): generates IAM auth token via `aws rds generate-db-auth-token`
  - SM auth: reads password from Secrets Manager via stack's `DbSecretArn` output
- Base64-encodes credentials to safely pass through nested shell layers
- `connect`: interactive psql session
- `query`: runs SQL, prints results
- `shell`: interactive `/bin/sh` on the container

### Investigation Rules

- **Always use `ops.sh`** instead of raw `aws` CLI commands for logs, metrics, and health checks
- **Chain AWS commands** when raw CLI is needed — batch multiple filter-log-events calls into a single shell invocation instead of running them one at a time
- **Subagents for AWS ops** must be spawned with `mode: "bypassPermissions"` or have all commands pre-batched into a single shell call, to avoid permission prompt ping-pong
- **Include credential setup** in the same command chain when spawning agents: `eval $(isengardcli credentials <alias> --role Admin --shell sh) && aws logs ...`
- **Default to `--env prod`** unless the user specifies otherwise — most investigations are production

### Environments

| | Staging | Production |
|---|---|---|
| Account | `antkawa` (021721386746) | `antkawa+prod` (935934578497) |
| Profile | `antkawa-Admin` | `antkawa+prod-Admin` |
| Domain | `staging.ccag.antkawa.people.aws.dev` | `ccag.antkawa.people.aws.dev` |
| Region | `ap-southeast-2` | `ap-southeast-2` |

- Log groups: auto-discovered — picks the most-recently-active `AppLogGroup` (or set `log_group_name` in environments.json)
- ECS cluster/service: auto-discovered via stack name (or set `ecs_cluster`/`ecs_service` in environments.json)
- RDS: auto-discovered by instance identifier containing `ccag` or `proxy` (or set `rds_endpoint` in environments.json)

### Common Diagnostic Patterns

```bash
# Recent errors
.claude/scripts/ops.sh logs --env prod --pattern "ERROR" --hours 1

# Multiple patterns (OR)
.claude/scripts/ops.sh logs --env prod --pattern "ERROR|WARN|panic" --hours 1

# Short time window
.claude/scripts/ops.sh logs --env staging --pattern "Connected" --minutes 5

# Auth failures chain
.claude/scripts/ops.sh auth-failures --env prod --hours 4

# Bedrock throttling
.claude/scripts/ops.sh logs --env prod --pattern "ThrottlingException" --hours 6

# Full service health
.claude/scripts/ops.sh health --env prod

# ECS task inspection (images, revisions, stopped tasks)
.claude/scripts/ops.sh ecs --env staging

# Compare running vs latest task definition
.claude/scripts/ops.sh diff-deploy --env prod

# Insights: top errors by count
.claude/scripts/ops.sh logs --env prod --insights 'fields @message | filter @message like /ERROR/ | stats count(*) by @message | sort count(*) desc | limit 20'

# DB investigation
.claude/scripts/bastion.sh query --env prod 'SELECT count(*) FROM api_keys WHERE active = true'
```
