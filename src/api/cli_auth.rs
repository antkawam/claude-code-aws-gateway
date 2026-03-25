use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::auth;
use crate::proxy::GatewayState;

const SESSION_TTL_SECS: u64 = 300; // 5 minutes

pub struct CliSession {
    pub token: Option<String>,
    pub created_at: Instant,
}

pub type CliSessionStore = RwLock<HashMap<String, CliSession>>;

pub fn new_session_store() -> CliSessionStore {
    RwLock::new(HashMap::new())
}

// ── DB-backed session helpers (using proxy_settings as KV store) ────────

fn session_key(session_id: &str) -> String {
    format!("cli_session:{}", session_id)
}

/// Create a pending session in the DB. Value "" = pending, non-empty = token.
async fn db_create_session(pool: &sqlx::PgPool, session_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO proxy_settings (key, value, updated_at)
           VALUES ($1, '', now())
           ON CONFLICT (key) DO UPDATE SET value = '', updated_at = now()"#,
    )
    .bind(session_key(session_id))
    .execute(pool)
    .await?;
    Ok(())
}

/// Store the completed token against a session.
async fn db_complete_session(
    pool: &sqlx::PgPool,
    session_id: &str,
    token: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"UPDATE proxy_settings SET value = $2, updated_at = now()
           WHERE key = $1 AND updated_at > now() - interval '5 minutes'"#,
    )
    .bind(session_key(session_id))
    .bind(token)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Poll for a completed session. Returns:
/// - Ok(Some(token)) if complete
/// - Ok(None) if pending
/// - Err if expired/not found
async fn db_poll_session(pool: &sqlx::PgPool, session_id: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT value, updated_at FROM proxy_settings WHERE key = $1")
            .bind(session_key(session_id))
            .fetch_optional(pool)
            .await?;

    match row {
        Some((value, updated_at)) => {
            let age = chrono::Utc::now() - updated_at;
            if age.num_seconds() > SESSION_TTL_SECS as i64 {
                // Expired — clean up
                db_delete_session(pool, session_id).await.ok();
                anyhow::bail!("expired")
            }
            if value.is_empty() {
                Ok(None) // pending
            } else {
                // Complete — clean up and return token
                let token = value;
                db_delete_session(pool, session_id).await.ok();
                Ok(Some(token))
            }
        }
        None => anyhow::bail!("not_found"),
    }
}

/// Clean up a session.
async fn db_delete_session(pool: &sqlx::PgPool, session_id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM proxy_settings WHERE key = $1")
        .bind(session_key(session_id))
        .execute(pool)
        .await?;
    Ok(())
}

/// Clean up expired CLI sessions from the DB (called periodically or on create).
async fn db_cleanup_expired(pool: &sqlx::PgPool) {
    let _ = sqlx::query(
        "DELETE FROM proxy_settings WHERE key LIKE 'cli_session:%' AND updated_at < now() - interval '5 minutes'",
    )
    .execute(pool)
    .await;
}

// ─────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginParams {
    session: String,
}

/// GET /auth/cli/login?session=UUID
/// Creates a pending session and redirects to the first configured IDP.
pub async fn cli_login(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<LoginParams>,
) -> Response {
    let session_id = params.session;

    // Validate session ID is a UUID to prevent abuse
    if uuid::Uuid::parse_str(&session_id).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid session ID (expected UUID)",
        )
            .into_response();
    }

    // Store pending session in DB
    let pool = state.db().await;
    let pool = &pool;
    db_cleanup_expired(pool).await;
    if let Err(e) = db_create_session(pool, &session_id).await {
        tracing::error!(%e, "Failed to create CLI session in DB");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create session",
        )
            .into_response();
    }

    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");

    // Find the first configured IDP and build redirect URL
    let redirect_url = build_idp_redirect_url(&state, host, &session_id).await;

    match redirect_url {
        Some(url) => Redirect::temporary(&url).into_response(),
        None => (StatusCode::NOT_FOUND, "No identity provider configured").into_response(),
    }
}

/// Build the IDP authorization URL with redirect back to /auth/cli/callback.
async fn build_idp_redirect_url(
    state: &GatewayState,
    host: &str,
    session_id: &str,
) -> Option<String> {
    // Check DB IDPs
    if let Ok(db_idps) = crate::db::idp::get_enabled_idps(&state.db().await).await
        && let Some(row) = db_idps.first()
    {
        let idp = crate::auth::oidc::IdpConfig::from_db_row(row);
        return build_auth_url(&state.http_client, &idp, host, session_id).await;
    }
    // Fall back to env IDP
    if let Some(env_idp) = crate::auth::oidc::IdpConfig::from_env() {
        return build_auth_url(&state.http_client, &env_idp, host, session_id).await;
    }
    None
}

/// Discover the OIDC authorization endpoint and build the redirect URL.
async fn build_auth_url(
    http_client: &reqwest::Client,
    idp: &crate::auth::oidc::IdpConfig,
    host: &str,
    session_id: &str,
) -> Option<String> {
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        idp.issuer.trim_end_matches('/')
    );

    let authorize_endpoint = match http_client.get(&discovery_url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(doc) => match doc["authorization_endpoint"].as_str() {
                Some(ep) => ep.to_string(),
                None => {
                    tracing::error!(idp = %idp.name, "No authorization_endpoint in OIDC discovery document");
                    return None;
                }
            },
            Err(e) => {
                tracing::error!(idp = %idp.name, %e, "Failed to parse OIDC discovery document");
                return None;
            }
        },
        Err(e) => {
            tracing::error!(idp = %idp.name, %e, "Failed to fetch OIDC discovery document");
            return None;
        }
    };

    let redirect_host = host.split(':').next().unwrap_or(host);
    // If the IDP has an audience configured that looks like a domain name,
    // use it as the trusted redirect host to prevent Host header injection.
    // Skip if audience is a UUID/client_id (e.g. Entra uses UUIDs as client_ids).
    let trusted_host = idp
        .audience
        .as_deref()
        .filter(|a| !a.is_empty() && a.contains('.'))
        .unwrap_or(redirect_host);
    let redirect_uri = format!("https://{}/auth/cli/callback", trusted_host);
    let audience = idp.audience.as_deref().unwrap_or("");

    let separator = if authorize_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };

    // Cryptographically random nonce
    let nonce = format!("{:032x}", rand::random::<u128>());

    Some(format!(
        "{authorize_endpoint}{separator}response_type=id_token&client_id={audience}&redirect_uri={redirect_uri}&state={session_id}&nonce={nonce}&scope=openid%20email%20profile"
    ))
}

/// GET /auth/cli/callback
/// Serves an HTML page that extracts the token from URL fragment/query params
/// and POSTs it back to /auth/cli/complete.
pub async fn cli_callback() -> impl IntoResponse {
    Html(CLI_CALLBACK_HTML)
}

#[derive(Deserialize)]
pub struct CompleteBody {
    session: String,
    token: String,
}

/// POST /auth/cli/complete
/// Receives the IDP token from the callback page, validates it, issues a
/// gateway session token, and stores it against the session.
pub async fn cli_complete(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CompleteBody>,
) -> Response {
    if body.token.is_empty() || body.token.len() > 16384 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Invalid token"})),
        )
            .into_response();
    }

    // Validate the IDP token and issue a longer-lived gateway session token
    let token_to_store = match state.idp_validator.validate_token(&body.token).await {
        Ok(identity) => {
            let ttl = state.session_token_ttl_hours.load(Ordering::Relaxed) as u64;
            let session_token = auth::session::issue(&state.session_signing_key, &identity, ttl);
            tracing::info!(sub = %identity.sub, ttl_hours = ttl, "Issued gateway session token");
            session_token
        }
        Err(e) => {
            tracing::warn!(%e, "IDP token validation failed during CLI login, storing raw token");
            body.token.clone()
        }
    };

    // Complete session in DB
    match db_complete_session(&state.db().await, &body.session, &token_to_store).await {
        Ok(true) => Json(serde_json::json!({"status": "ok"})).into_response(),
        Ok(false) => (
            StatusCode::GONE,
            Json(serde_json::json!({"error": "Session expired or not found"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(%e, "Failed to complete CLI session in DB");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct PollParams {
    session: String,
}

/// GET /auth/cli/poll?session=UUID
/// Returns the token if the browser flow has completed.
pub async fn cli_poll(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<PollParams>,
) -> Response {
    // Poll session from DB
    match db_poll_session(&state.db().await, &params.session).await {
        Ok(Some(token)) => {
            Json(serde_json::json!({"status": "complete", "token": token})).into_response()
        }
        Ok(None) => Json(serde_json::json!({"status": "pending"})).into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("expired") {
                Json(serde_json::json!({"status": "expired"})).into_response()
            } else {
                Json(serde_json::json!({"status": "not_found"})).into_response()
            }
        }
    }
}

static CLI_CALLBACK_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Claude Code AWS Gateway</title>
<link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='6' fill='%230c0c0f'/%3E%3Ctext x='4' y='22' font-family='ui-monospace,SFMono-Regular,monospace' font-weight='700' font-size='18' fill='%23d4883a'%3E%3E_%3C/text%3E%3C/svg%3E">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;600;700&family=IBM+Plex+Sans:wght@400;500&display=swap" rel="stylesheet">
<style>
  :root {
    --bg: #0c0c0f; --bg-card: #18181d; --border: #1e1e25;
    --text: #e8e6e3; --text-secondary: #9b978f; --text-muted: #5e5b55;
    --accent: #d4883a; --green: #3ecf71; --red: #e5534b;
    --font-mono: 'IBM Plex Mono', 'SF Mono', monospace;
    --font-sans: 'IBM Plex Sans', -apple-system, sans-serif;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: var(--font-sans); background: var(--bg); color: var(--text);
    display: flex; justify-content: center; align-items: center;
    min-height: 100vh; -webkit-font-smoothing: antialiased;
  }
  .card {
    background: var(--bg-card); border: 1px solid var(--border);
    border-radius: 10px; padding: 40px; text-align: center;
    max-width: 400px; width: 90%;
  }
  .brand { font-family: var(--font-mono); font-size: 28px; color: var(--accent); font-weight: 700; letter-spacing: -0.04em; margin-bottom: 8px; }
  .title { font-family: var(--font-mono); font-size: 13px; font-weight: 600; color: var(--text); margin-bottom: 24px; }
  .title .aws { color: var(--accent); }
  .spinner { border: 2px solid var(--border); border-top: 2px solid var(--accent); border-radius: 50%; width: 20px; height: 20px; animation: spin 0.8s linear infinite; margin: 0 auto 12px; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .msg { font-size: 13px; line-height: 1.6; color: var(--text-secondary); }
  .msg strong { color: var(--text); font-weight: 500; }
  .success-icon { font-size: 32px; margin-bottom: 12px; }
  .hint { font-family: var(--font-mono); font-size: 11px; color: var(--text-muted); margin-top: 16px; padding-top: 16px; border-top: 1px solid var(--border); }
  .status-success .msg { color: var(--green); }
  .status-error .msg { color: var(--red); }
</style>
</head>
<body>
<div class="card">
  <div class="brand">&gt;_</div>
  <div class="title">Claude Code <span class="aws">AWS</span> Gateway</div>
  <div id="status">
    <div class="spinner"></div>
    <p class="msg">Completing authentication...</p>
  </div>
</div>
<script>
(function() {
  var params = {};
  [window.location.hash.substring(1), window.location.search.substring(1)].forEach(function(str) {
    str.split('&').forEach(function(pair) {
      var kv = pair.split('=');
      if (kv.length === 2) params[kv[0]] = decodeURIComponent(kv[1]);
    });
  });

  var token = params.id_token;
  var state = params.state;
  var el = document.getElementById('status');

  if (!token || !state) {
    el.className = 'status-error';
    el.innerHTML = '<p class="msg">Authentication failed — missing token or session.</p><p class="hint">Close this tab and try again from your terminal.</p>';
    return;
  }

  fetch('/auth/cli/complete', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({session: state, token: token})
  }).then(function(resp) {
    if (resp.ok) {
      el.className = 'status-success';
      var shortSession = state.substring(0, 8);
      el.innerHTML = '<div class="success-icon">&#10003;</div><p class="msg"><strong>Authenticated</strong></p><p class="hint">Session ' + shortSession + ' &#183; You can close this tab and return to your terminal.</p>';
    } else {
      return resp.json().then(function(data) {
        el.className = 'status-error';
        el.innerHTML = '<p class="msg">' + (data.error || 'Unknown error') + '</p><p class="hint">Close this tab and try again from your terminal.</p>';
      });
    }
  }).catch(function(err) {
    el.className = 'status-error';
    el.innerHTML = '<p class="msg">Connection failed: ' + err.message + '</p><p class="hint">Close this tab and try again from your terminal.</p>';
  });
})();
</script>
</body>
</html>
"##;
