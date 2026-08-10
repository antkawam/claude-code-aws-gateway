use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};
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

/// Everything the callback needs to redeem the code, once the browser comes back.
/// Stored server-side only (proxy_settings), keyed by session — never sent to the browser.
#[derive(Serialize, Deserialize)]
struct PendingCliLogin {
    verifier: String,
    token_endpoint: String,
    audience: String,
    redirect_uri: String,
}

/// A CLI login session's state: either still waiting on the browser round trip
/// (holding the PKCE verifier + IDP details needed to redeem the code), or done
/// (holding the gateway session token the CLI will poll for). Untagged: the two
/// shapes don't overlap ("verifier" vs "token"), so serde picks the right one.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum CliSessionState {
    Pending(PendingCliLogin),
    Complete { token: String },
}

fn session_key(session_id: &str) -> String {
    format!("cli_session:{}", session_id)
}

/// Create a pending session in the DB, holding the PKCE verifier + IDP details
/// needed to redeem the authorization code when the browser comes back.
async fn db_create_session(
    pool: &sqlx::PgPool,
    session_id: &str,
    pending: PendingCliLogin,
) -> anyhow::Result<()> {
    let value = serde_json::to_string(&CliSessionState::Pending(pending))?;
    sqlx::query(
        r#"INSERT INTO proxy_settings (key, value, updated_at)
           VALUES ($1, $2, now())
           ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()"#,
    )
    .bind(session_key(session_id))
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch the pending login's stored verifier/IDP details (does not consume it —
/// the callback overwrites the row with the final token right after redeeming it).
async fn db_get_pending(
    pool: &sqlx::PgPool,
    session_id: &str,
) -> anyhow::Result<Option<PendingCliLogin>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM proxy_settings WHERE key = $1")
        .bind(session_key(session_id))
        .fetch_optional(pool)
        .await?;
    match row {
        Some((value,)) => match serde_json::from_str::<CliSessionState>(&value) {
            Ok(CliSessionState::Pending(p)) => Ok(Some(p)),
            _ => Ok(None), // already completed (or never existed in this shape)
        },
        None => Ok(None),
    }
}

/// Store the completed gateway session token against a session.
async fn db_complete_session(
    pool: &sqlx::PgPool,
    session_id: &str,
    token: &str,
) -> anyhow::Result<bool> {
    let value = serde_json::to_string(&CliSessionState::Complete {
        token: token.to_string(),
    })?;
    let result = sqlx::query(
        r#"UPDATE proxy_settings SET value = $2, updated_at = now()
           WHERE key = $1 AND updated_at > now() - interval '5 minutes'"#,
    )
    .bind(session_key(session_id))
    .bind(value)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Poll for a completed session. Returns:
/// - Ok(Some(token)) if complete
/// - Ok(None) if pending (still waiting on the browser round trip)
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
            match serde_json::from_str::<CliSessionState>(&value) {
                Ok(CliSessionState::Complete { token }) => {
                    db_delete_session(pool, session_id).await.ok();
                    Ok(Some(token))
                }
                _ => Ok(None), // pending
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
/// Creates a pending session (holding a fresh PKCE verifier) and redirects to
/// the first configured IDP's authorization endpoint.
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

    let pool = state.db().await;
    let pool = &pool;
    db_cleanup_expired(pool).await;

    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");

    let idp = match resolve_idp(&state).await {
        Some(idp) => idp,
        None => {
            return (StatusCode::NOT_FOUND, "No identity provider configured").into_response();
        }
    };

    match build_auth_redirect(&state.http_client, &idp, host, &session_id).await {
        Some((redirect_url, pending)) => {
            if let Err(e) = db_create_session(pool, &session_id, pending).await {
                tracing::error!(%e, "Failed to create CLI session in DB");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create session",
                )
                    .into_response();
            }
            Redirect::temporary(&redirect_url).into_response()
        }
        None => (StatusCode::BAD_GATEWAY, "Failed to reach identity provider").into_response(),
    }
}

/// Resolve the IDP to use: first enabled DB-configured IDP, falling back to env config.
async fn resolve_idp(state: &GatewayState) -> Option<crate::auth::oidc::IdpConfig> {
    if let Ok(db_idps) = crate::db::idp::get_enabled_idps(&state.db().await).await
        && let Some(row) = db_idps.first()
    {
        return Some(crate::auth::oidc::IdpConfig::from_db_row(row));
    }
    crate::auth::oidc::IdpConfig::from_env()
}

/// Discover the IDP's endpoints and build the authorization-code + PKCE redirect URL.
/// Returns the redirect URL alongside what the callback will need to redeem the code.
async fn build_auth_redirect(
    http_client: &reqwest::Client,
    idp: &crate::auth::oidc::IdpConfig,
    host: &str,
    session_id: &str,
) -> Option<(String, PendingCliLogin)> {
    let endpoints = auth::oidc::discover_endpoints(http_client, &idp.issuer).await?;

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
    let audience = idp.audience.clone().unwrap_or_default();

    let scopes = crate::auth::oidc::resolve_oidc_scopes(idp);
    let encoded_scopes = scopes.replace(' ', "%20");

    if idp.flow_type == "authorization_code" {
        let pkce = auth::pkce::generate();
        let separator = if endpoints.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let redirect_url = format!(
            "{authz}{separator}response_type=code&client_id={audience}&redirect_uri={redirect_uri}&state={session_id}&scope={encoded_scopes}&code_challenge={challenge}&code_challenge_method=S256",
            authz = endpoints.authorization_endpoint,
            challenge = pkce.challenge,
        );
        Some((
            redirect_url,
            PendingCliLogin {
                verifier: pkce.verifier,
                token_endpoint: endpoints.token_endpoint,
                audience,
                redirect_uri,
            },
        ))
    } else {
        // Legacy implicit flow, kept for IDPs configured to use it.
        let separator = if endpoints.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let nonce = format!("{:032x}", rand::random::<u128>());
        let redirect_url = format!(
            "{authz}{separator}response_type=id_token&client_id={audience}&redirect_uri={redirect_uri}&state={session_id}&nonce={nonce}&scope={encoded_scopes}",
            authz = endpoints.authorization_endpoint,
        );
        // No verifier/token_endpoint needed for implicit — the legacy callback path
        // (client-side extraction + POST to /auth/cli/complete) handles this case.
        Some((
            redirect_url,
            PendingCliLogin {
                verifier: String::new(),
                token_endpoint: String::new(),
                audience,
                redirect_uri,
            },
        ))
    }
}

#[derive(Deserialize)]
pub struct CliCallbackParams {
    /// Present on the authorization-code path.
    code: Option<String>,
    /// Session ID, echoed back by the IDP as `state`.
    state: Option<String>,
    /// Present if the user denied consent or the IDP otherwise failed the request.
    error: Option<String>,
    error_description: Option<String>,
}

/// GET /auth/cli/callback
/// Authorization-code path: redeems the code server-side (back-channel, PKCE-bound),
/// validates the resulting id_token, issues a gateway session token, and completes
/// the session — all without the browser ever seeing the IDP's token. Renders a
/// static result page; the CLI itself learns the outcome via /auth/cli/poll.
///
/// Implicit-flow IDPs still land here with the token in the URL fragment (which the
/// server never sees) — CLI_CALLBACK_LEGACY_HTML below is served in that case and
/// does the old client-side extract + POST to /auth/cli/complete.
pub async fn cli_callback(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<CliCallbackParams>,
) -> Response {
    let Some(session_id) = params.state else {
        return Html(legacy_callback_html()).into_response();
    };

    if let Some(err) = params.error {
        let desc = params.error_description.unwrap_or_default();
        tracing::warn!(%err, %desc, "IDP returned an error on CLI callback");
        return Html(render_result_html(
            false,
            "Authentication was cancelled or denied.",
        ))
        .into_response();
    }

    let Some(code) = params.code else {
        // No code and no error — this is the implicit-flow redirect shape
        // (token lives in the URL fragment, which never reaches the server).
        return Html(legacy_callback_html()).into_response();
    };

    let pool = state.db().await;
    let pending = match db_get_pending(&pool, &session_id).await {
        Ok(Some(p)) if !p.verifier.is_empty() => p,
        _ => {
            return Html(render_result_html(
                false,
                "Session expired or not found — close this tab and try again from your terminal.",
            ))
            .into_response();
        }
    };

    let id_token = match auth::oidc::exchange_code_for_id_token(
        &state.http_client,
        &pending.token_endpoint,
        &code,
        &pending.redirect_uri,
        &pending.audience,
        &pending.verifier,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%e, "CLI code exchange failed");
            return Html(render_result_html(false, "Authentication failed.")).into_response();
        }
    };

    let session_token = match state.idp_validator.validate_token(&id_token).await {
        Ok(identity) => {
            let ttl = state.session_token_ttl_hours.load(Ordering::Relaxed) as u64;
            let token = auth::session::issue(&state.session_signing_key, &identity, ttl);
            tracing::info!(sub = %identity.sub, ttl_hours = ttl, "Issued gateway session token");
            token
        }
        Err(e) => {
            tracing::warn!(%e, "IDP token validation failed during CLI login");
            return Html(render_result_html(false, "Authentication failed.")).into_response();
        }
    };

    match db_complete_session(&pool, &session_id, &session_token).await {
        Ok(true) => Html(render_result_html(true, "")).into_response(),
        Ok(false) => Html(render_result_html(
            false,
            "Session expired — close this tab and try again from your terminal.",
        ))
        .into_response(),
        Err(e) => {
            tracing::error!(%e, "Failed to complete CLI session in DB");
            Html(render_result_html(false, "Internal error.")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CompleteBody {
    session: String,
    token: String,
}

/// POST /auth/cli/complete
/// Legacy (implicit-flow) path only: receives the IDP token from the client-side
/// callback page, validates it, issues a gateway session token, and stores it
/// against the session. The authorization-code path never calls this — it's
/// handled entirely server-side in `cli_callback`.
pub async fn cli_complete(
    State(state): State<Arc<GatewayState>>,
    axum::Json(body): axum::Json<CompleteBody>,
) -> Response {
    if body.token.is_empty() || body.token.len() > 16384 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "Invalid token"})),
        )
            .into_response();
    }

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

    match db_complete_session(&state.db().await, &body.session, &token_to_store).await {
        Ok(true) => axum::Json(serde_json::json!({"status": "ok"})).into_response(),
        Ok(false) => (
            StatusCode::GONE,
            axum::Json(serde_json::json!({"error": "Session expired or not found"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(%e, "Failed to complete CLI session in DB");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "Internal error"})),
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
    match db_poll_session(&state.db().await, &params.session).await {
        Ok(Some(token)) => {
            axum::Json(serde_json::json!({"status": "complete", "token": token})).into_response()
        }
        Ok(None) => axum::Json(serde_json::json!({"status": "pending"})).into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("expired") {
                axum::Json(serde_json::json!({"status": "expired"})).into_response()
            } else {
                axum::Json(serde_json::json!({"status": "not_found"})).into_response()
            }
        }
    }
}

fn render_result_html(success: bool, message: &str) -> String {
    let (class, body) = if success {
        (
            "status-success",
            r#"<div class="success-icon">&#10003;</div><p class="msg"><strong>Authenticated</strong></p><p class="hint">You can close this tab and return to your terminal.</p>"#
                .to_string(),
        )
    } else {
        (
            "status-error",
            format!(
                r#"<p class="msg">{}</p><p class="hint">Close this tab and try again from your terminal.</p>"#,
                askama_escape::escape(message)
            ),
        )
    };
    CLI_CALLBACK_RESULT_TEMPLATE
        .replace("__CARD_STYLE__", CARD_STYLE)
        .replace("__CLASS__", class)
        .replace("__BODY__", &body)
}

fn legacy_callback_html() -> String {
    CLI_CALLBACK_LEGACY_HTML_TEMPLATE.replace("__CARD_STYLE__", CARD_STYLE)
}

/// Minimal HTML/CSS-escaping shim — the only untrusted input is a short, server-chosen
/// error message (never user/IDP-controlled free text), but escape defensively anyway.
mod askama_escape {
    pub fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
}

const CARD_STYLE: &str = r##"<style>
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
</style>"##;

/// Server-rendered result page for the authorization-code path — no client-side
/// JS at all, since the exchange already happened server-side before this renders.
const CLI_CALLBACK_RESULT_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Claude Code AWS Gateway</title>
<link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='6' fill='%230c0c0f'/%3E%3Ctext x='4' y='22' font-family='ui-monospace,SFMono-Regular,monospace' font-weight='700' font-size='18' fill='%23d4883a'%3E%3E_%3C/text%3E%3C/svg%3E">
__CARD_STYLE__
</head>
<body>
<div class="card">
  <div class="brand">&gt;_</div>
  <div class="title">Claude Code <span class="aws">AWS</span> Gateway</div>
  <div id="status" class="__CLASS__">
    __BODY__
  </div>
</div>
</body>
</html>
"##;

/// Legacy client-side callback page for implicit-flow IDPs: extracts the id_token
/// from the URL fragment/query and POSTs it to /auth/cli/complete for validation.
const CLI_CALLBACK_LEGACY_HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Claude Code AWS Gateway</title>
<link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='6' fill='%230c0c0f'/%3E%3Ctext x='4' y='22' font-family='ui-monospace,SFMono-Regular,monospace' font-weight='700' font-size='18' fill='%23d4883a'%3E%3E_%3C/text%3E%3C/svg%3E">
__CARD_STYLE__
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

#[cfg(test)]
mod tests {
    use super::*;

    // The pending/complete discrimination is the one thing that MUST never be
    // ambiguous: a poll landing while a login is still pending must never read
    // the stored verifier back as if it were a completed session token.

    #[test]
    fn pending_state_round_trips_and_is_not_mistaken_for_complete() {
        let pending = CliSessionState::Pending(PendingCliLogin {
            verifier: "verifier-abc".to_string(),
            token_endpoint: "https://idp.example.com/token".to_string(),
            audience: "ccag".to_string(),
            redirect_uri: "https://ccag.example.com/auth/cli/callback".to_string(),
        });
        let json = serde_json::to_string(&pending).unwrap();
        match serde_json::from_str::<CliSessionState>(&json).unwrap() {
            CliSessionState::Pending(p) => assert_eq!(p.verifier, "verifier-abc"),
            CliSessionState::Complete { .. } => {
                panic!("pending state must never deserialize as complete")
            }
        }
    }

    #[test]
    fn complete_state_round_trips_and_is_not_mistaken_for_pending() {
        let complete = CliSessionState::Complete {
            token: "session-token-xyz".to_string(),
        };
        let json = serde_json::to_string(&complete).unwrap();
        match serde_json::from_str::<CliSessionState>(&json).unwrap() {
            CliSessionState::Complete { token } => assert_eq!(token, "session-token-xyz"),
            CliSessionState::Pending(_) => {
                panic!("complete state must never deserialize as pending")
            }
        }
    }

    #[test]
    fn legacy_empty_verifier_pending_is_still_pending_not_complete() {
        // The implicit-flow path stores a PendingCliLogin with an empty verifier
        // as its "session created, waiting on the browser" marker (mirroring the
        // old empty-string sentinel). It must still discriminate as Pending, not
        // be misread as a completed session carrying an empty token.
        let pending = CliSessionState::Pending(PendingCliLogin {
            verifier: String::new(),
            token_endpoint: String::new(),
            audience: "ccag".to_string(),
            redirect_uri: "https://ccag.example.com/auth/cli/callback".to_string(),
        });
        let json = serde_json::to_string(&pending).unwrap();
        match serde_json::from_str::<CliSessionState>(&json).unwrap() {
            CliSessionState::Pending(_) => {}
            CliSessionState::Complete { .. } => {
                panic!("empty-verifier pending must not be mistaken for complete")
            }
        }
    }
}
