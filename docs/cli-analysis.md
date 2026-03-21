# CLI Analysis & Roadmap

Analysis of the `ccag` management CLI — tested against the running production gateway (2026-03-21).

## Test Results

| Command | Status | Notes |
|---|---|---|
| `config list` | **Works** | Displays all runtime settings |
| `config get <key>` | **Works** | Shows value or warning for missing key |
| `keys create --name <n>` | **Works** | Returns key + Claude Code config instructions |
| `keys list` | **Broken** | Silent exit 0, no output |
| `keys revoke <id>` | **Works** | |
| `users list` | **Broken** | Silent exit 0, no output |
| `users create --name <n>` | **Broken** | "error decoding response body" |
| `status` | **Works** | Requires `--region` and `--profile` flags |
| `status --verbose` | **Works** | Shows deep health check JSON |
| `logs` | **Untested** | Hardcoded log group format may not match CDK naming |

## Bug Inventory

### Critical (commands fail silently or with errors)

**Bug 1: `keys list` — response wrapping mismatch**
- CLI: `resp.as_array()` — expects a bare JSON array
- API: returns `{"keys": [...]}`
- Result: `as_array()` returns `None`, command exits 0 with no output
- Fix: `resp["keys"].as_array()`

**Bug 2: `users list` — response wrapping mismatch**
- Same as Bug 1. API returns `{"users": [...]}`, CLI does `resp.as_array()`
- Fix: `resp["users"].as_array()`

**Bug 3: `users create` — field name mismatch**
- CLI sends `{"name": "..."}` but API expects `{"email": "..."}`
- `CreateUserRequest` has `pub email: String`, not `name`
- Result: deserialization error
- Fix: send `{"email": name}` in the request body

### High (wrong data displayed)

**Bug 4: `keys list` — missing `rate_limit_rpm` in API response**
- The `list_keys` handler omits `rate_limit_rpm` from the JSON response
- `VirtualKey` struct has the field, handler just doesn't include it
- CLI always shows `-` for the RPM column
- Fix: add `"rate_limit_rpm": k.rate_limit_rpm` to the JSON in admin.rs

**Bug 5: `users list` — displays `name` instead of `email`**
- CLI reads `user["name"]` but the `User` struct has `email`, not `name`
- Column header says "NAME" but the field is `email`
- Fix: read `user["email"]`, rename column to "EMAIL"

### Medium

**Bug 6: `status`/`logs` — `unsafe { std::env::set_var() }`**
- Uses `unsafe` blocks to mutate process environment
- Unsound in Rust 2024 edition, will become a compile error
- Fix: pass env vars via `Command::env()` builder instead

**Bug 7: `logs` — hardcoded log group name**
- Assumes `/ecs/{stack_name.to_lowercase()}` which doesn't match CDK-generated log groups
- Fix: allow `--log-group` override, or query CloudFormation for the actual name

### Low

**Bug 8: Error messages** — raw API errors shown without context (e.g., 401 doesn't suggest checking the token)

## API Coverage (after Tier 1 implementation)

| Domain | API Endpoints | CLI Commands | Coverage |
|---|---|---|---|
| **Keys** | 5 | 5 (create, list, revoke, delete, setup-token) | **100%** |
| **Users** | 6 | 6 (create, list, update, delete, set-team, set-spend-limit) | **100%** |
| **Teams** | 7 | 7 (create, list, delete, set-budget, analytics, endpoints, set-endpoints) | **100%** |
| **Endpoints** | 7 | 7 (list, create, update, delete, set-default, quotas, models) | **100%** |
| **IDPs** | 4 | 4 (create, list, update, delete) | **100%** |
| **Settings** | 2 | 3 (get, set, list) | **100%** |
| Health | 1 | 0 (status uses AWS CLI) | 0% |
| User Analytics | 3 | 0 | 0% |
| Org Analytics | 6 | 0 | 0% |
| Budget | 3 | 0 | 0% |
| Notifications | 7 | 0 | 0% |
| Search Providers | 5 | 0 | 0% |
| Bedrock Validation | 3 | 0 | 0% |
| **Total** | **61** | **33** | **~54%** |

### Tier 2 (next priorities)
- Analytics (user-scoped with `--days`, `--granularity`, CSV export)
- Org analytics (admin cross-tenant dashboard)
- Budget status
- Health status (use gateway's own endpoint)

### Tier 3
- Notifications config (webhook setup, test, activate)
- Search providers
- Bedrock validation
- Filtering/pagination on list commands

## Missing CLI Coverage (Tier 2+)

| API Area | Endpoints | CLI Status |
|---|---|---|
| Analytics | Spend, activity, models, tools | Not in CLI |
| Notifications | Budget alerts, webhook config | Not in CLI |

## Architecture Decision: CLI Boundary

### The `ccag` CLI is a management tool, not a deployment tool.

Researched 10 comparable products: Vault, Kong, Grafana, Keycloak, MinIO, Teleport, GitLab, Argo CD, LiteLLM, Backstage. Every one separates management CLI from deployment.

| Concern | Owned by |
|---|---|
| Infrastructure provisioning | CDK stack (`infra/`), Docker Compose, or user's own IaC |
| Initial config scaffolding | `setup.sh` (one-shot, writes `environments.json`) |
| Build + deploy | `deploy.sh` (reads `environments.json`, builds image, runs CDK) |
| Application management (day-2) | `ccag` CLI (config, keys, users, status, logs) |
| Bootstrap admin | Environment variables (`ADMIN_PASSWORD`), server creates on first startup |

### Why no `ccag init`

AWS Copilot tried combining deployment and management in one CLI. It was deprecated in 2025. The failure pattern:
- Too opinionated: imposed rigid structure
- Escape hatches undermined the abstraction
- Maintenance burden of both deployment AND management surfaces
- CDK/Terraform won: the ecosystem converged on general-purpose IaC

CCAG's CDK stack is a maintained reference template — users can deploy via Docker Compose, raw ECS, or their own IaC. The `ccag` binary shouldn't need to know which path they chose.

### Public deployment scripts

Two new scripts for the public repo:

**`scripts/setup.sh`** — One-shot bootstrap wizard
- Checks prerequisites (AWS CLI, Docker, Node.js, CDK, credentials, Bedrock access)
- Auto-detects AWS account/region, Route53 hosted zones
- Asks for domain, admin password, scaling, CIDR restrictions
- Writes `environments.json`
- Run once on day 1. After that, user edits `environments.json` directly.
- If `environments.json` exists, warns and exits (unless `--force`)

**`scripts/deploy.sh`** — Build + deploy orchestrator
- Reads `environments.json`
- Creates ECR repo if missing, CDK bootstraps if needed
- Builds ARM64 image, pushes to ECR
- Runs `cdk deploy`, waits for ECS rollout
- Post-deploy verification (health check, log scan)
- Supports `--fast` for code-only ECS task updates (skip CDK)

The user owns `environments.json` after `setup.sh` creates it. `deploy.sh` only reads it.
