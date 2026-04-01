pub mod admin;
pub mod cli_auth;
mod handlers;
#[cfg(feature = "mock-bedrock")]
pub mod mock_bedrock;

use std::sync::Arc;

use axum::http::HeaderValue;
use axum::{
    Router,
    extract::State,
    http::Method,
    response::{Html, IntoResponse, Redirect},
    routing::{delete, get, post, put},
};
use subtle::ConstantTimeEq;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};

use crate::proxy::GatewayState;

static PORTAL_HTML: &str = include_str!("../../static/index.html");

pub fn router(state: Arc<GatewayState>) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/portal") }))
        .route("/health", get(handlers::health))
        .route("/health/deep", get(handlers::health_deep))
        .route("/v1/messages", post(handlers::messages))
        .route("/v1/messages/count_tokens", post(handlers::count_tokens))
        .route("/v1/models", get(handlers::list_models))
        // Admin API — Keys
        .route("/admin/keys", post(admin::create_key))
        .route("/admin/keys", get(admin::list_keys))
        .route("/admin/keys/{key_id}/revoke", post(admin::revoke_key))
        .route("/admin/keys/{key_id}", delete(admin::delete_key))
        .route(
            "/admin/keys/{key_id}/setup-token",
            post(admin::create_setup_token),
        )
        // Admin API — Teams
        .route("/admin/teams", post(admin::create_team))
        .route("/admin/teams", get(admin::list_teams))
        .route("/admin/teams/{team_id}", delete(admin::delete_team))
        .route(
            "/admin/teams/{team_id}/budget",
            put(admin::update_team_budget),
        )
        .route(
            "/admin/teams/{team_id}/analytics",
            get(admin::get_team_analytics),
        )
        // Admin API — Users
        .route("/admin/users", post(admin::create_user))
        .route("/admin/users", get(admin::list_users))
        .route("/admin/users/{user_id}", put(admin::update_user))
        .route("/admin/users/{user_id}", delete(admin::delete_user))
        .route("/admin/users/{user_id}/team", put(admin::update_user_team))
        // Analytics (user-scoped, available to all authenticated users)
        .route("/admin/analytics", get(admin::get_analytics))
        // Budget management
        .route(
            "/admin/analytics/overview",
            get(admin::get_analytics_overview),
        )
        .route("/admin/analytics/export", get(admin::export_analytics_csv))
        // Admin API — Org Analytics
        .route(
            "/admin/analytics/org/overview",
            get(admin::get_org_overview),
        )
        .route("/admin/analytics/org/spend", get(admin::get_org_spend))
        .route(
            "/admin/analytics/org/activity",
            get(admin::get_org_activity),
        )
        .route("/admin/analytics/org/models", get(admin::get_org_models))
        .route("/admin/analytics/org/tools", get(admin::get_org_tools))
        .route("/admin/analytics/org/export", get(admin::export_org_csv))
        .route("/admin/budget/status", get(admin::get_budget_status))
        // Admin API — User spend limits
        .route(
            "/admin/users/{user_id}/spend-limit",
            put(admin::update_user_spend_limit),
        )
        // Admin API — Identity Providers
        .route("/admin/idps", post(admin::create_idp))
        .route("/admin/idps", get(admin::list_idps))
        .route("/admin/idps/{idp_id}", put(admin::update_idp))
        .route("/admin/idps/{idp_id}", delete(admin::delete_idp))
        .route(
            "/admin/idps/{idp_id}/scim-tokens",
            post(admin::create_scim_token),
        )
        .route("/admin/idps/{idp_id}/scim", put(admin::update_idp_scim))
        // Admin API — Endpoints
        .route("/admin/endpoints", get(admin::list_endpoints))
        .route("/admin/endpoints", post(admin::create_endpoint))
        .route(
            "/admin/endpoints/{endpoint_id}",
            put(admin::update_endpoint),
        )
        .route(
            "/admin/endpoints/{endpoint_id}",
            delete(admin::delete_endpoint),
        )
        .route(
            "/admin/endpoints/{endpoint_id}/quotas",
            get(admin::get_endpoint_quotas),
        )
        .route(
            "/admin/endpoints/{endpoint_id}/models",
            get(admin::get_endpoint_models),
        )
        .route("/admin/bedrock/models", get(admin::get_all_models))
        .route(
            "/admin/teams/{team_id}/endpoints",
            get(admin::get_team_endpoints),
        )
        .route(
            "/admin/teams/{team_id}/endpoints",
            put(admin::set_team_endpoints),
        )
        .route(
            "/admin/endpoints/{endpoint_id}/default",
            put(admin::set_default_endpoint),
        )
        // Admin API — Bedrock
        .route("/admin/bedrock/validate", post(admin::validate_bedrock))
        .route("/admin/bedrock/quotas", get(admin::get_bedrock_quotas))
        // Admin API — Health
        .route("/admin/health/status", get(admin::get_health_status))
        // User self-service — Search Providers (multi-provider)
        .route(
            "/admin/search-providers",
            get(admin::get_search_providers).put(admin::set_search_provider),
        )
        .route(
            "/admin/search-providers/activate",
            post(admin::activate_search_provider),
        )
        .route(
            "/admin/search-providers/test",
            post(admin::test_search_provider),
        )
        .route(
            "/admin/search-providers/{provider_type}",
            delete(admin::delete_search_provider),
        )
        // Admin API — Websearch Mode
        .route(
            "/admin/websearch-mode",
            get(admin::get_websearch_mode).put(admin::set_websearch_mode),
        )
        // Admin API — Settings
        .route("/admin/settings", get(admin::get_settings))
        .route(
            "/admin/settings/default-budget",
            get(admin::get_default_budget).put(admin::update_default_budget),
        )
        .route("/admin/settings/{key}", put(admin::update_setting))
        // Admin API — Notifications
        .route(
            "/admin/notifications/config",
            get(admin::get_notification_config)
                .put(admin::save_notification_config)
                .delete(admin::delete_notification_config),
        )
        .route("/admin/notifications/test", post(admin::test_notification))
        .route(
            "/admin/notifications/draft",
            delete(admin::delete_notification_draft),
        )
        .route(
            "/admin/notifications/activate",
            post(admin::activate_notification),
        )
        .route(
            "/admin/notifications/categories",
            put(admin::update_notification_categories),
        )
        // Public API
        .route("/auth/providers", get(auth_providers))
        .route("/auth/login", post(auth_login))
        .route("/auth/me", get(handlers::auth_me))
        .route("/auth/setup", get(auth_setup))
        .route("/auth/setup/token-script", get(auth_setup))
        // CLI browser login flow
        .route("/auth/cli/login", get(cli_auth::cli_login))
        .route("/auth/cli/callback", get(cli_auth::cli_callback))
        .route("/auth/cli/complete", post(cli_auth::cli_complete))
        .route("/auth/cli/poll", get(cli_auth::cli_poll))
        // SCIM 2.0 Discovery (public, no auth required)
        .route(
            "/scim/v2/ServiceProviderConfig",
            get(crate::scim::discovery::service_provider_config),
        )
        .route(
            "/scim/v2/ResourceTypes",
            get(crate::scim::discovery::resource_types),
        )
        .route("/scim/v2/Schemas", get(crate::scim::discovery::schemas))
        // SCIM 2.0 User endpoints (authenticated)
        .route(
            "/scim/v2/Users",
            post(crate::scim::users::create_user).get(crate::scim::users::list_users),
        )
        .route(
            "/scim/v2/Users/{id}",
            get(crate::scim::users::get_user)
                .put(crate::scim::users::replace_user)
                .patch(crate::scim::users::patch_user)
                .delete(crate::scim::users::delete_user),
        )
        // SCIM 2.0 Group endpoints (authenticated)
        .route(
            "/scim/v2/Groups",
            post(crate::scim::groups::create_group).get(crate::scim::groups::list_groups),
        )
        .route(
            "/scim/v2/Groups/{id}",
            get(crate::scim::groups::get_group)
                .put(crate::scim::groups::replace_group)
                .patch(crate::scim::groups::patch_group)
                .delete(crate::scim::groups::delete_group),
        )
        // Portal & Metrics
        .route("/portal", get(portal))
        .route("/metrics", get(prometheus_metrics))
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
                    is_allowed_cors_origin(origin)
                }))
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers(AllowHeaders::mirror_request()),
        )
}

async fn portal() -> impl IntoResponse {
    Html(PORTAL_HTML)
}

async fn auth_login(
    State(state): State<Arc<GatewayState>>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    if !state.admin_login_enabled() {
        return axum::Json(serde_json::json!({
            "error": { "message": "Admin login is disabled. Use SSO to sign in." }
        }));
    }

    let username = body.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");

    // Rate limit: max 10 login attempts per 60s (global, not per-user)
    {
        let now = std::time::Instant::now();
        let mut attempts = state.login_attempts.lock().await;
        attempts.retain(|t| now.duration_since(*t).as_secs() < 60);
        if attempts.len() >= 10 {
            return axum::Json(serde_json::json!({
                "error": { "message": "Too many login attempts. Try again later." }
            }));
        }
        attempts.push(now);
    }

    // Constant-time comparison to prevent timing attacks
    let user_match = username
        .as_bytes()
        .ct_eq(state.config.admin_username.as_bytes());
    let pass_match = password
        .as_bytes()
        .ct_eq(state.config.admin_password.as_bytes());
    let len_match_u = username.len() == state.config.admin_username.len();
    let len_match_p = password.len() == state.config.admin_password.len();

    if user_match.into() && pass_match.into() && len_match_u && len_match_p {
        // Issue a gateway session token for the bootstrap admin
        let identity = crate::auth::oidc::OidcIdentity {
            sub: username.to_string(),
            email: None,
            idp_name: "Local".to_string(),
        };
        let ttl = state
            .session_token_ttl_hours
            .load(std::sync::atomic::Ordering::Relaxed) as u64;
        let token = crate::auth::session::issue(&state.session_signing_key, &identity, ttl);

        // Auto-provision as admin in DB
        if let Ok(None) = crate::db::users::get_user_by_email(&state.db().await, username).await {
            let _ = crate::db::users::create_user(&state.db().await, username, None, "admin").await;
            tracing::info!(sub = %username, "Auto-provisioned bootstrap admin user");
        }

        axum::Json(serde_json::json!({
            "token": token,
            "role": "admin",
            "type": "session",
        }))
    } else {
        axum::Json(serde_json::json!({
            "error": { "message": "Invalid username or password" }
        }))
    }
}

async fn auth_providers(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let mut providers: Vec<serde_json::Value> = Vec::new();

    // Load IDPs from DB (env IDP was seeded at startup)
    if let Ok(db_idps) = crate::db::idp::get_enabled_idps(&state.db().await).await {
        for row in &db_idps {
            let idp = crate::auth::oidc::IdpConfig::from_db_row(row);
            if let Some(info) = build_provider_info(&state.http_client, &idp, host).await {
                providers.push(info);
            }
        }
    }

    axum::Json(serde_json::json!({
        "providers": providers,
        "admin_login_enabled": state.admin_login_enabled(),
    }))
}

/// Discover the OIDC authorization endpoint and build provider info for the portal.
async fn build_provider_info(
    http_client: &reqwest::Client,
    idp: &crate::auth::oidc::IdpConfig,
    host: &str,
) -> Option<serde_json::Value> {
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        idp.issuer.trim_end_matches('/')
    );

    let authorize_endpoint = match http_client.get(&discovery_url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(doc) => match doc["authorization_endpoint"].as_str() {
                Some(ep) => ep.to_string(),
                None => {
                    tracing::error!(idp = %idp.name, "No authorization_endpoint in OIDC discovery");
                    return None;
                }
            },
            Err(e) => {
                tracing::error!(idp = %idp.name, %e, "Failed to parse OIDC discovery");
                return None;
            }
        },
        Err(e) => {
            tracing::error!(idp = %idp.name, %e, "Failed to fetch OIDC discovery");
            return None;
        }
    };

    // Build implicit flow URL: redirect back to /portal where checkSsoCallback extracts id_token
    let redirect_host = host.split(':').next().unwrap_or(host);
    let redirect_uri = format!("https://{redirect_host}/portal");
    let audience = idp.audience.as_deref().unwrap_or("");
    let nonce = format!("{}", chrono::Utc::now().timestamp());

    let separator = if authorize_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };

    let login_url = format!(
        "{authorize_endpoint}{separator}response_type=id_token&client_id={audience}&redirect_uri={redirect_uri}&nonce={nonce}&scope=openid"
    );

    Some(serde_json::json!({
        "name": idp.name,
        "flow_type": "implicit",
        "login_url": login_url,
    }))
}

// Setup script for SSO IDPs — supports user-wide (~/.claude/) or project-level (.claude/)
// The __SCOPE__ placeholder is replaced by the handler based on ?scope= query param.
static SETUP_SCRIPT_SSO: &str = r##"#!/bin/bash
set -euo pipefail

PROXY_HOST="__PROXY_HOST__"
PROXY_URL="__PROXY_URL__"
SCOPE="__SCOPE__"

if [ "${SCOPE}" = "project" ]; then
  TARGET_DIR=".claude"
  TOKEN_SCRIPT_PATH=".claude/proxy-token.sh"
  SETTINGS_FILE=".claude/settings.json"
else
  TARGET_DIR="${HOME}/.claude"
  TOKEN_SCRIPT_PATH="${HOME}/.claude/proxy-token.sh"
  SETTINGS_FILE="${HOME}/.claude/settings.json"
fi

echo "Setting up Claude Code to use proxy at ${PROXY_URL}..."

# 1. Download the token helper script
mkdir -p "${TARGET_DIR}"
curl -fsSL "${PROXY_URL}/auth/setup/token-script" -o "${TOKEN_SCRIPT_PATH}"
chmod +x "${TOKEN_SCRIPT_PATH}"
echo "  Downloaded token helper -> ${TOKEN_SCRIPT_PATH}"

# 2. Write settings (and clean up conflicting VK auth if present)
NEW_SETTINGS=$(cat <<SETTINGS_EOF
{
  "env": {
    "ANTHROPIC_BASE_URL": "__PROXY_URL__",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS": "__TTL_MS__"
  },
  "apiKeyHelper": "bash ${TOKEN_SCRIPT_PATH} __PROXY_HOST__"
}
SETTINGS_EOF
)

if command -v jq &>/dev/null; then
  if [ -f "${SETTINGS_FILE}" ]; then
    # Merge new settings, remove conflicting keys (VK auth, Bedrock direct, model overrides)
    MERGED=$(jq -s '.[0] * .[1]' "${SETTINGS_FILE}" <(echo "${NEW_SETTINGS}") \
      | jq 'del(.env.ANTHROPIC_AUTH_TOKEN)
          | del(.env.CLAUDE_CODE_USE_BEDROCK)
          | del(.env.ANTHROPIC_BEDROCK_BASE_URL)
          | del(.env.CLAUDE_CODE_SKIP_BEDROCK_AUTH)
          | del(.env.ANTHROPIC_MODEL)
          | del(.env.ANTHROPIC_DEFAULT_OPUS_MODEL)
          | del(.env.ANTHROPIC_DEFAULT_SONNET_MODEL)
          | del(.env.ANTHROPIC_DEFAULT_HAIKU_MODEL)
          | del(.env.ANTHROPIC_SMALL_FAST_MODEL)
          | del(.env.DISABLE_PROMPT_CACHING)')
    echo "${MERGED}" > "${SETTINGS_FILE}"
    echo "  Merged settings into ${SETTINGS_FILE}"
  else
    echo "${NEW_SETTINGS}" | jq . > "${SETTINGS_FILE}"
    echo "  Created ${SETTINGS_FILE}"
  fi
else
  if [ -f "${SETTINGS_FILE}" ]; then
    echo "  Warning: jq not found. Please manually merge these settings into ${SETTINGS_FILE}:"
    echo "${NEW_SETTINGS}"
  else
    echo "${NEW_SETTINGS}" > "${SETTINGS_FILE}"
    echo "  Created ${SETTINGS_FILE}"
  fi
fi

# 3. For local dev with self-signed certs, add NODE_EXTRA_CA_CERTS
#    so CC trusts our cert without disabling TLS verification globally.
CERT_CANDIDATES=(
  "${HOME}/Library/Application Support/ccag/dev-cert.pem"
  "${HOME}/.local/share/ccag/dev-cert.pem"
)
if [[ "${PROXY_HOST}" == localhost* || "${PROXY_HOST}" == 127.0.0.1* ]]; then
  for cert in "${CERT_CANDIDATES[@]}"; do
    if [ -f "$cert" ]; then
      LOCAL_SETTINGS="${TARGET_DIR}/settings.local.json"
      LOCAL_ENV_SETTINGS=$(cat <<CERT_EOF
{
  "env": {
    "NODE_EXTRA_CA_CERTS": "${cert}"
  }
}
CERT_EOF
)
      if command -v jq &>/dev/null && [ -f "$LOCAL_SETTINGS" ]; then
        MERGED=$(jq -s '.[0] * .[1]' "$LOCAL_SETTINGS" <(echo "$LOCAL_ENV_SETTINGS"))
        echo "$MERGED" > "$LOCAL_SETTINGS"
      else
        echo "$LOCAL_ENV_SETTINGS" > "$LOCAL_SETTINGS"
      fi
      echo "  Added self-signed cert trust to ${LOCAL_SETTINGS}"
      break
    fi
  done
fi

# 4. Check for conflicting env vars in the current shell
CONFLICTS=""
CONFLICT_VARS=(
  ANTHROPIC_AUTH_TOKEN "conflicts with apiKeyHelper SSO auth"
  CLAUDE_CODE_USE_BEDROCK "bypasses the gateway — must be unset"
  ANTHROPIC_BEDROCK_BASE_URL "points to a different Bedrock endpoint"
  CLAUDE_CODE_SKIP_BEDROCK_AUTH "Bedrock-direct setting — not needed with gateway"
  ANTHROPIC_MODEL "may contain a Bedrock model ID that bypasses gateway mapping"
  ANTHROPIC_DEFAULT_OPUS_MODEL "Bedrock model override — gateway handles mapping"
  ANTHROPIC_DEFAULT_SONNET_MODEL "Bedrock model override — gateway handles mapping"
  ANTHROPIC_DEFAULT_HAIKU_MODEL "Bedrock model override — gateway handles mapping"
  ANTHROPIC_SMALL_FAST_MODEL "deprecated Bedrock model override"
  DISABLE_PROMPT_CACHING "defeats gateway cache savings"
  ANTHROPIC_BASE_URL "set in your shell — may override settings.json"
)
for ((i=0; i<${#CONFLICT_VARS[@]}; i+=2)); do
  var="${CONFLICT_VARS[i]}"
  msg="${CONFLICT_VARS[i+1]}"
  if [ -n "${!var:-}" ]; then
    CONFLICTS="${CONFLICTS}\n  - ${var} is set (${msg})"
  fi
done

# Also check the other settings scope for conflicting keys
if [ "${SCOPE}" = "project" ]; then
  OTHER_SETTINGS="${HOME}/.claude/settings.json"
else
  OTHER_SETTINGS=".claude/settings.json"
fi
if command -v jq &>/dev/null && [ -f "${OTHER_SETTINGS}" ]; then
  for var in ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_USE_BEDROCK ANTHROPIC_MODEL \
             ANTHROPIC_DEFAULT_OPUS_MODEL ANTHROPIC_DEFAULT_SONNET_MODEL \
             ANTHROPIC_DEFAULT_HAIKU_MODEL ANTHROPIC_SMALL_FAST_MODEL \
             ANTHROPIC_BEDROCK_BASE_URL DISABLE_PROMPT_CACHING; do
    val=$(jq -r ".env.${var} // empty" "${OTHER_SETTINGS}" 2>/dev/null || true)
    if [ -n "${val}" ]; then
      CONFLICTS="${CONFLICTS}\n  - ${var} found in ${OTHER_SETTINGS} (must be removed)"
    fi
  done
fi

if [ -n "${CONFLICTS}" ]; then
  echo ""
  echo "WARNING: Conflicting settings detected:"
  echo -e "${CONFLICTS}"
  echo ""
  echo "  Remove these from your shell profile (.zshrc/.bashrc) and/or other settings.json files."
  echo "  Then restart your terminal before running 'claude'."
else
  echo ""
  echo "Done! Run 'claude' to get started."
fi
"##;

// Setup script for API-key-only mode (no SSO)
static SETUP_SCRIPT_APIKEY: &str = r##"#!/bin/bash
set -euo pipefail

PROXY_URL="__PROXY_URL__"
CLAUDE_DIR="${HOME}/.claude"
SETTINGS_FILE="${CLAUDE_DIR}/settings.json"

echo "Setting up Claude Code to use proxy at ${PROXY_URL}..."
echo ""
echo "This proxy uses API key authentication."
echo "Get a key from your admin or create one at ${PROXY_URL}/portal#/keys"
echo ""
echo "Add these to your shell profile (~/.zshrc or ~/.bashrc):"
echo ""
echo "  export ANTHROPIC_BASE_URL=${PROXY_URL}"
echo "  export ANTHROPIC_AUTH_TOKEN=your-api-key-here"
echo ""
echo "Then run 'claude' to get started."
"##;

// Setup script for virtual key setup via one-time token
static SETUP_SCRIPT_VK: &str = r##"#!/bin/bash
set -euo pipefail

PROXY_URL="__PROXY_URL__"
API_KEY="__API_KEY__"
SCOPE="__SCOPE__"

if [ "$SCOPE" = "project" ]; then
  CLAUDE_DIR=".claude"
  SETTINGS_FILE="${CLAUDE_DIR}/settings.json"
  echo "Setting up Claude Code (project scope) to use gateway at ${PROXY_URL}..."
else
  CLAUDE_DIR="${HOME}/.claude"
  SETTINGS_FILE="${CLAUDE_DIR}/settings.json"
  echo "Setting up Claude Code (user-wide) to use gateway at ${PROXY_URL}..."
fi

mkdir -p "${CLAUDE_DIR}"

# Merge settings into existing settings.json (or create new one)
if [ -f "${SETTINGS_FILE}" ]; then
  EXISTING=$(cat "${SETTINGS_FILE}")
else
  EXISTING="{}"
fi

# Build new settings with jq if available, otherwise write directly
# Also clean up conflicting SSO auth, Bedrock direct, and model overrides
if command -v jq &>/dev/null; then
  echo "${EXISTING}" | jq \
    --arg url "${PROXY_URL}" \
    --arg key "${API_KEY}" \
    '. + {"env": (.env // {} | . + {"ANTHROPIC_BASE_URL": $url, "ANTHROPIC_AUTH_TOKEN": $key})}
     | del(.apiKeyHelper)
     | del(.env.CLAUDE_CODE_API_KEY_HELPER_TTL_MS)
     | del(.env.CLAUDE_CODE_USE_BEDROCK)
     | del(.env.ANTHROPIC_BEDROCK_BASE_URL)
     | del(.env.CLAUDE_CODE_SKIP_BEDROCK_AUTH)
     | del(.env.ANTHROPIC_MODEL)
     | del(.env.ANTHROPIC_DEFAULT_OPUS_MODEL)
     | del(.env.ANTHROPIC_DEFAULT_SONNET_MODEL)
     | del(.env.ANTHROPIC_DEFAULT_HAIKU_MODEL)
     | del(.env.ANTHROPIC_SMALL_FAST_MODEL)
     | del(.env.DISABLE_PROMPT_CACHING)' \
    > "${SETTINGS_FILE}"
else
  cat > "${SETTINGS_FILE}" <<SETTINGS_EOF
{
  "env": {
    "ANTHROPIC_BASE_URL": "${PROXY_URL}",
    "ANTHROPIC_AUTH_TOKEN": "${API_KEY}"
  }
}
SETTINGS_EOF
fi
echo "  Configured gateway URL and API key in ${SETTINGS_FILE}"

# Check for conflicting env vars in the current shell
CONFLICTS=""
CONFLICT_VARS=(
  CLAUDE_CODE_USE_BEDROCK "bypasses the gateway — must be unset"
  ANTHROPIC_BEDROCK_BASE_URL "points to a different Bedrock endpoint"
  CLAUDE_CODE_SKIP_BEDROCK_AUTH "Bedrock-direct setting — not needed with gateway"
  ANTHROPIC_MODEL "may contain a Bedrock model ID that bypasses gateway mapping"
  ANTHROPIC_DEFAULT_OPUS_MODEL "Bedrock model override — gateway handles mapping"
  ANTHROPIC_DEFAULT_SONNET_MODEL "Bedrock model override — gateway handles mapping"
  ANTHROPIC_DEFAULT_HAIKU_MODEL "Bedrock model override — gateway handles mapping"
  ANTHROPIC_SMALL_FAST_MODEL "deprecated Bedrock model override"
  DISABLE_PROMPT_CACHING "defeats gateway cache savings"
  ANTHROPIC_BASE_URL "set in your shell — may override settings.json"
)
for ((i=0; i<${#CONFLICT_VARS[@]}; i+=2)); do
  var="${CONFLICT_VARS[i]}"
  msg="${CONFLICT_VARS[i+1]}"
  if [ -n "${!var:-}" ]; then
    CONFLICTS="${CONFLICTS}\n  - ${var} is set (${msg})"
  fi
done

# Also check the other settings scope for conflicting keys
if [ "$SCOPE" = "project" ]; then
  OTHER_SETTINGS="${HOME}/.claude/settings.json"
else
  OTHER_SETTINGS=".claude/settings.json"
fi
if command -v jq &>/dev/null && [ -f "${OTHER_SETTINGS}" ]; then
  for var in CLAUDE_CODE_USE_BEDROCK ANTHROPIC_MODEL apiKeyHelper \
             ANTHROPIC_DEFAULT_OPUS_MODEL ANTHROPIC_DEFAULT_SONNET_MODEL \
             ANTHROPIC_DEFAULT_HAIKU_MODEL ANTHROPIC_SMALL_FAST_MODEL \
             ANTHROPIC_BEDROCK_BASE_URL DISABLE_PROMPT_CACHING; do
    if [ "${var}" = "apiKeyHelper" ]; then
      val=$(jq -r ".${var} // empty" "${OTHER_SETTINGS}" 2>/dev/null || true)
    else
      val=$(jq -r ".env.${var} // empty" "${OTHER_SETTINGS}" 2>/dev/null || true)
    fi
    if [ -n "${val}" ]; then
      CONFLICTS="${CONFLICTS}\n  - ${var} found in ${OTHER_SETTINGS} (must be removed)"
    fi
  done
fi

if [ -n "${CONFLICTS}" ]; then
  echo ""
  echo "WARNING: Conflicting settings detected:"
  echo -e "${CONFLICTS}"
  echo ""
  echo "  Remove these from your shell profile (.zshrc/.bashrc) and/or other settings.json files."
  echo "  Then restart your terminal before running 'claude'."
else
  echo ""
  echo "Done! Run 'claude' to get started."
fi
"##;

static PROXY_LOGIN_SCRIPT: &str = include_str!("proxy_login.sh");
static PROXY_LOGIN_SCRIPT_PS1: &str = include_str!("proxy_login.ps1");

// PowerShell setup script for SSO IDPs — mirrors SETUP_SCRIPT_SSO
static SETUP_SCRIPT_SSO_PS1: &str = r##"$ErrorActionPreference = 'Stop'

$ProxyHost = '__PROXY_HOST__'
$ProxyUrl = '__PROXY_URL__'
$Scope = '__SCOPE__'

if ($Scope -eq 'project') {
    $TargetDir = Join-Path (Get-Location) '.claude'
    $TokenScriptPath = Join-Path $TargetDir 'proxy-token.ps1'
    $SettingsFile = Join-Path $TargetDir 'settings.json'
} else {
    $TargetDir = Join-Path $env:USERPROFILE '.claude'
    $TokenScriptPath = Join-Path $TargetDir 'proxy-token.ps1'
    $SettingsFile = Join-Path $TargetDir 'settings.json'
}

Write-Host "Setting up Claude Code to use proxy at $ProxyUrl..."

# 1. Download the token helper script
if (-not (Test-Path $TargetDir)) { New-Item -ItemType Directory -Path $TargetDir -Force | Out-Null }
Invoke-RestMethod -Uri "$ProxyUrl/auth/setup/token-script?platform=windows" -OutFile $TokenScriptPath
Write-Host "  Downloaded token helper -> $TokenScriptPath"

# 2. Write settings (and clean up conflicting VK auth if present)
$NewSettings = @{
    env = @{
        ANTHROPIC_BASE_URL = '__PROXY_URL__'
        CLAUDE_CODE_API_KEY_HELPER_TTL_MS = '__TTL_MS__'
    }
    apiKeyHelper = "powershell -ExecutionPolicy Bypass -File `"$TokenScriptPath`" __PROXY_HOST__"
}

if (Test-Path $SettingsFile) {
    $existing = Get-Content $SettingsFile -Raw | ConvertFrom-Json
    # Merge env
    if (-not $existing.env) { $existing | Add-Member -NotePropertyName env -NotePropertyValue @{} }
    $NewSettings.env.GetEnumerator() | ForEach-Object {
        $existing.env | Add-Member -NotePropertyName $_.Key -NotePropertyValue $_.Value -Force
    }
    $existing | Add-Member -NotePropertyName apiKeyHelper -NotePropertyValue $NewSettings.apiKeyHelper -Force
    # Remove conflicting keys
    @('ANTHROPIC_AUTH_TOKEN','CLAUDE_CODE_USE_BEDROCK','ANTHROPIC_BEDROCK_BASE_URL',
      'CLAUDE_CODE_SKIP_BEDROCK_AUTH','ANTHROPIC_MODEL','ANTHROPIC_DEFAULT_OPUS_MODEL',
      'ANTHROPIC_DEFAULT_SONNET_MODEL','ANTHROPIC_DEFAULT_HAIKU_MODEL',
      'ANTHROPIC_SMALL_FAST_MODEL','DISABLE_PROMPT_CACHING') | ForEach-Object {
        $existing.env.PSObject.Properties.Remove($_)
    }
    $existing | ConvertTo-Json -Depth 10 | Set-Content $SettingsFile -Encoding UTF8
    Write-Host "  Merged settings into $SettingsFile"
} else {
    $NewSettings | ConvertTo-Json -Depth 10 | Set-Content $SettingsFile -Encoding UTF8
    Write-Host "  Created $SettingsFile"
}

# 3. Check for conflicting env vars
$conflicts = @()
$conflictVars = @{
    ANTHROPIC_AUTH_TOKEN = 'conflicts with apiKeyHelper SSO auth'
    CLAUDE_CODE_USE_BEDROCK = 'bypasses the gateway - must be unset'
    ANTHROPIC_BEDROCK_BASE_URL = 'points to a different Bedrock endpoint'
    CLAUDE_CODE_SKIP_BEDROCK_AUTH = 'Bedrock-direct setting - not needed with gateway'
    ANTHROPIC_MODEL = 'may contain a Bedrock model ID that bypasses gateway mapping'
    ANTHROPIC_DEFAULT_OPUS_MODEL = 'Bedrock model override - gateway handles mapping'
    ANTHROPIC_DEFAULT_SONNET_MODEL = 'Bedrock model override - gateway handles mapping'
    ANTHROPIC_DEFAULT_HAIKU_MODEL = 'Bedrock model override - gateway handles mapping'
    ANTHROPIC_SMALL_FAST_MODEL = 'deprecated Bedrock model override'
    DISABLE_PROMPT_CACHING = 'defeats gateway cache savings'
    ANTHROPIC_BASE_URL = 'set in your shell - may override settings.json'
}
foreach ($var in $conflictVars.Keys) {
    $val = [Environment]::GetEnvironmentVariable($var)
    if ($val) { $conflicts += "  - $var is set ($($conflictVars[$var]))" }
}

# Check the other settings scope for conflicting keys
if ($Scope -eq 'project') {
    $OtherSettings = Join-Path (Join-Path $env:USERPROFILE '.claude') 'settings.json'
} else {
    $OtherSettings = Join-Path (Join-Path (Get-Location) '.claude') 'settings.json'
}
if (Test-Path $OtherSettings) {
    try {
        $other = Get-Content $OtherSettings -Raw | ConvertFrom-Json
        @('ANTHROPIC_AUTH_TOKEN','CLAUDE_CODE_USE_BEDROCK','ANTHROPIC_MODEL',
          'ANTHROPIC_DEFAULT_OPUS_MODEL','ANTHROPIC_DEFAULT_SONNET_MODEL',
          'ANTHROPIC_DEFAULT_HAIKU_MODEL','ANTHROPIC_SMALL_FAST_MODEL',
          'ANTHROPIC_BEDROCK_BASE_URL','DISABLE_PROMPT_CACHING') | ForEach-Object {
            if ($other.env.$_) { $conflicts += "  - $_ found in $OtherSettings (must be removed)" }
        }
    } catch {}
}

if ($conflicts.Count -gt 0) {
    Write-Host ''
    Write-Host 'WARNING: Conflicting settings detected:'
    $conflicts | ForEach-Object { Write-Host $_ }
    Write-Host ''
    Write-Host '  Remove these from your profile and/or other settings.json files.'
    Write-Host '  Then restart your terminal before running ''claude''.'
} else {
    Write-Host ''
    Write-Host 'Done! Run ''claude'' to get started.'
}
"##;

// PowerShell setup script for virtual key setup via one-time token — mirrors SETUP_SCRIPT_VK
static SETUP_SCRIPT_VK_PS1: &str = r##"$ErrorActionPreference = 'Stop'

$ProxyUrl = '__PROXY_URL__'
$ApiKey = '__API_KEY__'
$Scope = '__SCOPE__'

if ($Scope -eq 'project') {
    $ClaudeDir = Join-Path (Get-Location) '.claude'
    $SettingsFile = Join-Path $ClaudeDir 'settings.json'
    Write-Host "Setting up Claude Code (project scope) to use gateway at $ProxyUrl..."
} else {
    $ClaudeDir = Join-Path $env:USERPROFILE '.claude'
    $SettingsFile = Join-Path $ClaudeDir 'settings.json'
    Write-Host "Setting up Claude Code (user-wide) to use gateway at $ProxyUrl..."
}

if (-not (Test-Path $ClaudeDir)) { New-Item -ItemType Directory -Path $ClaudeDir -Force | Out-Null }

# Build new settings, merge if existing
$NewEnv = @{
    ANTHROPIC_BASE_URL = $ProxyUrl
    ANTHROPIC_AUTH_TOKEN = $ApiKey
}

if (Test-Path $SettingsFile) {
    $existing = Get-Content $SettingsFile -Raw | ConvertFrom-Json
    if (-not $existing.env) { $existing | Add-Member -NotePropertyName env -NotePropertyValue @{} }
    $NewEnv.GetEnumerator() | ForEach-Object {
        $existing.env | Add-Member -NotePropertyName $_.Key -NotePropertyValue $_.Value -Force
    }
    # Remove conflicting keys
    $existing.PSObject.Properties.Remove('apiKeyHelper')
    @('CLAUDE_CODE_API_KEY_HELPER_TTL_MS','CLAUDE_CODE_USE_BEDROCK','ANTHROPIC_BEDROCK_BASE_URL',
      'CLAUDE_CODE_SKIP_BEDROCK_AUTH','ANTHROPIC_MODEL','ANTHROPIC_DEFAULT_OPUS_MODEL',
      'ANTHROPIC_DEFAULT_SONNET_MODEL','ANTHROPIC_DEFAULT_HAIKU_MODEL',
      'ANTHROPIC_SMALL_FAST_MODEL','DISABLE_PROMPT_CACHING') | ForEach-Object {
        $existing.env.PSObject.Properties.Remove($_)
    }
    $existing | ConvertTo-Json -Depth 10 | Set-Content $SettingsFile -Encoding UTF8
} else {
    @{ env = $NewEnv } | ConvertTo-Json -Depth 10 | Set-Content $SettingsFile -Encoding UTF8
}
Write-Host "  Configured gateway URL and API key in $SettingsFile"

# Check for conflicting env vars
$conflicts = @()
$conflictVars = @{
    CLAUDE_CODE_USE_BEDROCK = 'bypasses the gateway - must be unset'
    ANTHROPIC_BEDROCK_BASE_URL = 'points to a different Bedrock endpoint'
    CLAUDE_CODE_SKIP_BEDROCK_AUTH = 'Bedrock-direct setting - not needed with gateway'
    ANTHROPIC_MODEL = 'may contain a Bedrock model ID that bypasses gateway mapping'
    ANTHROPIC_DEFAULT_OPUS_MODEL = 'Bedrock model override - gateway handles mapping'
    ANTHROPIC_DEFAULT_SONNET_MODEL = 'Bedrock model override - gateway handles mapping'
    ANTHROPIC_DEFAULT_HAIKU_MODEL = 'Bedrock model override - gateway handles mapping'
    ANTHROPIC_SMALL_FAST_MODEL = 'deprecated Bedrock model override'
    DISABLE_PROMPT_CACHING = 'defeats gateway cache savings'
    ANTHROPIC_BASE_URL = 'set in your shell - may override settings.json'
}
foreach ($var in $conflictVars.Keys) {
    $val = [Environment]::GetEnvironmentVariable($var)
    if ($val) { $conflicts += "  - $var is set ($($conflictVars[$var]))" }
}

# Check the other settings scope for conflicting keys
if ($Scope -eq 'project') {
    $OtherSettings = Join-Path (Join-Path $env:USERPROFILE '.claude') 'settings.json'
} else {
    $OtherSettings = Join-Path (Join-Path (Get-Location) '.claude') 'settings.json'
}
if (Test-Path $OtherSettings) {
    try {
        $other = Get-Content $OtherSettings -Raw | ConvertFrom-Json
        @('CLAUDE_CODE_USE_BEDROCK','ANTHROPIC_MODEL','ANTHROPIC_DEFAULT_OPUS_MODEL',
          'ANTHROPIC_DEFAULT_SONNET_MODEL','ANTHROPIC_DEFAULT_HAIKU_MODEL',
          'ANTHROPIC_SMALL_FAST_MODEL','ANTHROPIC_BEDROCK_BASE_URL',
          'DISABLE_PROMPT_CACHING') | ForEach-Object {
            if ($other.env.$_) { $conflicts += "  - $_ found in $OtherSettings (must be removed)" }
        }
        if ($other.apiKeyHelper) {
            $conflicts += "  - apiKeyHelper found in $OtherSettings (must be removed)"
        }
    } catch {}
}

if ($conflicts.Count -gt 0) {
    Write-Host ''
    Write-Host 'WARNING: Conflicting settings detected:'
    $conflicts | ForEach-Object { Write-Host $_ }
    Write-Host ''
    Write-Host '  Remove these from your profile and/or other settings.json files.'
    Write-Host '  Then restart your terminal before running ''claude''.'
} else {
    Write-Host ''
    Write-Host 'Done! Run ''claude'' to get started.'
}
"##;

/// Resolves the primary IDP. Returns (has_sso, idp_name).
async fn resolve_primary_idp(state: &GatewayState) -> Option<(bool, String)> {
    // Check DB IDPs
    if let Ok(db_idps) = crate::db::idp::get_enabled_idps(&state.db().await).await
        && let Some(row) = db_idps.first()
    {
        let idp = crate::auth::oidc::IdpConfig::from_db_row(row);
        return Some((true, idp.name));
    }
    None
}

async fn auth_setup(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let host_no_port = host.split(':').next().unwrap_or(host);
    // Detect scheme: trust X-Forwarded-Proto from reverse proxies,
    // otherwise default to http for localhost, https for everything else.
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_else(|| {
            if host_no_port == "localhost" || host_no_port == "127.0.0.1" {
                "http"
            } else {
                "https"
            }
        });
    let proxy_url = format!("{}://{}", scheme, host);

    // Parse platform query param: "windows" or default "unix"
    let is_windows = uri
        .query()
        .and_then(|q| q.split('&').find(|p| p.starts_with("platform=")))
        .and_then(|p| p.strip_prefix("platform="))
        .map(|v| v == "windows")
        .unwrap_or(false);

    // Sub-route: /auth/setup/token-script serves the raw token helper
    if uri.path() == "/auth/setup/token-script" {
        let idp = resolve_primary_idp(&state).await;
        let script = match idp {
            Some(_) => {
                if is_windows {
                    PROXY_LOGIN_SCRIPT_PS1.replace("localhost", host_no_port)
                } else {
                    PROXY_LOGIN_SCRIPT.replace("localhost", host_no_port)
                }
            }
            None => {
                if is_windows {
                    "Write-Error 'No SSO provider configured on this proxy.'; exit 1".to_string()
                } else {
                    "#!/bin/bash\necho \"ERROR: No SSO provider configured on this proxy.\" >&2\nexit 1\n".to_string()
                }
            }
        };
        return ([(axum::http::header::CONTENT_TYPE, "text/plain")], script).into_response();
    }

    // One-time token setup: /auth/setup?token=st_... — returns VK-specific setup script
    let token_param = uri
        .query()
        .and_then(|q| q.split('&').find(|p| p.starts_with("token=")))
        .and_then(|p| p.strip_prefix("token="));

    if let Some(token) = token_param {
        // Consume the token (single-use)
        let raw_key = {
            let mut store = state.setup_tokens.write().await;
            match store.remove(token) {
                Some(t) => {
                    if std::time::Instant::now()
                        .duration_since(t.created_at)
                        .as_secs()
                        < crate::proxy::SETUP_TOKEN_TTL_SECS
                    {
                        Some(t.raw_key)
                    } else {
                        None // Expired
                    }
                }
                None => None,
            }
        };

        let mut script = match raw_key {
            Some(key) => {
                let scope = uri
                    .query()
                    .and_then(|q| q.split('&').find(|p| p.starts_with("scope=")))
                    .and_then(|p| p.strip_prefix("scope="))
                    .unwrap_or("user");
                let scope = if scope == "project" {
                    "project"
                } else {
                    "user"
                };
                let template = if is_windows {
                    SETUP_SCRIPT_VK_PS1
                } else {
                    SETUP_SCRIPT_VK
                };
                template
                    .replace("__PROXY_URL__", &proxy_url)
                    .replace("__API_KEY__", &key)
                    .replace("__SCOPE__", scope)
            }
            None => {
                if is_windows {
                    "Write-Error 'Invalid or expired setup token.'; exit 1".to_string()
                } else {
                    "#!/bin/bash\necho \"ERROR: Invalid or expired setup token.\" >&2\nexit 1\n"
                        .to_string()
                }
            }
        };

        // Inject WebSearch deny for VK setup path too
        append_websearch_deny(&mut script, &state, is_windows).await;

        return ([(axum::http::header::CONTENT_TYPE, "text/plain")], script).into_response();
    }

    // Main setup script (unauthenticated — no secrets embedded)
    let scope = uri
        .query()
        .and_then(|q| q.split('&').find(|p| p.starts_with("scope=")))
        .and_then(|p| p.strip_prefix("scope="))
        .unwrap_or("user");
    let scope = if scope == "project" {
        "project"
    } else {
        "user"
    };

    // Resolve TTL from admin setting, default 24h
    let ttl_ms = crate::db::settings::get_setting(&state.db().await, "api_key_helper_ttl_ms")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "86400000".to_string());

    let idp = resolve_primary_idp(&state).await;
    let mut script = match idp {
        Some(_) => {
            // SSO setup: download token helper + merge settings
            let template = if is_windows {
                SETUP_SCRIPT_SSO_PS1
            } else {
                SETUP_SCRIPT_SSO
            };
            template
                .replace("__PROXY_HOST__", host_no_port)
                .replace("__PROXY_URL__", &proxy_url)
                .replace("__SCOPE__", scope)
                .replace("__TTL_MS__", &ttl_ms)
        }
        None => {
            // No IDP: API key instructions only
            SETUP_SCRIPT_APIKEY
                .replace("__PROXY_HOST__", host_no_port)
                .replace("__PROXY_URL__", &proxy_url)
        }
    };

    // When websearch is disabled by admin, inject permission deny into setup script
    append_websearch_deny(&mut script, &state, is_windows).await;

    ([(axum::http::header::CONTENT_TYPE, "text/plain")], script).into_response()
}

/// Append WebSearch permission denial to a setup script when websearch mode is disabled.
async fn append_websearch_deny(
    script: &mut String,
    state: &std::sync::Arc<crate::proxy::GatewayState>,
    is_windows: bool,
) {
    let websearch_mode = crate::db::settings::get_setting(&state.db().await, "websearch_mode")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "enabled".to_string());

    if websearch_mode == "disabled" {
        if is_windows {
            script.push_str(
                r#"
# Admin has disabled WebSearch - apply permission denial
$DenySettings = '{"permissions":{"deny":["WebSearch"]}}'
if (Test-Path $SettingsFile) {
    $existing = Get-Content $SettingsFile -Raw | ConvertFrom-Json
    $deny = $DenySettings | ConvertFrom-Json
    $existing | Add-Member -NotePropertyName permissions -NotePropertyValue $deny.permissions -Force
    $existing | ConvertTo-Json -Depth 10 | Set-Content $SettingsFile -Encoding UTF8
    Write-Host '  Applied WebSearch deny permission (admin-configured)'
}
"#,
            );
        } else {
            script.push_str(
                r#"
# Admin has disabled WebSearch - apply permission denial
DENY_SETTINGS='{"permissions":{"deny":["WebSearch"]}}'
if command -v jq &>/dev/null && [ -f "${SETTINGS_FILE:-}" ]; then
  MERGED=$(jq --argjson deny "$DENY_SETTINGS" '. * $deny' "${SETTINGS_FILE}")
  echo "${MERGED}" > "${SETTINGS_FILE}"
  echo "  Applied WebSearch deny permission (admin-configured)"
elif [ -n "${SETTINGS_FILE:-}" ] && [ -f "${SETTINGS_FILE}" ]; then
  echo "  NOTE: WebSearch is disabled by admin. Manually add to ${SETTINGS_FILE}:"
  echo "    $DENY_SETTINGS"
fi
"#,
            );
        }
    }
}

/// Check if an origin is allowed for CORS.
/// Permits `https://claude.ai` and `https://*.claude.ai` (e.g. `pivot.claude.ai`).
fn is_allowed_cors_origin(origin: &HeaderValue) -> bool {
    origin
        .to_str()
        .map(|s| {
            s == "https://claude.ai" || (s.ends_with(".claude.ai") && s.starts_with("https://"))
        })
        .unwrap_or(false)
}

async fn prometheus_metrics(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Require auth to prevent information disclosure
    if let Err(resp) = admin::check_admin_auth(&headers, &state).await {
        return resp;
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        state.metrics.prometheus_text(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cors_allows_claude_ai() {
        assert!(is_allowed_cors_origin(&HeaderValue::from_static(
            "https://claude.ai"
        )));
    }

    #[test]
    fn test_cors_allows_subdomain() {
        assert!(is_allowed_cors_origin(&HeaderValue::from_static(
            "https://pivot.claude.ai"
        )));
        assert!(is_allowed_cors_origin(&HeaderValue::from_static(
            "https://excel.claude.ai"
        )));
        assert!(is_allowed_cors_origin(&HeaderValue::from_static(
            "https://anything.claude.ai"
        )));
    }

    #[test]
    fn test_cors_blocks_unrelated_origins() {
        assert!(!is_allowed_cors_origin(&HeaderValue::from_static(
            "https://evil.com"
        )));
        assert!(!is_allowed_cors_origin(&HeaderValue::from_static(
            "https://example.com"
        )));
    }

    #[test]
    fn test_cors_blocks_http() {
        assert!(!is_allowed_cors_origin(&HeaderValue::from_static(
            "http://claude.ai"
        )));
        assert!(!is_allowed_cors_origin(&HeaderValue::from_static(
            "http://pivot.claude.ai"
        )));
    }

    #[test]
    fn test_cors_blocks_suffix_spoofing() {
        assert!(!is_allowed_cors_origin(&HeaderValue::from_static(
            "https://fakeclaude.ai"
        )));
        assert!(!is_allowed_cors_origin(&HeaderValue::from_static(
            "https://notclaude.ai"
        )));
        assert!(!is_allowed_cors_origin(&HeaderValue::from_static(
            "https://evil.claude.ai.attacker.com"
        )));
    }
}
