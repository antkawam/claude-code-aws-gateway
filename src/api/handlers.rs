use std::sync::Arc;
use std::time::Instant;

use aws_sdk_bedrockruntime::primitives::Blob;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures::stream::StreamExt;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::CachedKey;
use crate::auth::oidc::OidcIdentity;
use crate::db::spend::RequestLogEntry;
use crate::proxy::GatewayState;
use crate::telemetry::Metrics;
use crate::translate::{models, request, response, streaming};
use crate::websearch;

/// RAII guard that decrements the in-flight request counter on drop.
struct InFlightGuard {
    metrics: Arc<Metrics>,
}

impl InFlightGuard {
    fn new(metrics: Arc<Metrics>) -> Self {
        metrics.in_flight_requests.add(1, &[]);
        Self { metrics }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.in_flight_requests.add(-1, &[]);
    }
}

/// Auth result: either a virtual key or an authenticated identity.
enum AuthResult {
    VirtualKey(CachedKey),
    Oidc(OidcIdentity),
}

/// Identity extracted from auth for spend tracking.
#[derive(Clone)]
struct AuthIdentity {
    key_id: Option<Uuid>,
    user_identity: Option<String>,
    user_id: Option<Uuid>,
}

/// Metadata extracted from the incoming request.
#[derive(Clone)]
struct RequestInfo {
    tool_count: i16,
    tool_names: Vec<String>,
    turn_count: i16,
    thinking_enabled: bool,
    has_system_prompt: bool,
    // Session tracking fields
    session_id: Option<String>,
    project_key: Option<String>,
    tool_errors: Option<Value>,
    has_correction: bool,
    content_block_types: Vec<String>,
    system_prompt_hash: Option<String>,
    // Detection flags (rule-based pattern detection)
    detection_flags: Option<Value>,
}

/// GET /v1/models — List available Claude models (Anthropic API format).
/// Required by Claude for Excel/PowerPoint add-ins.
pub async fn list_models() -> Response {
    let models = [
        ("claude-opus-4-6-20250605", "Claude Opus 4.6"),
        ("claude-sonnet-4-6-20250514", "Claude Sonnet 4.6"),
        ("claude-opus-4-5-20251101", "Claude Opus 4.5"),
        ("claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
        ("claude-sonnet-4-20250514", "Claude Sonnet 4"),
        ("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ];

    let data: Vec<serde_json::Value> = models
        .iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "display_name": name,
                "type": "model",
                "created_at": "2025-01-01T00:00:00Z",
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "data": data,
            "has_more": false,
            "first_id": data.first().map(|m| m["id"].as_str().unwrap_or("")),
            "last_id": data.last().map(|m| m["id"].as_str().unwrap_or("")),
        })),
    )
        .into_response()
}

pub async fn health(State(state): State<Arc<GatewayState>>) -> Response {
    let mut status = "ok";
    let mut db_ok = true;

    if let Err(e) = sqlx::query("SELECT 1").execute(&state.db().await).await {
        tracing::error!(%e, "Health check: database unreachable");
        status = "degraded";
        db_ok = false;
    }

    // ALB still gets 200 to avoid flapping; use /health/deep for strict check
    let _ = db_ok;
    let code = StatusCode::OK;
    (
        code,
        Json(serde_json::json!({ "status": status, "db": db_ok })),
    )
        .into_response()
}

/// Deep health check — returns 503 if any dependency is unhealthy.
/// Not used by ALB (which needs stability), but useful for debugging.
pub async fn health_deep(State(state): State<Arc<GatewayState>>) -> Response {
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    match sqlx::query("SELECT 1").execute(&state.db().await).await {
        Ok(_) => {
            checks.insert("db".into(), serde_json::json!("ok"));
        }
        Err(e) => {
            checks.insert("db".into(), serde_json::json!(format!("error: {e}")));
            all_ok = false;
        }
    }

    // Bedrock health check (cached 30s)
    let bedrock_ok = {
        let cached = state.bedrock_health.read().await;
        if let Some((ts, healthy)) = cached.as_ref() {
            if ts.elapsed().as_secs() < 30 {
                Some(*healthy)
            } else {
                None
            }
        } else {
            None
        }
    };

    let bedrock_ok = match bedrock_ok {
        Some(ok) => ok,
        None => {
            // Perform fresh check
            let ok = state
                .bedrock_control_client
                .list_inference_profiles()
                .max_results(1)
                .send()
                .await
                .is_ok();
            let mut cached = state.bedrock_health.write().await;
            *cached = Some((std::time::Instant::now(), ok));
            ok
        }
    };

    if bedrock_ok {
        checks.insert("bedrock".into(), serde_json::json!("ok"));
    } else {
        checks.insert("bedrock".into(), serde_json::json!("unhealthy"));
        all_ok = false;
    }

    checks.insert(
        "key_cache_size".into(),
        serde_json::json!(state.key_cache.len().await),
    );

    let code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(serde_json::json!({ "status": if all_ok { "ok" } else { "unhealthy" }, "checks": checks }))).into_response()
}

/// Returns the authenticated identity. Used by the portal to verify SSO tokens.
pub async fn auth_me(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    match check_auth(&headers, &state).await {
        Ok(AuthResult::Oidc(identity)) => {
            let role = match resolve_oidc_role(&state, &identity).await {
                Ok(r) => r,
                Err(msg) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": { "message": msg } })),
                    )
                        .into_response();
                }
            };
            // Look up user DB id for portal features (e.g. filtering keys)
            let user_id =
                crate::db::users::get_user_by_email(&state.db().await, identity.user_id())
                    .await
                    .ok()
                    .flatten()
                    .map(|u| u.id);
            Json(json!({
                "type": "oidc",
                "sub": identity.user_id(),
                "idp": identity.idp_name,
                "role": role,
                "user_id": user_id,
            }))
            .into_response()
        }
        Ok(AuthResult::VirtualKey(key)) => Json(json!({
            "type": "virtual_key",
            "name": key.name,
            "user_id": key.user_id,
            "team_id": key.team_id,
            "role": "member",
        }))
        .into_response(),
        Err(resp) => resp,
    }
}

/// Resolve the role for an authenticated user. Auto-provisions if not in DB.
/// Admin users (bootstrap admin + ADMIN_USERS) are seeded to DB at startup,
/// so this just checks the DB role. New SSO users are auto-provisioned as "member".
///
/// Returns `Err(message)` when the user should be denied access (inactive account,
/// or not yet provisioned when SCIM is enabled for the IDP).
pub async fn resolve_oidc_role(
    state: &GatewayState,
    identity: &OidcIdentity,
) -> Result<String, String> {
    let pool = state.db().await;
    let pool = &pool;

    // Check if user exists in DB (keyed by email, falling back to sub)
    if let Ok(Some(user)) = crate::db::users::get_user_by_email(pool, identity.user_id()).await {
        if !user.active {
            return Err("Your account has been deactivated".to_string());
        }
        return Ok(user.role);
    }

    // Check if the user's IDP has SCIM enabled — if so, reject auto-provisioning
    if let Ok(idps) = crate::db::idp::get_enabled_idps(pool).await {
        for idp in &idps {
            if idp.name == identity.idp_name && idp.scim_enabled {
                return Err("User not provisioned. Contact your administrator.".to_string());
            }
        }
    }

    // Auto-provision new user as member
    match crate::db::users::create_user(pool, identity.user_id(), None, "member").await {
        Ok(user) => {
            tracing::info!(sub = %identity.sub, user_id = %identity.user_id(), role = %user.role, "Auto-provisioned OIDC user");
            Ok(user.role)
        }
        Err(e) => {
            tracing::warn!(%e, sub = %identity.sub, user_id = %identity.user_id(), "Failed to auto-provision user");
            Ok("member".to_string())
        }
    }
}

#[cfg_attr(feature = "mock-bedrock", allow(unreachable_code))]
pub async fn messages(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<request::AnthropicRequest>,
) -> Response {
    let start = Instant::now();
    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let _in_flight = InFlightGuard::new(state.metrics.clone());

    // Auth check
    let auth_result = match check_auth(&headers, &state).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Extract identity for spend tracking
    let identity = match &auth_result {
        AuthResult::VirtualKey(k) => AuthIdentity {
            key_id: Some(k.id),
            user_identity: k.name.clone(),
            user_id: k.user_id,
        },
        AuthResult::Oidc(id) => {
            let user_id = crate::db::users::get_user_by_email(&state.db().await, id.user_id())
                .await
                .ok()
                .flatten()
                .map(|u| u.id);
            AuthIdentity {
                key_id: None,
                user_identity: Some(id.user_id().to_string()),
                user_id,
            }
        }
    };

    // Budget check for OIDC users (replaces old spend limit check)
    let budget_status = if let AuthResult::Oidc(ref oidc) = auth_result {
        match check_budget(&state, oidc.user_id()).await {
            Ok(status) => status,
            Err(resp) => return resp,
        }
    } else {
        None
    };

    // Budget shaping: apply RPM throttle via rate limiter with synthetic key
    if let Some(ref bs) = budget_status
        && let Some(rpm) = bs.shaped_rpm
    {
        // Use a deterministic UUID from user identity for the shaping bucket
        let shape_key = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!(
                "budget-shape:{}",
                identity.user_identity.as_deref().unwrap_or("")
            )
            .as_bytes(),
        );
        if let Err(retry_after) = state.rate_limiter.check(shape_key, rpm).await {
            return rate_limit_response(retry_after);
        }
    }

    // Rate limiting (only for virtual keys with a rate limit set)
    if let AuthResult::VirtualKey(ref key) = auth_result
        && let Some(rpm) = key.rate_limit_rpm
    {
        match state.rate_limiter.check(key.id, rpm as u32).await {
            Ok(remaining) => {
                tracing::debug!(request_id = %request_id, remaining, "Rate limit check passed");
                let _ = remaining;
            }
            Err(retry_after) => {
                tracing::warn!(
                    request_id = %request_id,
                    key_id = %key.id,
                    retry_after,
                    "Rate limited"
                );
                state.metrics.record_rate_limit();
                return rate_limit_response(retry_after);
            }
        }
    }

    // Extract request metadata before translation
    let req_info = extract_request_info(&body);

    let is_streaming = body.stream.unwrap_or(false);
    let original_model = body.model.clone();

    let beta_header = headers.get("anthropic-beta").and_then(|v| v.to_str().ok());

    // Resolve endpoint for this request
    let team_id = match &auth_result {
        AuthResult::VirtualKey(k) => k.team_id,
        AuthResult::Oidc(_) => None, // TODO: resolve team from user's DB record
    };
    let (team_endpoints, routing_strategy) = if let Some(tid) = team_id {
        let pool = state.db().await;
        let pool = &pool;
        let endpoints = crate::db::endpoints::get_team_endpoints(pool, tid)
            .await
            .unwrap_or_default();
        let strategy = crate::db::teams::get_team(pool, tid)
            .await
            .ok()
            .flatten()
            .map(|t| t.routing_strategy)
            .unwrap_or_else(|| "sticky_user".to_string());
        (endpoints, strategy)
    } else {
        (Vec::new(), "sticky_user".to_string())
    };

    let user_identity_str = identity.user_identity.as_deref();
    let selected_endpoint = state
        .endpoint_pool
        .select_endpoint(&team_endpoints, user_identity_str, &routing_strategy)
        .await;

    // Determine routing prefix: use endpoint's if available, otherwise gateway default
    let routing_prefix = selected_endpoint
        .as_ref()
        .map(|ep| ep.config.routing_prefix.clone())
        .unwrap_or_else(|| state.config.bedrock_routing_prefix.clone());

    // Read websearch mode from admin settings (disabled / enabled / global)
    let websearch_mode = crate::db::settings::get_setting(&state.db().await, "websearch_mode")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "enabled".to_string());

    let (mut bedrock_model, bedrock_body, web_search_ctx) = request::translate(
        body,
        beta_header,
        &routing_prefix,
        Some(&state.model_cache),
        &websearch_mode,
    );

    // If the endpoint has an application inference profile ARN configured, use it directly
    // as the model ID — overrides the prefix-based cross-region inference profile.
    if let Some(profile_arn) = selected_endpoint
        .as_ref()
        .and_then(|ep| ep.config.inference_profile_arn.as_deref())
    {
        bedrock_model = profile_arn.to_string();
    } else {
        // Discovery-on-miss: if the model wasn't resolved (no dot = no prefix match),
        // try discovering it via ListInferenceProfiles.
        let control_client = selected_endpoint
            .as_ref()
            .map(|ep| &ep.control_client)
            .unwrap_or(&state.bedrock_control_client);

        if !bedrock_model.contains('.')
            && let Some((prefix, suffix, display)) =
                models::discover_model(control_client, &bedrock_model, &routing_prefix).await
        {
            bedrock_model = format!("{}.{}", routing_prefix, &suffix);
            let mapping = models::CachedMapping {
                anthropic_prefix: prefix.clone(),
                bedrock_suffix: suffix.clone(),
                anthropic_display: display.clone(),
            };
            state.model_cache.insert(mapping).await;
            // Persist to DB
            if let Err(e) = crate::db::model_mappings::upsert_mapping(
                &state.db().await,
                &prefix,
                &suffix,
                display.as_deref(),
            )
            .await
            {
                tracing::warn!(%e, "Failed to persist discovered model mapping");
            }
        }
    }

    if web_search_ctx.is_some() {
        tracing::info!(request_id = %request_id, "Web search tool detected, interception enabled");
    }

    let body_bytes = match serde_json::to_vec(&bedrock_body) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(%e, "Failed to serialize Bedrock request");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                &e.to_string(),
            );
        }
    };

    // Select the runtime client: endpoint pool or fallback to gateway default
    #[cfg_attr(feature = "mock-bedrock", allow(unused_variables))]
    let runtime_client = selected_endpoint
        .as_ref()
        .map(|ep| ep.runtime_client.clone())
        .unwrap_or_else(|| state.bedrock_client.clone());

    let endpoint_id = selected_endpoint.as_ref().map(|ep| ep.config.id);

    // Record per-endpoint stats
    if let Some(ep_id) = endpoint_id {
        state.endpoint_stats.record_request(ep_id).await;
    }

    tracing::info!(
        request_id = %request_id,
        bedrock_model = %bedrock_model,
        streaming = is_streaming,
        tools = req_info.tool_count,
        turns = req_info.turn_count,
        endpoint = ?endpoint_id,
        auth_latency_us = start.elapsed().as_micros() as u64,
        "Forwarding request to Bedrock"
    );

    if tracing::enabled!(tracing::Level::DEBUG)
        && let Ok(s) = std::str::from_utf8(&body_bytes)
    {
        tracing::debug!(body = %s, "Bedrock request body");
    }

    // Mock Bedrock: return canned responses with realistic latency for load testing.
    // Feature-gated at compile time — zero overhead in production builds.
    #[cfg(feature = "mock-bedrock")]
    #[allow(unreachable_code, unused_variables)]
    {
        tracing::info!(request_id = %request_id, "Using mock Bedrock response (load testing mode)");
        let (input_tokens, output_tokens) = crate::api::mock_bedrock::mock_token_counts(None);

        // Record spend (just like real requests)
        let entry = crate::db::spend::RequestLogEntry {
            key_id: identity.key_id,
            user_identity: identity.user_identity.clone(),
            request_id: request_id.clone(),
            model: original_model.clone(),
            streaming: is_streaming,
            duration_ms: 0, // will be overridden below
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            stop_reason: Some("end_turn".to_string()),
            tool_count: req_info.tool_count,
            tool_names: req_info.tool_names.clone(),
            turn_count: req_info.turn_count,
            thinking_enabled: req_info.thinking_enabled,
            has_system_prompt: req_info.has_system_prompt,
            session_id: req_info.session_id.clone(),
            project_key: req_info.project_key.clone(),
            tool_errors: req_info.tool_errors.clone(),
            has_correction: req_info.has_correction,
            content_block_types: vec!["text".to_string()],
            system_prompt_hash: req_info.system_prompt_hash.clone(),
            detection_flags: None,
            endpoint_id,
        };

        let mock_resp = if is_streaming {
            crate::api::mock_bedrock::mock_streaming_response(&original_model, &request_id)
        } else {
            crate::api::mock_bedrock::mock_non_streaming_response(&original_model, &request_id)
        };

        // Record spend after response (spawn so we don't block)
        let tracker = state.spend_tracker.clone();
        let mut final_entry = entry;
        final_entry.duration_ms = start.elapsed().as_millis() as i32;
        tokio::spawn(async move { tracker.record(final_entry).await });

        state.metrics.record_request(
            &original_model,
            is_streaming,
            start.elapsed().as_millis() as f64,
            "ok",
        );

        return mock_resp;
    }

    // If web search is active, use the search-aware handler for both streaming and non-streaming.
    // The search handler does internal non-streaming loops and synthesizes the final response.
    let resp = if let Some(ref ws_ctx) = web_search_ctx {
        handle_with_web_search(
            &state,
            &runtime_client,
            &bedrock_model,
            bedrock_body,
            &original_model,
            request_id.clone(),
            identity.clone(),
            req_info.clone(),
            start,
            is_streaming,
            ws_ctx,
            endpoint_id,
            &websearch_mode,
        )
        .await
    } else if is_streaming {
        handle_streaming(
            &state,
            &runtime_client,
            &bedrock_model,
            body_bytes.clone(),
            &original_model,
            request_id.clone(),
            identity.clone(),
            req_info.clone(),
            start,
            endpoint_id,
        )
        .await
    } else {
        handle_non_streaming(
            &state,
            &runtime_client,
            &bedrock_model,
            body_bytes.clone(),
            &original_model,
            request_id.clone(),
            identity.clone(),
            req_info.clone(),
            start,
            endpoint_id,
        )
        .await
    };

    // Check if we got a throttle/5xx and should failover to another endpoint
    let mut resp = if let Some(ref primary_ep) = selected_endpoint
        && is_failover_eligible(&resp)
    {
        let fallbacks = state
            .endpoint_pool
            .get_fallback_endpoints(primary_ep.config.id, &team_endpoints)
            .await;

        if !fallbacks.is_empty() {
            tracing::info!(
                request_id = %request_id,
                primary = %primary_ep.config.name,
                fallback_count = fallbacks.len(),
                "Primary endpoint throttled/errored, trying fallback"
            );

            let mut fallback_resp = resp;
            for fallback_ep in &fallbacks {
                // Re-compute model with fallback endpoint's routing prefix
                let fb_model = reprefix_model(
                    &bedrock_model,
                    &routing_prefix,
                    &fallback_ep.config.routing_prefix,
                );
                let fb_client = &fallback_ep.runtime_client;
                let fb_endpoint_id = Some(fallback_ep.config.id);

                let candidate = if web_search_ctx.is_some() {
                    // Web search failover is complex; skip for now, return original error
                    break;
                } else if is_streaming {
                    handle_streaming(
                        &state,
                        fb_client,
                        &fb_model,
                        body_bytes.clone(),
                        &original_model,
                        request_id.clone(),
                        identity.clone(),
                        req_info.clone(),
                        start,
                        fb_endpoint_id,
                    )
                    .await
                } else {
                    handle_non_streaming(
                        &state,
                        fb_client,
                        &fb_model,
                        body_bytes.clone(),
                        &original_model,
                        request_id.clone(),
                        identity.clone(),
                        req_info.clone(),
                        start,
                        fb_endpoint_id,
                    )
                    .await
                };

                fallback_resp = candidate;
                if !is_failover_eligible(&fallback_resp) {
                    // Success or non-retryable error — update affinity
                    if let Some(uid) = user_identity_str {
                        state
                            .endpoint_pool
                            .update_affinity(uid, fallback_ep.config.id)
                            .await;
                    }
                    tracing::info!(
                        request_id = %request_id,
                        endpoint = %fallback_ep.config.name,
                        "Fallback endpoint succeeded"
                    );
                    break;
                }
            }
            fallback_resp
        } else {
            resp
        }
    } else {
        // Update affinity on success with primary endpoint
        if let Some(ref ep) = selected_endpoint
            && !is_failover_eligible(&resp)
            && let Some(uid) = user_identity_str
        {
            state.endpoint_pool.update_affinity(uid, ep.config.id).await;
        }
        resp
    };

    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    state
        .metrics
        .record_request(&original_model, is_streaming, elapsed_ms, "ok");

    tracing::info!(
        request_id = %request_id,
        model = %original_model,
        streaming = is_streaming,
        total_ms = elapsed.as_millis() as u64,
        "Request completed"
    );

    // Add budget response headers
    if let Some(bs) = budget_status {
        let headers = resp.headers_mut();
        headers.insert(
            "x-ccag-budget-percent",
            format!("{:.1}", bs.percent).parse().unwrap(),
        );
        headers.insert(
            "x-ccag-budget-remaining-usd",
            format!("{:.2}", bs.remaining_usd).parse().unwrap(),
        );
        headers.insert("x-ccag-budget-status", bs.status.parse().unwrap());
        headers.insert("x-ccag-budget-resets", bs.resets.parse().unwrap());
    }

    resp
}

/// Check if a response is eligible for failover (throttle or 5xx).
fn is_failover_eligible(resp: &Response) -> bool {
    let status = resp.status();
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Re-prefix a bedrock model ID from one routing prefix to another.
/// E.g., "us.anthropic.claude-3-5-sonnet-20241022-v2:0" with old="us", new="eu"
/// becomes "eu.anthropic.claude-3-5-sonnet-20241022-v2:0".
fn reprefix_model(model: &str, old_prefix: &str, new_prefix: &str) -> String {
    if let Some(suffix) = model.strip_prefix(&format!("{}.", old_prefix)) {
        format!("{}.{}", new_prefix, suffix)
    } else {
        // No prefix match — use model as-is
        model.to_string()
    }
}

fn extract_request_info(req: &request::AnthropicRequest) -> RequestInfo {
    // Extract actual tool invocations from assistant messages (tool_use content blocks),
    // not from the tools array (which is just tool definitions sent every request).
    let mut tool_names: Vec<String> = Vec::new();
    let mut tool_errors: Vec<Value> = Vec::new();
    let mut content_block_types: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut has_correction = false;
    let mut last_was_assistant_with_tool = false;

    for msg in &req.messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
            for block in content {
                if let Some(bt) = block.get("type").and_then(|t| t.as_str()) {
                    content_block_types.insert(bt.to_string());
                }
            }

            if role == "assistant" {
                let mut has_tool_use = false;
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        has_tool_use = true;
                        if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                            tool_names.push(name.to_string());
                        }
                    }
                }
                last_was_assistant_with_tool = has_tool_use;
            } else if role == "user" {
                // Check for tool_result errors
                let mut has_tool_result = false;
                for block in content {
                    let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if bt == "tool_result" {
                        has_tool_result = true;
                        if block
                            .get("is_error")
                            .and_then(|e| e.as_bool())
                            .unwrap_or(false)
                        {
                            // Find the tool name by matching tool_use_id in the previous messages
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(|id| id.as_str())
                                .unwrap_or("");
                            let error_snippet = block
                                .get("content")
                                .and_then(|c| c.as_str())
                                .map(|s| if s.len() > 100 { &s[..100] } else { s })
                                .unwrap_or("unknown");
                            tool_errors.push(json!({
                                "tool_use_id": tool_use_id,
                                "error": error_snippet,
                            }));
                        }
                    }
                }
                // Correction detection: user text message (not tool_result) after assistant tool_use
                if last_was_assistant_with_tool && !has_tool_result {
                    has_correction = true;
                }
                last_was_assistant_with_tool = false;
            }
        }
    }

    let thinking_enabled = req
        .thinking
        .as_ref()
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .map(|t| t == "enabled")
        .unwrap_or(false);

    // Session ID from metadata.user_id (CC sends this as conversation identifier)
    let session_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Project key: extract git remote from system prompt if present
    let project_key = extract_project_key(&req.system);

    // System prompt hash for dedup/change detection
    let system_prompt_hash = req.system.as_ref().map(|sys| {
        use sha2::{Digest, Sha256};
        let serialized = serde_json::to_string(sys).unwrap_or_default();
        let hash = Sha256::digest(serialized.as_bytes());
        hex::encode(&hash[..8]) // 16 hex chars
    });

    // Run detection rules
    let detection_flags = {
        let flags = crate::detection::detect(req);
        if flags.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&flags).unwrap_or(Value::Null))
        }
    };

    RequestInfo {
        tool_count: tool_names.len() as i16,
        tool_names,
        turn_count: req.messages.len() as i16,
        thinking_enabled,
        has_system_prompt: req.system.is_some(),
        session_id,
        project_key,
        tool_errors: if tool_errors.is_empty() {
            None
        } else {
            Some(Value::Array(tool_errors))
        },
        has_correction,
        content_block_types: content_block_types.into_iter().collect(),
        system_prompt_hash,
        detection_flags,
    }
}

/// Extract project key from system prompt. CC includes an <env> block with the
/// working directory path. We use that as a project identifier.
fn extract_project_key(system: &Option<Value>) -> Option<String> {
    let sys = system.as_ref()?;
    let text = match sys {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    // Look for working directory in CC's <env> block
    // Pattern: "Primary working directory: /path/to/project"
    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches("- ");
        if let Some(path) = trimmed.strip_prefix("Primary working directory: ") {
            // Normalize: take last 2 path components as project key
            let parts: Vec<&str> = path.trim().split('/').filter(|s| !s.is_empty()).collect();
            if parts.len() >= 2 {
                return Some(format!(
                    "{}/{}",
                    parts[parts.len() - 2],
                    parts[parts.len() - 1]
                ));
            } else if !parts.is_empty() {
                return Some(parts.last().unwrap().to_string());
            }
        }
    }
    None
}

fn extract_response_metadata(resp: &Value) -> (i32, i32, i32, i32, Option<String>) {
    let usage = resp.get("usage").and_then(|u| u.as_object());
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let cache_read = usage
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let cache_write = usage
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let stop_reason = resp
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .map(String::from);
    (input, output, cache_read, cache_write, stop_reason)
}

#[allow(clippy::too_many_arguments)]
async fn handle_non_streaming(
    state: &GatewayState,
    runtime_client: &aws_sdk_bedrockruntime::Client,
    bedrock_model: &str,
    body_bytes: Vec<u8>,
    original_model: &str,
    request_id: String,
    identity: AuthIdentity,
    req_info: RequestInfo,
    start: Instant,
    endpoint_id: Option<Uuid>,
) -> Response {
    let result = runtime_client
        .invoke_model()
        .model_id(bedrock_model)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(body_bytes))
        .send()
        .await;

    match result {
        Ok(output) => {
            let response_bytes = output.body().as_ref();
            match serde_json::from_slice::<Value>(response_bytes) {
                Ok(resp) => {
                    let (input, output_tok, cache_read, cache_write, stop_reason) =
                        extract_response_metadata(&resp);

                    // Record metrics
                    state.metrics.record_tokens(
                        original_model,
                        input as u64,
                        output_tok as u64,
                        cache_read as u64,
                        cache_write as u64,
                    );
                    state.metrics.record_tools(&req_info.tool_names);

                    // Record spend
                    state
                        .spend_tracker
                        .record(RequestLogEntry {
                            key_id: identity.key_id,
                            user_identity: identity.user_identity,
                            request_id,
                            model: original_model.to_string(),
                            streaming: false,
                            duration_ms: start.elapsed().as_millis() as i32,
                            input_tokens: input,
                            output_tokens: output_tok,
                            cache_read_tokens: cache_read,
                            cache_write_tokens: cache_write,
                            stop_reason,
                            tool_count: req_info.tool_count,
                            tool_names: req_info.tool_names,
                            turn_count: req_info.turn_count,
                            thinking_enabled: req_info.thinking_enabled,
                            has_system_prompt: req_info.has_system_prompt,
                            session_id: req_info.session_id,
                            project_key: req_info.project_key,
                            tool_errors: req_info.tool_errors,
                            has_correction: req_info.has_correction,
                            content_block_types: req_info.content_block_types,
                            system_prompt_hash: req_info.system_prompt_hash,
                            detection_flags: req_info.detection_flags,
                            endpoint_id,
                        })
                        .await;

                    let normalized = response::normalize_response(
                        resp,
                        original_model,
                        Some(&state.model_cache),
                    );
                    Json(normalized).into_response()
                }
                Err(e) => {
                    tracing::error!(%e, "Failed to parse Bedrock response");
                    state.metrics.record_error(
                        "parse_error",
                        endpoint_id.as_ref().map(|id| id.to_string()).as_deref(),
                    );
                    if let Some(ep_id) = endpoint_id {
                        state.endpoint_stats.record_error(ep_id).await;
                    }
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        "Failed to parse upstream response",
                    )
                }
            }
        }
        Err(e) => {
            tracing::error!(%e, "Bedrock InvokeModel failed");
            let ep_str = endpoint_id.as_ref().map(|id| id.to_string());
            state
                .metrics
                .record_error("bedrock_invoke", ep_str.as_deref());
            let (status, message, is_throttle) = map_bedrock_error(&e);
            if is_throttle {
                state
                    .metrics
                    .record_bedrock_throttle(original_model, ep_str.as_deref());
                if let Some(ep_id) = endpoint_id {
                    state.endpoint_stats.record_throttle(ep_id).await;
                }
            }
            if let Some(ep_id) = endpoint_id {
                state.endpoint_stats.record_error(ep_id).await;
            }
            error_response(status, "api_error", &message)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_streaming(
    state: &GatewayState,
    runtime_client: &aws_sdk_bedrockruntime::Client,
    bedrock_model: &str,
    body_bytes: Vec<u8>,
    original_model: &str,
    request_id: String,
    identity: AuthIdentity,
    req_info: RequestInfo,
    start: Instant,
    endpoint_id: Option<Uuid>,
) -> Response {
    let result = runtime_client
        .invoke_model_with_response_stream()
        .model_id(bedrock_model)
        .content_type("application/json")
        .body(Blob::new(body_bytes))
        .send()
        .await;

    match result {
        Ok(output) => {
            let original_model = original_model.to_string();
            let mut event_stream = output.body;

            // Record tool metrics before stream captures req_info
            state.metrics.record_tools(&req_info.tool_names);

            // Shared state for accumulating stream metadata
            let spend_tracker = Arc::clone(&state.spend_tracker);
            let metrics = state.metrics.clone();
            let model_cache = state.model_cache.clone();
            let model_for_spend = original_model.clone();

            let sse_stream = async_stream::stream! {
                let mut input_tokens: i32 = 0;
                let mut output_tokens: i32 = 0;
                let mut cache_read_tokens: i32 = 0;
                let mut cache_write_tokens: i32 = 0;
                let mut stop_reason: Option<String> = None;

                loop {
                    match event_stream.recv().await {
                        Ok(Some(event)) => {
                            use aws_sdk_bedrockruntime::types::ResponseStream;
                            match event {
                                ResponseStream::Chunk(chunk) => {
                                    if let Some(bytes) = chunk.bytes() {
                                        match serde_json::from_slice::<Value>(bytes.as_ref()) {
                                            Ok(event_json) => {
                                                let event_type = event_json
                                                    .get("type")
                                                    .and_then(|t| t.as_str())
                                                    .unwrap_or("unknown")
                                                    .to_string();

                                                // Capture metadata from stream events
                                                match event_type.as_str() {
                                                    "message_start" => {
                                                        if let Some(usage) = event_json.pointer("/message/usage").and_then(|u| u.as_object()) {
                                                            input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                                                            cache_read_tokens = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                                                            cache_write_tokens = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                                                        }
                                                    }
                                                    "message_delta" => {
                                                        if let Some(usage) = event_json.get("usage").and_then(|u| u.as_object()) {
                                                            output_tokens = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                                                        }
                                                        if let Some(delta) = event_json.get("delta").and_then(|d| d.as_object()) {
                                                            stop_reason = delta.get("stop_reason").and_then(|v| v.as_str()).map(String::from);
                                                        }
                                                    }
                                                    _ => {}
                                                }

                                                let normalized = streaming::normalize_stream_event(
                                                    event_json,
                                                    &original_model,
                                                    Some(&model_cache),
                                                );

                                                let sse_text = streaming::format_sse_event(&event_type, &normalized);
                                                yield Ok::<_, std::convert::Infallible>(sse_text);
                                            }
                                            Err(e) => {
                                                tracing::warn!(%e, "Failed to parse stream chunk as JSON");
                                            }
                                        }
                                    }
                                }
                                _ => {
                                    tracing::debug!("Unknown event stream variant");
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!(%e, "Error receiving stream event");
                            let error_event = json!({
                                "type": "error",
                                "error": {"type": "api_error", "message": e.to_string()}
                            });
                            yield Ok(streaming::format_sse_event("error", &error_event));
                            break;
                        }
                    }
                }

                // Record spend after stream completes
                spend_tracker.record(RequestLogEntry {
                    key_id: identity.key_id,
                    user_identity: identity.user_identity,
                    request_id,
                    model: model_for_spend.clone(),
                    streaming: true,
                    duration_ms: start.elapsed().as_millis() as i32,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    stop_reason,
                    tool_count: req_info.tool_count,
                    tool_names: req_info.tool_names,
                    turn_count: req_info.turn_count,
                    thinking_enabled: req_info.thinking_enabled,
                    has_system_prompt: req_info.has_system_prompt,
                    session_id: req_info.session_id,
                    project_key: req_info.project_key,
                    tool_errors: req_info.tool_errors,
                    has_correction: req_info.has_correction,
                    content_block_types: req_info.content_block_types,
                    system_prompt_hash: req_info.system_prompt_hash,
                    detection_flags: req_info.detection_flags,
                    endpoint_id,
                }).await;

                metrics.record_tokens(&model_for_spend, input_tokens as u64, output_tokens as u64, cache_read_tokens as u64, cache_write_tokens as u64);
            };

            let (tx, rx) =
                tokio::sync::mpsc::channel::<Result<String, std::convert::Infallible>>(32);
            tokio::spawn(async move {
                tokio::pin!(sse_stream);
                while let Some(item) = sse_stream.next().await {
                    if tx.send(item).await.is_err() {
                        break;
                    }
                }
            });

            let body =
                axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));

            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive")
                .body(body)
                .unwrap()
        }
        Err(e) => {
            tracing::error!(%e, "Bedrock InvokeModelWithResponseStream failed");
            let ep_str = endpoint_id.as_ref().map(|id| id.to_string());
            state
                .metrics
                .record_error("bedrock_stream", ep_str.as_deref());
            let (status, message, is_throttle) = map_bedrock_error(&e);
            if is_throttle {
                state
                    .metrics
                    .record_bedrock_throttle(original_model, ep_str.as_deref());
                if let Some(ep_id) = endpoint_id {
                    state.endpoint_stats.record_throttle(ep_id).await;
                }
            }
            if let Some(ep_id) = endpoint_id {
                state.endpoint_stats.record_error(ep_id).await;
            }
            error_response(status, "api_error", &message)
        }
    }
}

/// Handle a request that includes web search interception.
///
/// When Claude wants to use web_search, we:
/// 1. Call Bedrock (non-streaming internally)
/// 2. If the response contains web_search tool_use, execute the search via DuckDuckGo
/// 3. Append the tool_result and re-invoke Bedrock
/// 4. Repeat until no more web_search calls or max_uses is exhausted
/// 5. Rewrite the final response: tool_use→server_tool_use, insert web_search_tool_result blocks
///
/// For streaming requests, the final response is synthesized as SSE events.
#[allow(clippy::too_many_arguments)]
async fn handle_with_web_search(
    state: &GatewayState,
    runtime_client: &aws_sdk_bedrockruntime::Client,
    bedrock_model: &str,
    mut bedrock_body: request::BedrockRequest,
    original_model: &str,
    request_id: String,
    identity: AuthIdentity,
    req_info: RequestInfo,
    start: Instant,
    is_streaming: bool,
    ws_ctx: &websearch::WebSearchContext,
    endpoint_id: Option<Uuid>,
    websearch_mode: &str,
) -> Response {
    // Resolve search provider: "global" mode uses admin-configured global provider,
    // otherwise fall back to per-user provider (which defaults to DuckDuckGo).
    let search_provider = if websearch_mode == "global" {
        resolve_global_search_provider(state).await
    } else {
        resolve_search_provider(state, &identity).await
    };
    tracing::debug!(
        request_id = %request_id,
        provider = %search_provider.provider_name(),
        "Resolved search provider"
    );

    let mut total_input_tokens: i32 = 0;
    let mut total_output_tokens: i32 = 0;
    let mut total_cache_read: i32 = 0;
    let mut total_cache_write: i32 = 0;
    let mut search_count: u32 = 0;
    let mut all_search_results: std::collections::HashMap<String, Vec<websearch::WebSearchResult>> =
        std::collections::HashMap::new();
    // Collect all intermediate content blocks to include in the final response
    let mut accumulated_content: Vec<Value> = Vec::new();
    #[allow(unused_assignments)]
    let mut final_stop_reason = None;
    let mut loop_iteration: u32 = 0;
    // Hard cap on loop iterations to prevent runaway loops even if max_uses is high.
    // Each iteration is a full Bedrock round-trip + search execution (~2-3s).
    let max_iterations: u32 = ws_ctx.max_uses.min(10) + 1;

    // Tool-use loop: call Bedrock, check for web_search, execute, repeat
    loop {
        loop_iteration += 1;
        if loop_iteration > max_iterations {
            tracing::warn!(
                request_id = %request_id,
                iterations = loop_iteration - 1,
                "Web search loop hit hard iteration cap"
            );
            break;
        }
        let body_bytes = match serde_json::to_vec(&bedrock_body) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(%e, "Failed to serialize Bedrock request (web search loop)");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    &e.to_string(),
                );
            }
        };

        let result = runtime_client
            .invoke_model()
            .model_id(bedrock_model)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(body_bytes))
            .send()
            .await;

        let resp_value = match result {
            Ok(output) => match serde_json::from_slice::<Value>(output.body().as_ref()) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(%e, "Failed to parse Bedrock response (web search loop)");
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        "Failed to parse upstream response",
                    );
                }
            },
            Err(e) => {
                tracing::error!(%e, "Bedrock call failed (web search loop)");
                let (status, message, is_throttle) = map_bedrock_error(&e);
                if is_throttle {
                    state.metrics.record_bedrock_throttle(
                        original_model,
                        endpoint_id.as_ref().map(|id| id.to_string()).as_deref(),
                    );
                    if let Some(ep_id) = endpoint_id {
                        state.endpoint_stats.record_throttle(ep_id).await;
                    }
                }
                if let Some(ep_id) = endpoint_id {
                    state.endpoint_stats.record_error(ep_id).await;
                }
                return error_response(status, "api_error", &message);
            }
        };

        // Accumulate token usage
        let (input, output_tok, cache_read, cache_write, stop_reason) =
            extract_response_metadata(&resp_value);
        total_input_tokens += input;
        total_output_tokens += output_tok;
        total_cache_read += cache_read;
        total_cache_write += cache_write;
        final_stop_reason = stop_reason.clone();

        let content = resp_value
            .get("content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        // Check for web_search tool_use blocks
        let web_searches = websearch::find_web_search_tool_uses(&content, &ws_ctx.tool_name);

        if web_searches.is_empty() || stop_reason.as_deref() != Some("tool_use") {
            // No more web searches — this is the final response
            accumulated_content.extend(content);
            break;
        }

        // Execute web searches
        let mut tool_results: Vec<Value> = Vec::new();

        for (tool_use_id, query) in &web_searches {
            if search_count >= ws_ctx.max_uses {
                tracing::warn!(
                    request_id = %request_id,
                    max_uses = ws_ctx.max_uses,
                    "Web search max_uses reached, returning error result"
                );
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": "Search limit reached. No more searches available.",
                    "is_error": true,
                }));
                continue;
            }

            tracing::info!(
                request_id = %request_id,
                query = %query,
                search_num = search_count + 1,
                "Executing web search"
            );

            match search_provider.search(&state.http_client, query).await {
                Ok(results) => {
                    let result_text = websearch::results_to_tool_result_text(&results);
                    all_search_results.insert(tool_use_id.clone(), results);
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": result_text,
                    }));
                    search_count += 1;
                }
                Err(e) => {
                    tracing::warn!(%e, request_id = %request_id, "Web search failed");
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": format!("Web search failed: {}", e),
                        "is_error": true,
                    }));
                }
            }
        }

        // If every search in this round was rejected (all hit max_uses),
        // break to avoid an infinite loop where the model keeps retrying.
        let all_rejected = tool_results
            .iter()
            .all(|r| r.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false));
        if all_rejected {
            tracing::warn!(
                request_id = %request_id,
                "All web searches in round hit max_uses, breaking loop"
            );
            accumulated_content.extend(content);
            break;
        }

        // Accumulate the assistant's content (with tool_use blocks) for the final response
        accumulated_content.extend(content.clone());

        // Append assistant message + user tool_result to messages for next iteration
        bedrock_body.messages.push(json!({
            "role": "assistant",
            "content": content,
        }));
        bedrock_body.messages.push(json!({
            "role": "user",
            "content": tool_results,
        }));
    }

    // Rewrite accumulated content: tool_use → server_tool_use + web_search_tool_result
    websearch::rewrite_response_content(
        &mut accumulated_content,
        &all_search_results,
        &ws_ctx.tool_name,
    );

    // Record metrics
    state.metrics.record_tokens(
        original_model,
        total_input_tokens as u64,
        total_output_tokens as u64,
        total_cache_read as u64,
        total_cache_write as u64,
    );
    state.metrics.record_tools(&req_info.tool_names);
    if search_count > 0 {
        state.metrics.record_web_searches(search_count as u64);
    }

    // Record spend
    state
        .spend_tracker
        .record(RequestLogEntry {
            key_id: identity.key_id,
            user_identity: identity.user_identity,
            request_id,
            model: original_model.to_string(),
            streaming: is_streaming,
            duration_ms: start.elapsed().as_millis() as i32,
            input_tokens: total_input_tokens,
            output_tokens: total_output_tokens,
            cache_read_tokens: total_cache_read,
            cache_write_tokens: total_cache_write,
            stop_reason: final_stop_reason.clone(),
            tool_count: req_info.tool_count,
            tool_names: req_info.tool_names,
            turn_count: req_info.turn_count,
            thinking_enabled: req_info.thinking_enabled,
            has_system_prompt: req_info.has_system_prompt,
            session_id: req_info.session_id,
            project_key: req_info.project_key,
            tool_errors: req_info.tool_errors,
            has_correction: req_info.has_correction,
            content_block_types: req_info.content_block_types,
            system_prompt_hash: req_info.system_prompt_hash,
            detection_flags: req_info.detection_flags,
            endpoint_id,
        })
        .await;

    // Build the final response
    let final_response = json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "content": accumulated_content,
        "model": original_model,
        "stop_reason": final_stop_reason.as_deref().unwrap_or("end_turn"),
        "stop_sequence": null,
        "usage": {
            "input_tokens": total_input_tokens,
            "output_tokens": total_output_tokens,
            "cache_creation_input_tokens": total_cache_write,
            "cache_read_input_tokens": total_cache_read,
        }
    });

    if is_streaming {
        // Synthesize SSE events from the non-streaming response
        synthesize_sse_response(final_response)
    } else {
        Json(final_response).into_response()
    }
}

/// Convert a non-streaming response into SSE events for streaming clients.
///
/// This is used when web search interception runs internally as non-streaming
/// but the client requested streaming.
fn synthesize_sse_response(response: Value) -> Response {
    let mut events = Vec::new();

    // message_start
    let mut message = response.clone();
    if let Some(obj) = message.as_object_mut() {
        obj.remove("content");
    }
    events.push(streaming::format_sse_event(
        "message_start",
        &json!({"type": "message_start", "message": message}),
    ));

    // content blocks
    if let Some(content) = response.get("content").and_then(|c| c.as_array()) {
        for (idx, block) in content.iter().enumerate() {
            // content_block_start
            events.push(streaming::format_sse_event(
                "content_block_start",
                &json!({"type": "content_block_start", "index": idx, "content_block": block}),
            ));

            // content_block_delta (for text blocks, emit the text as a delta)
            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text) = block.get("text").and_then(|t| t.as_str())
            {
                events.push(streaming::format_sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                ));
            }

            // content_block_stop
            events.push(streaming::format_sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    // message_delta
    events.push(streaming::format_sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": response.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("end_turn"),
                "stop_sequence": null,
            },
            "usage": response.get("usage").cloned().unwrap_or(json!({})),
        }),
    ));

    // message_stop
    events.push(streaming::format_sse_event(
        "message_stop",
        &json!({"type": "message_stop"}),
    ));

    let body_str = events.join("");
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(axum::body::Body::from(body_str))
        .unwrap()
}

/// Budget status returned from check_budget for adding response headers.
struct BudgetStatus {
    percent: f64,
    remaining_usd: f64,
    status: &'static str,
    resets: String,
    /// If shaping is active, the RPM throttle to apply.
    shaped_rpm: Option<u32>,
}

/// Load global default budget from proxy_settings.
/// Returns (limit, period, optional policy rules) or None if not configured.
pub(crate) async fn load_default_budget(
    pool: &sqlx::PgPool,
) -> Option<(
    f64,
    crate::budget::BudgetPeriod,
    Option<Vec<crate::budget::PolicyRule>>,
)> {
    use crate::budget::BudgetPeriod;

    let amount = crate::db::settings::get_setting(pool, "default_budget_usd")
        .await
        .ok()
        .flatten()
        .and_then(|s| s.parse::<f64>().ok())?;

    let period = crate::db::settings::get_setting(pool, "default_budget_period")
        .await
        .ok()
        .flatten()
        .map(|s| BudgetPeriod::parse(&s))
        .unwrap_or(BudgetPeriod::Monthly);

    let policy: Option<Vec<crate::budget::PolicyRule>> =
        crate::db::settings::get_setting(pool, "default_budget_policy")
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok());

    Some((amount, period, policy))
}

/// Check budget for a user (and their team). Replaces the old check_spend_limit.
/// Returns budget status info for response headers, or an error response if blocked.
async fn check_budget(
    state: &GatewayState,
    user_identity: &str,
) -> Result<Option<BudgetStatus>, Response> {
    use crate::budget::{self, BudgetDecision, BudgetPeriod, PolicyRule};

    let pool = state.db().await;
    let pool = &pool;

    // Look up user
    let user = match crate::db::users::get_user_by_email(pool, user_identity).await {
        Ok(Some(u)) => u,
        _ => return Ok(None),
    };

    // Resolve user budget: explicit > team default > global default
    let user_period = BudgetPeriod::parse(&user.budget_period);
    let team = if let Some(tid) = user.team_id {
        crate::db::teams::get_team(pool, tid).await.ok().flatten()
    } else {
        None
    };

    // Load global default budget (used as final fallback)
    let global_default = if user.spend_limit_monthly_usd.is_none() && team.is_none() {
        load_default_budget(pool).await
    } else {
        None
    };

    let (user_limit, effective_period) = if let Some(limit) = user.spend_limit_monthly_usd {
        (Some(limit), user_period)
    } else if let Some(ref t) = team {
        (
            t.default_user_budget_usd,
            BudgetPeriod::parse(&t.budget_period),
        )
    } else if let Some(ref gd) = global_default {
        (Some(gd.0), gd.1)
    } else {
        (None, user_period)
    };

    // Get user spend (cached)
    let user_spend = if user_limit.is_some() {
        match state.budget_cache.get_user_spend(user_identity).await {
            Some(s) => s,
            None => {
                let s = crate::db::budget::get_user_spend(pool, user_identity, effective_period)
                    .await
                    .unwrap_or(0.0);
                state.budget_cache.set_user_spend(user_identity, s).await;
                s
            }
        }
    } else {
        0.0
    };

    // Resolve user policy: explicit team policy > global default policy > standard preset
    let user_policy: Vec<PolicyRule> = team
        .as_ref()
        .and_then(|t| t.budget_policy.as_ref())
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .or_else(|| global_default.as_ref().and_then(|gd| gd.2.clone()))
        .unwrap_or_else(budget::preset_standard);

    // Evaluate user budget
    let mut decision = if let Some(limit) = user_limit {
        let d = budget::evaluate(&user_policy, user_spend, limit);
        // Record budget events for notify/shape/block
        match &d {
            BudgetDecision::Notify { threshold_percent }
            | BudgetDecision::Shape {
                threshold_percent, ..
            }
            | BudgetDecision::Block { threshold_percent } => {
                let event_type = match &d {
                    BudgetDecision::Notify { .. } => "notify",
                    BudgetDecision::Shape { .. } => "shape",
                    BudgetDecision::Block { .. } => "block",
                    _ => unreachable!(),
                };
                let _ = crate::db::budget::insert_event(
                    pool,
                    Some(user_identity),
                    user.team_id,
                    event_type,
                    *threshold_percent as i32,
                    user_spend,
                    limit,
                    (user_spend / limit) * 100.0,
                    effective_period.as_str(),
                    effective_period.period_start(),
                )
                .await;
            }
            _ => {}
        }
        d
    } else {
        BudgetDecision::Allow
    };

    // Evaluate team budget (if set)
    if let Some(ref team) = team
        && let Some(team_limit) = team.budget_amount_usd
    {
        let team_period = BudgetPeriod::parse(&team.budget_period);
        let team_spend = match state.budget_cache.get_team_spend(team.id).await {
            Some(s) => s,
            None => {
                let s = crate::db::budget::get_team_spend(pool, team.id, team_period)
                    .await
                    .unwrap_or(0.0);
                state.budget_cache.set_team_spend(team.id, s).await;
                s
            }
        };

        let team_policy: Vec<PolicyRule> = team
            .budget_policy
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_else(budget::preset_standard);

        let team_decision = budget::evaluate(&team_policy, team_spend, team_limit);

        // Record team-level events
        match &team_decision {
            BudgetDecision::Notify { threshold_percent }
            | BudgetDecision::Shape {
                threshold_percent, ..
            }
            | BudgetDecision::Block { threshold_percent } => {
                let event_type = match &team_decision {
                    BudgetDecision::Notify { .. } => "team_notify",
                    BudgetDecision::Shape { .. } => "team_shape",
                    BudgetDecision::Block { .. } => "team_block",
                    _ => unreachable!(),
                };
                let _ = crate::db::budget::insert_event(
                    pool,
                    None,
                    Some(team.id),
                    event_type,
                    *threshold_percent as i32,
                    team_spend,
                    team_limit,
                    (team_spend / team_limit) * 100.0,
                    team_period.as_str(),
                    team_period.period_start(),
                )
                .await;
            }
            _ => {}
        }

        decision = budget::most_restrictive(decision, team_decision);
    }

    // Build budget status for headers
    let (status_limit, status_spend, status_period) = if let Some(limit) = user_limit {
        (limit, user_spend, effective_period)
    } else if let Some(ref team) = team {
        if let Some(team_limit) = team.budget_amount_usd {
            let team_period = BudgetPeriod::parse(&team.budget_period);
            let team_spend = state
                .budget_cache
                .get_team_spend(team.id)
                .await
                .unwrap_or(0.0);
            (team_limit, team_spend, team_period)
        } else {
            return Ok(None);
        }
    } else {
        return Ok(None);
    };

    let percent = if status_limit > 0.0 {
        (status_spend / status_limit) * 100.0
    } else {
        0.0
    };
    let remaining = (status_limit - status_spend).max(0.0);

    match decision {
        BudgetDecision::Block { threshold_percent } => {
            tracing::warn!(
                user = %user_identity,
                spend = status_spend,
                limit = status_limit,
                threshold = threshold_percent,
                "Budget blocked"
            );
            Err(budget_block_response(
                status_spend,
                status_limit,
                status_period,
            ))
        }
        BudgetDecision::Shape {
            threshold_percent,
            rpm,
        } => {
            tracing::info!(
                user = %user_identity,
                spend = status_spend,
                limit = status_limit,
                threshold = threshold_percent,
                rpm,
                "Budget shaping active"
            );
            Ok(Some(BudgetStatus {
                percent,
                remaining_usd: remaining,
                status: "shaped",
                resets: status_period.period_start().to_rfc3339(),
                shaped_rpm: Some(rpm),
            }))
        }
        BudgetDecision::Notify { .. } => Ok(Some(BudgetStatus {
            percent,
            remaining_usd: remaining,
            status: "warning",
            resets: status_period.period_start().to_rfc3339(),
            shaped_rpm: None,
        })),
        BudgetDecision::Allow => Ok(Some(BudgetStatus {
            percent,
            remaining_usd: remaining,
            status: "ok",
            resets: status_period.period_start().to_rfc3339(),
            shaped_rpm: None,
        })),
    }
}

fn budget_block_response(spend: f64, limit: f64, period: crate::budget::BudgetPeriod) -> Response {
    let resets = period.period_start().to_rfc3339();
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "application/json")
        .header("x-ccag-budget-percent", format!("{:.1}", (spend / limit) * 100.0))
        .header("x-ccag-budget-remaining-usd", "0.00")
        .header("x-ccag-budget-status", "blocked")
        .header("x-ccag-budget-resets", &resets)
        .body(axum::body::Body::from(
            serde_json::to_string(&serde_json::json!({
                "type": "error",
                "error": {
                    "type": "spend_limit_exceeded",
                    "message": format!(
                        "Budget limit exceeded (${:.2} / ${:.2}). Resets at {}. Contact your admin.",
                        spend, limit, resets
                    )
                }
            }))
            .unwrap(),
        ))
        .unwrap()
}

pub async fn count_tokens(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let auth_result = match check_auth(&headers, &state).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Rate limiting
    if let AuthResult::VirtualKey(ref key) = auth_result
        && let Some(rpm) = key.rate_limit_rpm
        && let Err(retry_after) = state.rate_limiter.check(key.id, rpm as u32).await
    {
        return rate_limit_response(retry_after);
    }

    // Token counting is best-effort. Bedrock doesn't have a direct equivalent.
    // Estimate ~4 chars per token.
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    let estimated_tokens = body_str.len() / 4;

    Json(json!({
        "input_tokens": estimated_tokens
    }))
    .into_response()
}

/// Peek at JWT claims without validation (for logging purposes only).
fn peek_jwt_claims(token: &str) -> Option<(String, String)> {
    let mut validation = jsonwebtoken::Validation::default();
    validation.insecure_disable_signature_validation();
    validation.validate_aud = false;
    validation.validate_exp = false;
    #[derive(serde::Deserialize)]
    struct Claims {
        iss: Option<String>,
        sub: Option<String>,
    }
    let data = jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(b""),
        &validation,
    )
    .ok()?;
    Some((
        data.claims.iss.unwrap_or_default(),
        data.claims.sub.unwrap_or_default(),
    ))
}

/// Check auth. Supports:
/// 1. Virtual keys (from DB cache, if enabled)
/// 2. Gateway session tokens (HS256, issued by this gateway)
/// 3. OIDC JWT tokens (RS256, from external IDPs)
async fn check_auth(headers: &HeaderMap, state: &GatewayState) -> Result<AuthResult, Response> {
    let provided = headers
        .get("x-api-key")
        .or_else(|| headers.get("anthropic-api-key"))
        .or_else(|| headers.get("authorization"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s));

    let key_str = match provided {
        Some(k) => k,
        None => {
            tracing::warn!("Auth failure: missing API key");
            state.metrics.record_auth_failure("missing_key");
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Missing API key",
            ));
        }
    };

    // Try virtual key cache (if enabled)
    if state.virtual_keys_enabled() {
        if let Some(cached) = state.key_cache.validate(key_str).await {
            return Ok(AuthResult::VirtualKey(cached));
        }
    } else if state.key_cache.validate(key_str).await.is_some() {
        // Key exists but virtual keys are disabled
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "authentication_error",
            "Virtual key authentication is disabled by administrator",
        ));
    }

    // Try gateway session token (cheap HMAC check, no network call)
    match crate::auth::session::validate(&state.session_signing_key, key_str) {
        Ok(identity) => {
            tracing::info!(sub = %identity.sub, idp = %identity.idp_name, "Gateway session token validated");
            return Ok(AuthResult::Oidc(identity));
        }
        Err(e) => {
            // Log at info so we can see WHY the session token check failed
            // (e.g. "not a gateway session token", "ExpiredSignature", wrong key)
            let (token_alg, id_hint) = match jsonwebtoken::decode_header(key_str) {
                Ok(h) => {
                    let sub = peek_jwt_claims(key_str)
                        .map(|(_, s)| s)
                        .unwrap_or_else(|| "-".to_string());
                    (format!("{:?}", h.alg), sub)
                }
                Err(_) => {
                    let prefix = if key_str.len() > 8 {
                        &key_str[..8]
                    } else {
                        key_str
                    };
                    ("non-jwt".to_string(), format!("key:{prefix}"))
                }
            };
            tracing::info!(reason = %e, alg = %token_alg, id = %id_hint, "Session token check failed");
        }
    }

    // Try OIDC JWT validation (multi-IDP)
    if state.idp_validator.idp_count().await > 0 {
        match state.idp_validator.validate_token(key_str).await {
            Ok(identity) => {
                tracing::info!(sub = %identity.sub, idp = %identity.idp_name, "OIDC token validated");
                return Ok(AuthResult::Oidc(identity));
            }
            Err(e) => {
                let (token_iss, token_sub) = peek_jwt_claims(key_str).unwrap_or_default();
                tracing::info!(%e, %token_iss, %token_sub, "OIDC token validation failed");
            }
        }
    }

    let key_prefix = if key_str.len() > 8 {
        &key_str[..8]
    } else {
        key_str
    };
    let (fail_iss, fail_sub) = peek_jwt_claims(key_str).unwrap_or_default();
    tracing::warn!(key_prefix = %key_prefix, token_iss = %fail_iss, token_sub = %fail_sub, "Auth failure: invalid API key");
    state.metrics.record_auth_failure("invalid_key");
    Err(error_response(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Invalid API key",
    ))
}

fn rate_limit_response(retry_after: u64) -> Response {
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("retry-after", retry_after.to_string())
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({
                "type": "error",
                "error": {
                    "type": "rate_limit_error",
                    "message": format!("Rate limit exceeded. Retry after {retry_after} seconds.")
                }
            }))
            .unwrap(),
        ))
        .unwrap()
}

/// Resolve the search provider for a given user identity.
/// Looks up the user's configured provider in DB; falls back to DuckDuckGo.
async fn resolve_search_provider(
    state: &GatewayState,
    identity: &AuthIdentity,
) -> websearch::SearchProvider {
    let default = websearch::SearchProvider::DuckDuckGo { max_results: 5 };

    let pool = state.db().await;
    let pool = &pool;

    let user_id = match identity.user_id {
        Some(id) => id,
        None => return default,
    };

    // Look up user's active search provider config
    match crate::db::search_providers::get_active_by_user_id(pool, user_id).await {
        Ok(Some(config)) => match websearch::SearchProvider::from_config(&config) {
            Ok(provider) => provider,
            Err(e) => {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Invalid search provider config, falling back to DuckDuckGo"
                );
                default
            }
        },
        _ => default,
    }
}

/// Resolve the global search provider from admin settings.
/// Reads `websearch_global_provider` from proxy_settings as a JSON config object
/// and constructs a SearchProvider. Falls back to DuckDuckGo if not configured.
async fn resolve_global_search_provider(state: &GatewayState) -> websearch::SearchProvider {
    let default = websearch::SearchProvider::DuckDuckGo { max_results: 5 };

    let pool = state.db().await;
    let config_str =
        match crate::db::settings::get_setting(&pool, "websearch_global_provider").await {
            Ok(Some(s)) => s,
            _ => return default,
        };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Invalid websearch_global_provider JSON, falling back to DuckDuckGo"
            );
            return default;
        }
    };

    match websearch::SearchProvider::from_global_config(&config) {
        Ok(provider) => provider,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to construct global search provider, falling back to DuckDuckGo"
            );
            default
        }
    }
}

fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("x-request-id", &request_id)
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message
                }
            }))
            .unwrap(),
        ))
        .unwrap()
}

/// Map a Bedrock SDK error to an HTTP status and message.
/// Returns `(status, message, is_throttle)` so callers can record throttle metrics.
fn map_bedrock_error<E: std::fmt::Debug + std::fmt::Display>(
    error: &E,
) -> (StatusCode, String, bool) {
    let debug_msg = format!("{error:?}");
    tracing::error!(details = %debug_msg, "Bedrock error details");

    let message = extract_error_message(&debug_msg).unwrap_or_else(|| error.to_string());

    let is_throttle =
        debug_msg.contains("ThrottlingException") || debug_msg.contains("Too many requests");

    let status = if is_throttle {
        StatusCode::TOO_MANY_REQUESTS
    } else if debug_msg.contains("AccessDeniedException") || debug_msg.contains("not authorized") {
        StatusCode::FORBIDDEN
    } else if debug_msg.contains("ValidationException") {
        StatusCode::BAD_REQUEST
    } else if debug_msg.contains("ModelNotReadyException") {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_GATEWAY
    };
    (status, message, is_throttle)
}

fn extract_error_message(debug_msg: &str) -> Option<String> {
    if let Some(start) = debug_msg.find("message: Some(\"") {
        let rest = &debug_msg[start + 15..];
        if let Some(end) = rest.find("\")") {
            return Some(rest[..end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_request(messages: Vec<Value>) -> request::AnthropicRequest {
        request::AnthropicRequest {
            model: "claude-sonnet-4-6-20250514".to_string(),
            max_tokens: Some(1024),
            messages,
            system: None,
            stream: None,
            thinking: None,
            tools: None,
            tool_choice: None,
            metadata: None,
            stop_sequences: None,
            temperature: None,
            top_p: None,
            top_k: None,
            mcp_servers: None,
        }
    }

    #[test]
    fn test_extract_project_key_from_system_prompt() {
        let system = Some(json!([{
            "type": "text",
            "text": "Environment info:\n - Primary working directory: /Users/dev/projects/my-app\n - Is a git repository: true"
        }]));
        assert_eq!(
            extract_project_key(&system),
            Some("projects/my-app".to_string())
        );
    }

    #[test]
    fn test_extract_project_key_string_system() {
        let system = Some(json!(
            "Primary working directory: /home/user/code/repo-name"
        ));
        assert_eq!(
            extract_project_key(&system),
            Some("code/repo-name".to_string())
        );
    }

    #[test]
    fn test_extract_project_key_none() {
        assert_eq!(extract_project_key(&None), None);
        assert_eq!(extract_project_key(&Some(json!("no path here"))), None);
    }

    #[test]
    fn test_extract_request_info_tool_errors() {
        let req = make_request(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "fix it"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "npm test"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ENOENT", "is_error": true}
            ]}),
        ]);

        let info = extract_request_info(&req);
        assert_eq!(info.tool_count, 1);
        assert_eq!(info.tool_names, vec!["Bash"]);
        assert!(info.tool_errors.is_some());
        let errors = info.tool_errors.unwrap();
        assert_eq!(errors.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_extract_request_info_correction_detection() {
        let req = make_request(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "write a test"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Write", "input": {}}
            ]}),
            // User sends text (correction) instead of tool_result
            json!({"role": "user", "content": [{"type": "text", "text": "no, use pytest not unittest"}]}),
        ]);

        let info = extract_request_info(&req);
        assert!(info.has_correction);
    }

    #[test]
    fn test_extract_request_info_no_correction_on_tool_result() {
        let req = make_request(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "run tests"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}),
        ]);

        let info = extract_request_info(&req);
        assert!(!info.has_correction);
    }

    #[test]
    fn test_extract_request_info_session_id_from_metadata() {
        let mut req = make_request(vec![json!({"role": "user", "content": "hello"})]);
        req.metadata = Some(json!({"user_id": "session-abc-123"}));

        let info = extract_request_info(&req);
        assert_eq!(info.session_id, Some("session-abc-123".to_string()));
    }

    #[test]
    fn test_extract_request_info_content_block_types() {
        let req = make_request(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "..."},
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "file contents"}
            ]}),
        ]);

        let info = extract_request_info(&req);
        let mut types = info.content_block_types.clone();
        types.sort();
        assert_eq!(types, vec!["text", "thinking", "tool_result", "tool_use"]);
    }

    #[test]
    fn test_extract_request_info_system_prompt_hash() {
        let mut req = make_request(vec![json!({"role": "user", "content": "hello"})]);
        req.system = Some(json!("You are helpful."));

        let info = extract_request_info(&req);
        assert!(info.system_prompt_hash.is_some());
        assert_eq!(info.system_prompt_hash.as_ref().unwrap().len(), 16);
    }

    #[tokio::test]
    async fn test_list_models_response() {
        let response = list_models().await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        // Verify structure
        assert_eq!(json["has_more"], false);
        let data = json["data"].as_array().unwrap();
        assert!(!data.is_empty());

        // Verify each model has required fields
        for model in data {
            assert!(model["id"].is_string());
            assert!(model["display_name"].is_string());
            assert_eq!(model["type"], "model");
            assert!(model["created_at"].is_string());
        }

        // Verify first/last IDs match data
        assert_eq!(json["first_id"], data.first().unwrap()["id"]);
        assert_eq!(json["last_id"], data.last().unwrap()["id"]);
    }

    #[tokio::test]
    async fn test_list_models_contains_known_models() {
        let response = list_models().await;
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();

        assert!(ids.contains(&"claude-sonnet-4-6-20250514"));
        assert!(ids.contains(&"claude-opus-4-6-20250605"));
        assert!(ids.contains(&"claude-haiku-4-5-20251001"));
    }
}
