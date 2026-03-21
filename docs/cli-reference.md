# CLI Reference

## Overview

The `ccag` CLI manages a running CCAG instance. It communicates with the gateway's admin API to manage keys, users, teams, endpoints, identity providers, and runtime settings. These operations can also be performed through the admin portal or API directly.

## Setup

### First-time setup

```bash
ccag login --url https://ccag.example.com
```

This prompts for admin credentials, then saves the gateway URL and session token to `~/.ccag/`. All subsequent commands use the saved URL and token automatically.

### Non-interactive login (scripts, agents, CI)

```bash
ccag login --url https://ccag.example.com -u admin -p "$ADMIN_PASSWORD"
```

### Environment variable overrides

```bash
export CCAG_URL=https://ccag.example.com
export CCAG_TOKEN=<session-token>
ccag keys list
```

### Auth resolution order

| Source | URL | Token |
|---|---|---|
| 1. CLI flag | `--url` | `--token` |
| 2. Environment variable | `CCAG_URL` | `CCAG_TOKEN` |
| 3. Saved config | `~/.ccag/config.json` | `~/.ccag/token` |

If no token is available and the terminal is interactive, the CLI prompts for login.

---

## Commands

### ccag login

Authenticate and save credentials.

```bash
ccag login --url https://ccag.example.com        # interactive (prompts for password)
ccag login --url https://ccag.example.com -u admin -p secret  # non-interactive
ccag login                                         # re-authenticate with saved URL
```

---

### ccag config

Read or update runtime settings.

```bash
ccag config list                                  # list all settings
ccag config get virtual_keys_enabled              # get a specific setting
ccag config set virtual_keys_enabled true         # update a setting
```

---

### ccag keys

Manage virtual API keys.

```bash
ccag keys create --name "dev-key"                 # create a key
ccag keys create --name "limited" --rate-limit 60 --team TEAM_ID
ccag keys list                                     # list all keys
ccag keys revoke KEY_ID                            # deactivate a key
ccag keys delete KEY_ID                            # permanently delete a key
ccag keys setup-token KEY_ID                       # generate single-use setup token
```

| Subcommand | Description |
|---|---|
| `create --name <n> [--rate-limit <rpm>] [--team <id>]` | Create a new virtual key |
| `list` | List all keys (ID, name, active, RPM, created) |
| `revoke <key_id>` | Deactivate a key (reversible) |
| `delete <key_id>` | Permanently delete a key |
| `setup-token <key_id>` | Generate a single-use setup token |

---

### ccag users

Manage users.

```bash
ccag users list                                    # list all users
ccag users create --name alice@example.com         # create a member
ccag users create --name bob@example.com --role admin --team TEAM_ID
ccag users update USER_ID --role admin             # change role
ccag users delete USER_ID                          # delete user
ccag users set-team USER_ID --team TEAM_ID         # assign to team
ccag users set-team USER_ID                        # unassign from team
ccag users set-spend-limit USER_ID --limit 100.00  # set monthly limit
ccag users set-spend-limit USER_ID                 # remove limit
```

| Subcommand | Description |
|---|---|
| `list` | List all users (ID, email, role, team, created) |
| `create --name <email> [--role <admin\|member>] [--team <id>]` | Create a user |
| `update <user_id> --role <admin\|member>` | Update user's role |
| `delete <user_id>` | Delete a user |
| `set-team <user_id> [--team <id>]` | Assign/unassign team |
| `set-spend-limit <user_id> [--limit <usd>]` | Set/remove monthly spend limit |

---

### ccag teams

Manage teams and budgets.

```bash
ccag teams list                                    # list all teams
ccag teams create --name engineering               # create a team
ccag teams delete TEAM_ID                          # delete a team
ccag teams set-budget TEAM_ID --amount 500         # set monthly budget
ccag teams set-budget TEAM_ID --amount 500 --period weekly --policy soft
ccag teams analytics TEAM_ID                       # view team spend/usage
ccag teams endpoints TEAM_ID                       # view assigned endpoints
ccag teams set-endpoints TEAM_ID --endpoint EP_ID:1 --endpoint EP_ID2:2 --strategy primary_fallback
```

| Subcommand | Description |
|---|---|
| `list` | List all teams |
| `create --name <n>` | Create a team |
| `delete <team_id>` | Delete a team |
| `set-budget <team_id> --amount <usd> [--period <daily\|weekly\|monthly>] [--policy <standard\|soft\|shaped>] [--user-budget <usd>]` | Set team budget |
| `analytics <team_id>` | View team spend and per-user breakdown |
| `endpoints <team_id>` | View assigned endpoints and routing strategy |
| `set-endpoints <team_id> --endpoint <id:priority>... [--strategy <primary_fallback\|sticky_user\|round_robin>]` | Assign endpoints to team |

---

### ccag endpoints

Manage Bedrock endpoint pool.

```bash
ccag endpoints list                                # list all endpoints with health
ccag endpoints create --name "us-west" --region us-west-2 --routing-prefix us
ccag endpoints create --name "cross-account" --region us-east-1 --routing-prefix us --role-arn arn:aws:iam::ACCOUNT:role/NAME
ccag endpoints update EP_ID --enabled false        # disable endpoint
ccag endpoints delete EP_ID                        # delete endpoint
ccag endpoints set-default EP_ID                   # set as default
ccag endpoints quotas EP_ID                        # show Bedrock RPM/TPM limits
ccag endpoints models EP_ID                        # list available models
```

| Subcommand | Description |
|---|---|
| `list` | List all endpoints (ID, name, region, health, default, enabled, priority) |
| `create --name <n> --region <r> --routing-prefix <p> [--role-arn <a>] [--external-id <e>] [--inference-profile-arn <a>] [--priority <n>]` | Create an endpoint |
| `update <id> [--name <n>] [--region <r>] [--routing-prefix <p>] [--enabled <bool>] [--priority <n>]` | Update an endpoint |
| `delete <id>` | Delete an endpoint |
| `set-default <id>` | Set as the default endpoint |
| `quotas <id>` | Show Bedrock quota limits (RPM/TPM) |
| `models <id>` | List available models on the endpoint |

---

### ccag idps

Manage OIDC identity providers.

```bash
ccag idps list                                     # list configured IDPs
ccag idps create --name "Okta" --issuer-url https://company.okta.com --audience ccag
ccag idps create --name "Google" --issuer-url https://accounts.google.com --flow-type device_code --auto-provision
ccag idps update IDP_ID --enabled false            # disable an IDP
ccag idps delete IDP_ID                            # delete an IDP
```

| Subcommand | Description |
|---|---|
| `list` | List IDPs (ID, name, issuer, flow type, enabled) |
| `create --name <n> --issuer-url <u> [--client-id <c>] [--audience <a>] [--jwks-url <j>] [--flow-type <device_code\|implicit>] [--auto-provision] [--default-role <member\|admin>] [--allowed-domains <d,d>]` | Create an IDP |
| `update <id> [same flags as create] [--enabled <bool>]` | Update an IDP |
| `delete <id>` | Delete an IDP |

---

### ccag status

Check deployment health (requires AWS credentials, not gateway auth).

```bash
ccag status --region us-west-2                     # basic status
ccag status --region us-west-2 --verbose           # include deep health check
ccag status --region us-west-2 --profile my-profile --stack-name CCAG
```

| Flag | Description |
|---|---|
| `--region` | AWS region (required) |
| `--profile` | AWS CLI profile |
| `--stack-name` | CloudFormation stack name (default: `CCAG`) |
| `--verbose` | Include deep health check response |

---

### ccag logs

Tail gateway logs from CloudWatch (requires AWS credentials, not gateway auth).

```bash
ccag logs --region us-west-2                       # tail recent logs
ccag logs --region us-west-2 --follow              # live tail
ccag logs --region us-west-2 --filter "ERROR"      # filter pattern
ccag logs --region us-west-2 --since 1h --limit 50
```

| Flag | Description |
|---|---|
| `--region` | AWS region (required) |
| `--profile` | AWS CLI profile |
| `--stack-name` | CloudFormation stack name (default: `CCAG`) |
| `--follow` / `-f` | Live tail mode |
| `--filter` | CloudWatch filter pattern |
| `--since` | Time range (e.g., `1h`, `30m`) |
| `--limit` | Maximum number of log entries |

---

## API Equivalents

Every CLI command maps to an admin API endpoint. For programmatic access (scripts, MCP tools, agents), use the API directly:

```bash
# Authenticate
TOKEN=$(curl -s -X POST https://ccag.example.com/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"..."}' | jq -r '.token')

# List keys
curl https://ccag.example.com/admin/keys -H "Authorization: Bearer $TOKEN"

# Create a team
curl -X POST https://ccag.example.com/admin/teams \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name": "engineering"}'
```

See the admin portal for interactive API exploration.

## See Also

- [Getting Started](getting-started.md) — initial setup
- [Configuration](configuration.md) — runtime settings reference
- [Authentication](authentication.md) — OIDC provider setup
- [Endpoints](endpoints.md) — Bedrock endpoint routing
- [Budgets](budgets.md) — team and user budget management
