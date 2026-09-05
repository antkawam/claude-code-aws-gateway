//! Admin HTTP surface for the governance overlay.
//!
//! Every route lives under `/admin/mcpgov/*` and is admin-gated with CCAG's existing
//! [`check_admin_auth`] helper, called as the first statement of each handler exactly
//! as upstream handlers do. The whole surface is exposed as one [`routes`] router so
//! that wiring it into `src/api/mod.rs` costs a single `.merge()` line.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::api::admin::check_admin_auth;
use crate::proxy::GatewayState;

use super::{RequestContext, cache, sink, store};

/// The portal page for this feature, served as a standalone asset and injected into
/// the SPA at runtime. Keeping it out of `static/index.html` reduces the overlay's
/// footprint in that 7.5k-line file to a single `<script>` tag.
static PORTAL_EXT_JS: &str = include_str!("../../static/ext/mcp-governance.js");

fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message } }))).into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "mcpgov admin error");
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

const SERVER_STATUSES: [&str; 3] = ["pending", "approved", "denied"];
const TOOL_STATUSES: [&str; 3] = ["inherit", "approved", "denied"];

/// All governance routes. Merged into the main router before `.with_state()`.
pub fn routes() -> Router<Arc<GatewayState>> {
    Router::new()
        .route("/admin/mcpgov/summary", get(get_summary))
        .route(
            "/admin/mcpgov/servers",
            get(list_servers).post(create_server),
        )
        .route("/admin/mcpgov/servers/status", put(set_server_status))
        .route("/admin/mcpgov/servers/bulk-status", put(bulk_tool_status))
        .route("/admin/mcpgov/tools", get(list_tools))
        .route("/admin/mcpgov/tools/status", put(set_tool_status))
        .route(
            "/admin/mcpgov/policies",
            get(list_policies).put(upsert_policy),
        )
        .route(
            "/admin/mcpgov/policies/{id}",
            axum::routing::delete(delete_policy),
        )
        .route("/admin/mcpgov/simulate", post(simulate))
        .route("/admin/mcpgov/events", get(list_events))
        // Portal asset. Not admin-gated: it is static JavaScript containing no data,
        // and the SPA loads it before the user authenticates. Every API call it makes
        // is individually authorized.
        .route("/portal/ext/mcp-governance.js", get(portal_ext))
}

async fn portal_ext() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        PORTAL_EXT_JS,
    )
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

async fn get_summary(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;

    let summary = match store::summary(&pool).await {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let (dropped_tools, dropped_events) = sink::dropped_counts();

    // The globally effective policy, so the portal can show the current posture
    // without the operator having to open the policy editor.
    let global = cache::effective_policy(&RequestContext::default()).await;

    Json(json!({
        "summary": summary,
        "global_policy": global,
        "buffer_drops": { "discovery": dropped_tools, "events": dropped_events },
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Catalog: servers
// ---------------------------------------------------------------------------

async fn list_servers(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match store::load_servers(&pool).await {
        Ok(rows) => Json(json!({ "servers": rows })).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateServerRequest {
    server_name: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

async fn create_server(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<CreateServerRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let name = body.server_name.trim();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "server_name is required");
    }
    let status = body.status.as_deref().unwrap_or("approved");
    if !SERVER_STATUSES.contains(&status) {
        return err(
            StatusCode::BAD_REQUEST,
            "status must be one of: pending, approved, denied",
        );
    }

    let pool = state.db().await;
    match store::create_server(&pool, name, status, body.notes.as_deref()).await {
        Ok(row) => {
            cache::invalidate_now(&pool).await;
            tracing::info!(
                server = name,
                status,
                "mcpgov: server catalog entry upserted"
            );
            Json(json!({ "server": row })).into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(Debug, Deserialize)]
struct SetServerStatusRequest {
    server_name: String,
    status: String,
    #[serde(default)]
    notes: Option<String>,
}

async fn set_server_status(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<SetServerStatusRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    if !SERVER_STATUSES.contains(&body.status.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "status must be one of: pending, approved, denied",
        );
    }

    let pool = state.db().await;
    match store::set_server_status(
        &pool,
        &body.server_name,
        &body.status,
        body.notes.as_deref(),
    )
    .await
    {
        Ok(true) => {
            cache::invalidate_now(&pool).await;
            tracing::info!(
                server = %body.server_name,
                status = %body.status,
                "mcpgov: server status changed"
            );
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "server not found"),
        Err(e) => internal(e),
    }
}

#[derive(Debug, Deserialize)]
struct BulkToolStatusRequest {
    server_name: String,
    status: String,
}

/// Set the same status on every tool of one server — the bulk triage action.
async fn bulk_tool_status(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<BulkToolStatusRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    if !TOOL_STATUSES.contains(&body.status.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "status must be one of: inherit, approved, denied",
        );
    }

    let pool = state.db().await;
    match store::set_status_for_server_tools(&pool, &body.server_name, &body.status).await {
        Ok(n) => {
            cache::invalidate_now(&pool).await;
            tracing::info!(
                server = %body.server_name,
                status = %body.status,
                count = n,
                "mcpgov: bulk tool status change"
            );
            Json(json!({ "updated": n })).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Catalog: tools
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListToolsQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_tools(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<ListToolsQuery>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let limit = q.limit.unwrap_or(500).clamp(1, 5000);

    let pool = state.db().await;
    match store::list_tools_filtered(&pool, q.status.as_deref(), q.server.as_deref(), limit).await {
        Ok(rows) => Json(json!({ "tools": rows })).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Debug, Deserialize)]
struct SetToolStatusRequest {
    tool_name: String,
    status: String,
}

/// Tool status is set through a body rather than a path segment because tool names
/// contain `__` separators and arbitrary vendor characters that are awkward to encode.
async fn set_tool_status(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<SetToolStatusRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    if !TOOL_STATUSES.contains(&body.status.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "status must be one of: inherit, approved, denied",
        );
    }

    let pool = state.db().await;
    match store::set_tool_status(&pool, &body.tool_name, &body.status).await {
        Ok(true) => {
            cache::invalidate_now(&pool).await;
            tracing::info!(
                tool = %body.tool_name,
                status = %body.status,
                "mcpgov: tool status changed"
            );
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "tool not found"),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

async fn list_policies(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match store::load_policies(&pool).await {
        Ok(rows) => Json(json!({ "policies": rows })).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Debug, Deserialize)]
struct UpsertPolicyRequest {
    scope: String,
    #[serde(default)]
    scope_ref: Option<String>,
    mode: String,
    default_action: String,
    #[serde(default)]
    applies_to: Option<String>,
    #[serde(default)]
    allow_patterns: Vec<String>,
    #[serde(default)]
    deny_patterns: Vec<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

async fn upsert_policy(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<UpsertPolicyRequest>,
) -> Response {
    let admin = match crate::api::admin::check_admin_auth_identity(&headers, &state).await {
        Ok(sub) => sub,
        Err(resp) => return resp,
    };

    if !["global", "team", "user"].contains(&body.scope.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "scope must be one of: global, team, user",
        );
    }
    if !["observe", "warn", "enforce"].contains(&body.mode.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "mode must be one of: observe, warn, enforce",
        );
    }
    if !["allow", "deny"].contains(&body.default_action.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "default_action must be one of: allow, deny",
        );
    }
    let applies_to = body.applies_to.as_deref().unwrap_or("mcp_only");
    if !["mcp_only", "all_tools"].contains(&applies_to) {
        return err(
            StatusCode::BAD_REQUEST,
            "applies_to must be one of: mcp_only, all_tools",
        );
    }

    // Mirror the database CHECK constraint here so the caller gets a 400 with a clear
    // message instead of a 500 from a constraint violation.
    let scope_ref = match body.scope.as_str() {
        "global" => None,
        _ => {
            let r = body.scope_ref.as_deref().unwrap_or("").trim().to_string();
            if r.is_empty() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "scope_ref is required for team and user policies",
                );
            }
            if body.scope == "team" && Uuid::parse_str(&r).is_err() {
                return err(
                    StatusCode::BAD_REQUEST,
                    "scope_ref must be a team UUID for team policies",
                );
            }
            Some(r)
        }
    };

    // A pattern that is only `*` in an allowlist means "allow everything", which is
    // almost certainly not what an operator wants to type; it silently disables the
    // allowlist's exclusivity. Reject it rather than quietly accepting a no-op.
    if body.allow_patterns.iter().any(|p| p.trim() == "*") {
        return err(
            StatusCode::BAD_REQUEST,
            "an allow pattern of `*` allows everything — leave allow_patterns empty instead",
        );
    }

    let pool = state.db().await;
    match store::upsert_policy(
        &pool,
        &body.scope,
        scope_ref.as_deref(),
        &body.mode,
        &body.default_action,
        applies_to,
        &body.allow_patterns,
        &body.deny_patterns,
        body.enabled,
        Some(&admin),
    )
    .await
    {
        Ok(row) => {
            cache::invalidate_now(&pool).await;
            tracing::info!(
                scope = %body.scope,
                scope_ref = ?scope_ref,
                mode = %body.mode,
                by = %admin,
                "mcpgov: policy upserted"
            );
            Json(json!({ "policy": row })).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn delete_policy(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match store::delete_policy(&pool, id).await {
        Ok(true) => {
            cache::invalidate_now(&pool).await;
            tracing::info!(%id, "mcpgov: policy deleted");
            Json(json!({ "deleted": true })).into_response()
        }
        Ok(false) => err(
            StatusCode::NOT_FOUND,
            "policy not found, or it is the global policy (which cannot be deleted)",
        ),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Simulator
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SimulateRequest {
    #[serde(default)]
    user_identity: Option<String>,
    #[serde(default)]
    team_id: Option<Uuid>,
    /// Tool names to test. Empty means "everything in the catalog".
    #[serde(default)]
    tool_names: Vec<String>,
}

/// Answer "what would this user actually see?" without sending a request.
async fn simulate(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<SimulateRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let ctx = RequestContext {
        user_identity: body.user_identity.clone(),
        team_id: body.team_id,
        key_id: None,
        request_id: None,
    };

    let (eff, decisions) = super::simulate(&ctx, &body.tool_names).await;

    let results: Vec<_> = decisions
        .into_iter()
        .map(|(name, d)| {
            json!({
                "tool_name": name,
                "allowed": d.allowed,
                "reason": d.reason,
                "decided_by": d.decided_by.as_str(),
            })
        })
        .collect();

    let allowed = results
        .iter()
        .filter(|r| r["allowed"].as_bool().unwrap_or(false))
        .count();
    let denied = results.len() - allowed;

    Json(json!({
        "effective_policy": eff,
        "counts": { "allowed": allowed, "denied": denied },
        "results": results,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListEventsQuery {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_events(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<ListEventsQuery>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);

    let pool = state.db().await;
    match store::list_events(&pool, q.decision.as_deref(), q.user.as_deref(), limit).await {
        Ok(rows) => Json(json!({ "events": rows })).into_response(),
        Err(e) => internal(e),
    }
}
