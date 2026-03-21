# Authentication

CCAG supports a three-tier authentication system. Start with simple admin login, add virtual keys for teams, then integrate OIDC for enterprise SSO.

## Overview

### Authentication Tiers

| Tier | Method | Use Case |
|---|---|---|
| 1. Admin password | Username/password login via portal or API | Bootstrap, break-glass recovery |
| 2. Virtual keys | API keys prefixed with `sk-proxy-` | Team-level access, CI/CD, per-key rate limits |
| 3. OIDC JWT | Identity provider tokens (Okta, Azure AD, etc.) | Enterprise SSO, auto-provisioning, browser login |

All three tiers can be active simultaneously. Each request is authenticated by trying virtual key first, then OIDC JWT, then session token (from portal login).

### Token Types

| Token | Obtained From | Lifetime | Used For |
|---|---|---|---|
| Virtual key | Admin creates via portal/API | Until revoked | Claude Code `ANTHROPIC_API_KEY` |
| Session token | `POST /auth/login` (password) or portal SSO | Configurable (default 24h) | Portal access, admin API calls |
| OIDC JWT | Identity provider | Provider-dependent | Direct API auth, CLI apiKeyHelper |

## Virtual Key Management

Virtual keys are the simplest way to give users access to CCAG.

### Creating Keys

**Via the portal:**
1. Log in as admin
2. Go to Keys section
3. Click "Create Key"
4. Set a name and optional rate limit
5. Copy the key (shown only once)

**Via the API:**

```bash
# Authenticate first
TOKEN=$(curl -sf -X POST https://ccag.example.com/auth/login \
  -H "content-type: application/json" \
  -d '{"username":"admin","password":"'$ADMIN_PASSWORD'"}' | jq -r '.token')

# Create a key
curl -X POST https://ccag.example.com/admin/keys \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"name": "dev-team-key", "rate_limit_rpm": 120}'
```

Response:

```json
{
  "key": "sk-proxy-abc123...",
  "id": "uuid",
  "prefix": "sk-proxy-abc1",
  "name": "dev-team-key",
  "created_at": "2026-03-16T00:00:00Z"
}
```

### Revoking Keys

```bash
curl -X POST https://ccag.example.com/admin/keys/{key_id}/revoke \
  -H "authorization: Bearer $TOKEN"
```

Revoked keys are immediately rejected. The in-memory cache is updated on the instance that processed the revocation, and other instances pick up the change within 5 seconds.

### Assigning Keys to Users and Teams

Keys can be associated with users and teams for spend tracking and budget enforcement:

```bash
curl -X POST https://ccag.example.com/admin/keys \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"name": "alice-key", "user_id": "uuid", "team_id": "uuid"}'
```

## Generic OIDC Setup

CCAG supports any OpenID Connect provider that issues RS256-signed JWTs. The setup process is the same across providers:

1. **Create an application** in your identity provider
2. **Set the redirect URI** to `https://your-ccag-domain.com/portal` (for browser SSO)
3. **Note the issuer URL** (e.g., `https://dev-12345.okta.com`)
4. **Note the audience/client ID** for token validation
5. **Configure CCAG** with the issuer and audience (env vars or admin API)

### Environment Variable Configuration

For the primary IDP, set environment variables:

```bash
OIDC_ISSUER=https://your-idp.example.com
OIDC_AUDIENCE=your-client-id
```

### Admin API Configuration

For additional IDPs (or to avoid redeployment), use the admin API:

```bash
curl -X POST https://ccag.example.com/admin/idps \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "name": "Corporate SSO",
    "issuer_url": "https://your-idp.example.com",
    "audience": "your-client-id",
    "auto_provision": true,
    "default_role": "member",
    "allowed_domains": ["company.com"]
  }'
```

| IDP Field | Description |
|---|---|
| `name` | Display name shown on the portal login button |
| `issuer_url` | OIDC issuer URL (must serve `/.well-known/openid-configuration`) |
| `audience` | Expected `aud` claim in the JWT |
| `auto_provision` | Automatically create a user account on first login |
| `default_role` | Role for auto-provisioned users (`member` or `admin`) |
| `allowed_domains` | Restrict login to specific email domains (optional) |

## Provider-Specific Guides

### Okta

1. In the Okta Admin Console, go to **Applications > Create App Integration**
2. Select **OIDC - OpenID Connect** and **Single-Page Application**
3. Set the following:
   - **App integration name:** Claude Code Gateway
   - **Grant type:** Implicit (Hybrid). Check "Allow ID Token".
   - **Sign-in redirect URI:** `https://ccag.example.com/portal`
   - **Sign-out redirect URI:** `https://ccag.example.com/portal`
   - **Controlled access:** Assign to desired groups
4. Note the **Client ID** and your **Okta domain** (e.g., `dev-12345.okta.com`)
5. Configure CCAG:

```bash
OIDC_ISSUER=https://dev-12345.okta.com
OIDC_AUDIENCE=0oaXXXXXXXXXXXX  # Client ID from step 4
```

### Azure AD (Microsoft Entra ID)

1. In the Azure Portal, go to **Microsoft Entra ID > App registrations > New registration**
2. Set the following:
   - **Name:** Claude Code Gateway
   - **Supported account types:** Accounts in this organizational directory only
   - **Redirect URI:** Web, `https://ccag.example.com/portal`
3. Go to **Authentication** and enable **ID tokens** under Implicit grant
4. Go to **Token configuration > Add optional claim** and add `email` to the ID token
5. Note the **Application (client) ID** and **Directory (tenant) ID**
6. Configure CCAG:

```bash
OIDC_ISSUER=https://login.microsoftonline.com/{tenant-id}/v2.0
OIDC_AUDIENCE={application-client-id}
```

### Google Workspace

1. Go to the [Google Cloud Console](https://console.cloud.google.com) > **APIs & Services > Credentials**
2. Click **Create Credentials > OAuth client ID**
3. Set the following:
   - **Application type:** Web application
   - **Name:** Claude Code Gateway
   - **Authorized redirect URIs:** `https://ccag.example.com/portal`
4. Note the **Client ID**
5. Configure CCAG:

```bash
OIDC_ISSUER=https://accounts.google.com
OIDC_AUDIENCE={client-id}.apps.googleusercontent.com
```

### Auth0

1. In the Auth0 Dashboard, go to **Applications > Create Application**
2. Select **Single Page Web Applications**
3. In the **Settings** tab:
   - **Allowed Callback URLs:** `https://ccag.example.com/portal`
   - **Allowed Logout URLs:** `https://ccag.example.com/portal`
   - **Allowed Web Origins:** `https://ccag.example.com`
4. Note the **Domain** and **Client ID**
5. Configure CCAG:

```bash
OIDC_ISSUER=https://your-tenant.auth0.com/
OIDC_AUDIENCE={client-id}
```

### Keycloak

1. In the Keycloak Admin Console, select your realm
2. Go to **Clients > Create client**
3. Set the following:
   - **Client type:** OpenID Connect
   - **Client ID:** ccag
   - **Valid redirect URIs:** `https://ccag.example.com/portal`
   - **Web origins:** `https://ccag.example.com`
4. Under **Client scopes**, ensure `openid`, `email`, and `profile` are included
5. Configure CCAG:

```bash
OIDC_ISSUER=https://keycloak.example.com/realms/your-realm
OIDC_AUDIENCE=ccag
```

## CLI Auth Flow (apiKeyHelper)

For Claude Code CLI authentication without static API keys, CCAG supports browser-based login via the `apiKeyHelper` mechanism.

### How It Works

1. Claude Code invokes `proxy-login.sh` when it needs credentials
2. The script opens a browser to the CCAG login page
3. The user authenticates via SSO
4. The script receives a session token and returns it to Claude Code
5. Claude Code uses the token for subsequent API calls

The `proxy-login.sh` script is served by the gateway at `/auth/setup/token-script`. Download it with:

```bash
curl -s https://your-ccag-domain.com/auth/setup/token-script > ~/.claude/proxy-login.sh
chmod +x ~/.claude/proxy-login.sh
```

### Setup

Add to `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "https://ccag.example.com",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS": "840000"
  },
  "apiKeyHelper": "bash ~/.claude/proxy-login.sh"
}
```

The `CLAUDE_CODE_API_KEY_HELPER_TTL_MS` value (840000 = 14 minutes) controls how long Claude Code caches the token before requesting a new one. Set this slightly below your IDP's token lifetime.

### Login Endpoints

The CLI auth flow uses these gateway endpoints:

| Endpoint | Method | Description |
|---|---|---|
| `/auth/cli/login` | GET | Initiates browser-based login, returns a session code |
| `/auth/cli/callback` | GET | Browser redirect target after SSO |
| `/auth/cli/complete` | POST | Completes the login flow |
| `/auth/cli/poll` | GET | CLI polls for the completed token |

## Team and User Management

### Teams

Teams group users for spend tracking and budget enforcement.

```bash
# Create a team
curl -X POST https://ccag.example.com/admin/teams \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"name": "Engineering"}'

# List teams
curl https://ccag.example.com/admin/teams \
  -H "authorization: Bearer $TOKEN"
```

### Users

Users are created automatically when they first authenticate via OIDC (if `auto_provision` is enabled), or manually by an admin.

```bash
# Create a user
curl -X POST https://ccag.example.com/admin/users \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"subject": "jdoe", "email": "jdoe@example.com", "role": "member"}'

# Assign user to a team
curl -X PUT https://ccag.example.com/admin/users/{user_id}/team \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"team_id": "uuid"}'

# List users
curl https://ccag.example.com/admin/users \
  -H "authorization: Bearer $TOKEN"
```

### Roles

| Role | Permissions |
|---|---|
| `member` | Use the gateway (send messages), view own spend, manage own keys |
| `admin` | All member permissions plus: manage keys/users/teams/IDPs/settings, view all spend |

## Local Development with OIDC

For testing OIDC locally:

1. Start the gateway with `sudo` (needed for port 443):

```bash
sudo -E OIDC_ISSUER=https://your-idp.example.com \
  OIDC_AUDIENCE=localhost \
  DATABASE_URL=postgres://proxy:proxy@localhost:5432/proxy \
  cargo run
```

2. Open `http://127.0.0.1:8080/portal` and click "Sign in with SSO"
3. First time only: accept the self-signed certificate at `https://localhost`

## See Also

- [Configuration](configuration.md): all auth-related environment variables
- [Getting Started](getting-started.md): initial setup walkthrough
- [FAQ](faq.md): authentication troubleshooting
