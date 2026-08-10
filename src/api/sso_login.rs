//! Portal SSO login: authorization-code + PKCE, server-side exchange.
//!
//! The portal has no per-request server state of its own (unlike the CLI flow, which
//! already tracks a session UUID for polling) — `build_provider_info` mints one here,
//! keyed by a random `state` value, stored in `proxy_settings` (survives a callback
//! landing on a different pod than the one that issued the login_url: CCAG runs 2+
//! replicas behind a Service, requests aren't sticky).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::proxy::GatewayState;

const LOGIN_TTL_SECS: i64 = 300; // 5 minutes — one login attempt's worth of clock skew

#[derive(Serialize, Deserialize)]
struct PendingSsoLogin {
    verifier: String,
    token_endpoint: String,
    audience: String,
    redirect_uri: String,
}

fn login_key(state_id: &str) -> String {
    format!("sso_login:{}", state_id)
}

/// Store the PKCE verifier + IDP details for one in-flight portal login, keyed by `state`.
pub(super) async fn create_pending(
    pool: &sqlx::PgPool,
    state_id: &str,
    verifier: &str,
    token_endpoint: &str,
    audience: &str,
    redirect_uri: &str,
) -> anyhow::Result<()> {
    let value = serde_json::to_string(&PendingSsoLogin {
        verifier: verifier.to_string(),
        token_endpoint: token_endpoint.to_string(),
        audience: audience.to_string(),
        redirect_uri: redirect_uri.to_string(),
    })?;
    sqlx::query(
        r#"INSERT INTO proxy_settings (key, value, updated_at)
           VALUES ($1, $2, now())
           ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()"#,
    )
    .bind(login_key(state_id))
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch and delete (single-use) the pending login for `state`. `None` if missing,
/// expired, or already consumed — all treated the same by the caller (reject).
async fn take_pending(pool: &sqlx::PgPool, state_id: &str) -> Option<PendingSsoLogin> {
    let row: Option<(String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT value, updated_at FROM proxy_settings WHERE key = $1")
            .bind(login_key(state_id))
            .fetch_optional(pool)
            .await
            .ok()?;
    let (value, updated_at) = row?;

    // Always delete on read — a `state` is single-use whether it succeeds or not,
    // so a replayed callback can never redeem the same verifier twice.
    let _ = sqlx::query("DELETE FROM proxy_settings WHERE key = $1")
        .bind(login_key(state_id))
        .execute(pool)
        .await;

    if (chrono::Utc::now() - updated_at).num_seconds() > LOGIN_TTL_SECS {
        return None;
    }
    serde_json::from_str(&value).ok()
}

/// Clean up abandoned portal logins (user never completed the redirect).
pub(super) async fn cleanup_expired(pool: &sqlx::PgPool) {
    let _ = sqlx::query(
        "DELETE FROM proxy_settings WHERE key LIKE 'sso_login:%' AND updated_at < now() - interval '5 minutes'",
    )
    .execute(pool)
    .await;
}

#[derive(Deserialize)]
pub struct SsoCallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// GET /auth/sso/callback
/// Redeems the authorization code server-side (back-channel, PKCE-bound), validates
/// the resulting id_token, issues a gateway session token, and redirects the browser
/// back to the portal with THAT token — the external IDP token never reaches the browser.
pub async fn sso_callback(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<SsoCallbackParams>,
) -> Response {
    if let Some(err) = params.error {
        tracing::warn!(%err, "IDP returned an error on portal SSO callback");
        return Redirect::temporary("/portal?sso_error=1").into_response();
    }

    let (Some(code), Some(state_id)) = (params.code, params.state) else {
        return Redirect::temporary("/portal?sso_error=1").into_response();
    };

    let pool = state.db().await;
    let Some(pending) = take_pending(&pool, &state_id).await else {
        return Redirect::temporary("/portal?sso_error=1").into_response();
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
            tracing::warn!(%e, "Portal SSO code exchange failed");
            return Redirect::temporary("/portal?sso_error=1").into_response();
        }
    };

    let session_token = match state.idp_validator.validate_token(&id_token).await {
        Ok(identity) => {
            let ttl = state.session_token_ttl_hours.load(Ordering::Relaxed) as u64;
            let token = auth::session::issue(&state.session_signing_key, &identity, ttl);
            tracing::info!(sub = %identity.sub, ttl_hours = ttl, "Issued gateway session token (portal SSO)");
            token
        }
        Err(e) => {
            tracing::warn!(%e, "IDP token validation failed during portal SSO login");
            return Redirect::temporary("/portal?sso_error=1").into_response();
        }
    };

    // Fragment (not query) so the token is never sent in a Referer header or logged
    // server-side — same rationale implicit flow used, just carrying our own
    // narrowly-scoped, revocable session token instead of the external id_token.
    Redirect::temporary(&format!("/portal#session_token={session_token}")).into_response()
}
