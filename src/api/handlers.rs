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
use crate::endpoint::{EndpointClient, ProbeSource, SUFFIX_BETA_MAP};
use crate::proxy::GatewayState;
use crate::telemetry::Metrics;
use crate::translate::{betas as beta_filter, models, request, response, streaming};
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

/// Context passed to `handle_non_streaming` / `handle_streaming` so they can
/// perform opportunistic beta-capability learning and retry-on-rejection.
struct BetaRetryContext {
    /// The endpoint client whose capability cache should be updated.
    endpoint_client: Arc<EndpointClient>,
    /// Bedrock profile ID used as the cache key (e.g. `us.anthropic.claude-opus-4-7`).
    profile: String,
    /// Forwarded betas whose cache state was `None` at filter time.
    /// On HTTP 200 these are marked `(profile, beta) → true` via `RequestSuccess`.
    unknown_betas: Vec<String>,
    /// All betas that were forwarded in this request.
    /// Used to match against Bedrock's `ValidationException` error message.
    forwarded_betas: Vec<String>,
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

/// Derive a human-readable display name from an Anthropic model ID.
///
/// For known models we return the canonical name. For unknown/discovered models
/// we generate a reasonable name from the ID by title-casing the parts.
fn model_display_name(anthropic_id: &str) -> String {
    // Known model display names
    match anthropic_id {
        s if s.starts_with("claude-opus-4-7") => "Claude Opus 4.7".to_string(),
        s if s.starts_with("claude-opus-4-6") => "Claude Opus 4.6".to_string(),
        s if s.starts_with("claude-sonnet-4-6") => "Claude Sonnet 4.6".to_string(),
        s if s.starts_with("claude-opus-4-5") => "Claude Opus 4.5".to_string(),
        s if s.starts_with("claude-sonnet-4-5") => "Claude Sonnet 4.5".to_string(),
        s if s.starts_with("claude-sonnet-4-20") => "Claude Sonnet 4".to_string(),
        s if s.starts_with("claude-haiku-4-5") => "Claude Haiku 4.5".to_string(),
        other => {
            // Strip date suffix and title-case the remaining parts.
            // e.g. "claude-future-5-0-20260601" -> "Claude Future 5 0"
            let base = crate::translate::models::strip_date_suffix(other);
            // Remove ":0" or "-v1:0" style suffixes
            let base = base.trim_end_matches(":0");
            let base = if let Some(pos) = base.rfind("-v1") {
                &base[..pos]
            } else {
                base
            };
            base.split('-')
                .map(|part| {
                    let mut c = part.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

/// Build the `/v1/models` JSON response, including suffix variants for models
/// whose Bedrock profiles have a supported beta capability in the cache.
///
/// Parameters:
/// - `pairs`: (anthropic_id, bedrock_profile_id) pairs, one per available profile.
///   May contain duplicate anthropic_ids (e.g. same model on multiple profile prefixes);
///   deduplication is applied to bare entries and to variant entries independently.
/// - `capability_lookup`: pre-snapshotted closure. Callers `.await` the cache snapshot
///   in async land, then pass an owned closure to this sync helper.
///   - Some(true)  → beta supported on that profile → emit variant
///   - Some(false) → not supported → no variant
///   - None        → cache absent or TTL expired → no variant (bootstrap safety)
/// - `suffix_map`: (suffix, beta_name, display_suffix) tuples. Production caller passes
///   `crate::endpoint::SUFFIX_BETA_MAP`. Tests pass synthetic slices.
///
/// Output sort order: all bare entries first (alphabetic by id), then all suffix
/// variants (alphabetic by id). first_id and last_id reflect the resulting data.
pub(crate) fn build_models_response_with_variants(
    pairs: &[(String, String)],
    capability_lookup: impl Fn(&str, &str) -> Option<bool>,
    suffix_map: &[(&str, &str, &str)],
) -> serde_json::Value {
    let mut bare_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut variant_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Map from variant id → display_name, populated during collection
    let mut variant_display: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for (anthropic_id, bedrock_profile_id) in pairs {
        // Always include the bare entry
        bare_set.insert(anthropic_id.clone());

        // Check each suffix mapping
        for &(suffix, beta_name, display_suffix) in suffix_map {
            if capability_lookup(bedrock_profile_id, beta_name) == Some(true) {
                let variant_id = format!("{anthropic_id}{suffix}");
                if !variant_set.contains(&variant_id) {
                    let bare_display = model_display_name(anthropic_id);
                    let variant_display_name = format!("{bare_display} ({display_suffix})");
                    variant_display.insert(variant_id.clone(), variant_display_name);
                    variant_set.insert(variant_id);
                }
            }
        }
    }

    // Sort bare entries alphabetically
    let mut bare_ids: Vec<String> = bare_set.into_iter().collect();
    bare_ids.sort();

    // Sort variant entries alphabetically
    let mut variant_ids: Vec<String> = variant_set.into_iter().collect();
    variant_ids.sort();

    // Build data: bare entries first, then variant entries
    let mut data: Vec<serde_json::Value> = Vec::new();

    for id in &bare_ids {
        let display = model_display_name(id);
        data.push(json!({
            "id": id,
            "display_name": display,
            "type": "model",
            "created_at": "2025-01-01T00:00:00Z",
        }));
    }

    for id in &variant_ids {
        let display = variant_display
            .get(id)
            .cloned()
            .unwrap_or_else(|| model_display_name(id));
        data.push(json!({
            "id": id,
            "display_name": display,
            "type": "model",
            "created_at": "2025-01-01T00:00:00Z",
        }));
    }

    json!({
        "data": data,
        "has_more": false,
        "first_id": data.first().and_then(|m| m["id"].as_str()),
        "last_id": data.last().and_then(|m| m["id"].as_str()),
    })
}

/// GET /v1/models — List available Claude models (Anthropic API format).
/// Required by Claude for Excel/PowerPoint add-ins.
///
/// Returns models discovered from the endpoint pool's available model cache.
/// Returns an empty list if the cache has not been populated yet
/// (first tick of the health loop runs immediately on startup, so this window
/// is typically very short).
pub async fn list_models(State(state): State<Arc<GatewayState>>) -> Response {
    // Collect Bedrock profile IDs from gateway-wide default available models
    let mut bedrock_ids: Vec<String> = {
        let guard = state.endpoint_pool.default_available_models.read().await;
        guard.clone()
    };

    // Also collect from all endpoint clients in the pool
    let clients = state.endpoint_pool.get_all_clients().await;
    for client in &clients {
        let models = client.available_models.read().await;
        bedrock_ids.extend(models.iter().cloned());
    }

    // If the cache is empty, return an empty list
    if bedrock_ids.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "data": [],
                "has_more": false,
                "first_id": null,
                "last_id": null,
            })),
        )
            .into_response();
    }

    // Build (anthropic_id, bedrock_profile_id) pairs, deduplicating by bedrock_id
    // to avoid feeding the same bedrock profile twice (from default_available_models
    // and per-client available_models which can overlap).
    let mut seen_bedrock: std::collections::HashSet<String> = std::collections::HashSet::new();
    let pairs: Vec<(String, String)> = bedrock_ids
        .iter()
        .filter(|bedrock_id| seen_bedrock.insert((*bedrock_id).clone()))
        .map(|bedrock_id| {
            let anthropic_id = crate::translate::models::bedrock_to_anthropic(
                bedrock_id,
                Some(&state.model_cache),
            );
            (anthropic_id, bedrock_id.clone())
        })
        .collect();

    // Snapshot beta capability data from all endpoint clients.
    // Merge across endpoints: a (profile, beta) is "supported" if ANY endpoint says so.
    let mut supported: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut seen_cap: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    for client in &clients {
        // For each beta in the suffix map, check all profiles this endpoint advertises.
        let profile_ids = client.available_models.read().await.clone();
        for profile in &profile_ids {
            for &(_suffix, beta, _display) in SUFFIX_BETA_MAP {
                let key = (profile.clone(), beta.to_string());
                match client.is_beta_supported(profile, beta).await {
                    Some(true) => {
                        supported.insert(key.clone());
                        seen_cap.insert(key);
                    }
                    Some(false) => {
                        seen_cap.insert(key);
                    }
                    None => {}
                }
            }
        }
    }

    // Build sync capability closure capturing the snapshots.
    let capability_lookup = move |profile: &str, beta: &str| -> Option<bool> {
        let key = (profile.to_string(), beta.to_string());
        if supported.contains(&key) {
            Some(true)
        } else if seen_cap.contains(&key) {
            Some(false)
        } else {
            None
        }
    };

    let response = build_models_response_with_variants(&pairs, capability_lookup, SUFFIX_BETA_MAP);

    (StatusCode::OK, Json(response)).into_response()
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
        Json(serde_json::json!({ "status": status, "db": db_ok, "version": env!("CARGO_PKG_VERSION") })),
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
            user_identity: k.user_email.clone().or_else(|| k.name.clone()),
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

    // Resolve budget identity: OIDC email, or virtual key's assigned user email
    let budget_identity = match &auth_result {
        AuthResult::Oidc(oidc) => Some(oidc.user_id().to_string()),
        AuthResult::VirtualKey(key) => key.user_email.clone(),
    };

    let budget_status = if let Some(ref identity) = budget_identity {
        match check_budget(&state, identity).await {
            Ok(status) => status,
            Err(resp) => return resp,
        }
    } else if let AuthResult::VirtualKey(ref key) = auth_result
        && let Some(team_id) = key.team_id
    {
        // Key assigned to a team but no user -- check team budget only
        match check_team_budget_only(&state, team_id).await {
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

    // Filter team endpoints by model availability (only when team has endpoints configured).
    // Uses suffix matching: pass just the suffix (e.g. "anthropic.claude-sonnet-4-6") so it
    // matches full profile IDs like "us.anthropic.claude-sonnet-4-6" stored in available_models.
    let team_endpoints = if !team_endpoints.is_empty() {
        // Determine the Bedrock suffix for the requested model.
        // Prefer the model cache (exact suffix); fall back to prefix-based translation
        // and strip the region prefix so suffix matching works across regions.
        let bedrock_suffix = if let Some(suffix) = state
            .model_cache
            .lookup_forward_with_fallback(&original_model)
        {
            suffix
        } else {
            // Use default routing prefix to resolve a full Bedrock model ID, then strip prefix.
            let full = crate::translate::models::anthropic_to_bedrock(
                &original_model,
                &state.config.bedrock_routing_prefix,
                Some(&state.model_cache),
            );
            // Strip the leading "<prefix>." component (e.g. "us.") to get the suffix.
            if let Some(dot_pos) = full.find('.') {
                full[dot_pos + 1..].to_string()
            } else {
                full
            }
        };

        let filtered = state
            .endpoint_pool
            .filter_by_model(&team_endpoints, &bedrock_suffix)
            .await;

        if filtered.is_empty() {
            // Team has endpoints but none support this model — return a clear error.
            return build_model_unavailable_error(&original_model);
        }

        filtered
    } else {
        team_endpoints
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

    let (mut bedrock_model, mut bedrock_body, web_search_ctx) = request::translate(
        body,
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
            && let Some((prefix, suffix, display, profile_prefix)) =
                models::discover_model(control_client, &bedrock_model, &routing_prefix).await
        {
            bedrock_model = format!("{}.{}", profile_prefix, &suffix);
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

    // ── Beta capability filtering ────────────────────────────────────────────
    // Filter anthropic_beta through the per-endpoint capability cache.
    // Betas the cache says Some(false) are dropped here to avoid a Bedrock
    // ValidationException. Betas whose cache state is None are kept optimistically
    // and tracked for opportunistic learning when the request succeeds.
    //
    // This block also builds BetaRetryContext for the handler to use when
    // a ValidationException fires at runtime (parse, mark_unsupported, retry once).
    let beta_retry_ctx: Option<BetaRetryContext> = if let Some(ref ep) = selected_endpoint {
        let filter_result =
            beta_filter::filter_betas_by_cache(ep, &bedrock_model, &bedrock_body.anthropic_beta)
                .await;

        if !filter_result.dropped.is_empty() {
            tracing::info!(
                request_id = %request_id,
                dropped = ?filter_result.dropped,
                "Dropped betas that cache records as unsupported"
            );
        }

        let ctx = BetaRetryContext {
            endpoint_client: Arc::clone(ep),
            profile: bedrock_model.clone(),
            unknown_betas: filter_result.unknown.clone(),
            forwarded_betas: filter_result.kept.clone(),
        };
        bedrock_body.anthropic_beta = filter_result.kept;
        Some(ctx)
    } else {
        None
    };

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
            beta_retry_ctx,
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
            beta_retry_ctx,
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
            beta_retry_ctx,
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
                        None, // no beta retry on failover — already retried on primary
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
                        None, // no beta retry on failover — already retried on primary
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
    beta_ctx: Option<BetaRetryContext>,
) -> Response {
    let result = runtime_client
        .invoke_model()
        .model_id(bedrock_model)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(body_bytes.clone()))
        .send()
        .await;

    // Check for ValidationException that names a known beta — retry once with those
    // betas stripped, and mark them as unsupported in the capability cache.
    let result = if let Err(ref e) = result {
        use aws_sdk_bedrockruntime::error::SdkError;
        let is_ve = matches!(e, SdkError::ServiceError(svc) if svc.err().is_validation_exception());
        if is_ve && let Some(ref ctx) = beta_ctx {
            let err_str = format!("{e:?}");
            let rejected = beta_filter::parse_rejected_betas(&err_str, &ctx.forwarded_betas);
            if !rejected.is_empty() {
                tracing::info!(
                    model = %bedrock_model,
                    rejected = ?rejected,
                    "ValidationException named betas — marking unsupported and retrying"
                );
                for beta in &rejected {
                    ctx.endpoint_client
                        .mark_unsupported(&ctx.profile, beta, ProbeSource::RequestRejection)
                        .await;
                }
                // Rebuild body with rejected betas removed.
                let retry_bytes =
                    rebuild_body_without_betas(&body_bytes, &rejected).unwrap_or_default();
                if retry_bytes.is_empty() {
                    // Serialization failed — bubble original error.
                } else {
                    let retry_result = runtime_client
                        .invoke_model()
                        .model_id(bedrock_model)
                        .content_type("application/json")
                        .accept("application/json")
                        .body(Blob::new(retry_bytes))
                        .send()
                        .await;
                    // Return retry result regardless; if it also fails we bubble that.
                    // Per spec: "if retry fails (any reason) → bubble the original Bedrock error".
                    // We actually bubble the retry error here so the client gets a fresh message.
                    // On success the unknown betas are NOT re-marked (we only had ctx.forwarded);
                    // the remaining betas will be marked by the opportunistic path on Ok below.
                    match retry_result {
                        Ok(output) => {
                            // Retry succeeded — opportunistically learn remaining unknown betas.
                            let remaining_unknown: Vec<String> = ctx
                                .unknown_betas
                                .iter()
                                .filter(|b| !rejected.contains(b))
                                .cloned()
                                .collect();
                            for beta in &remaining_unknown {
                                ctx.endpoint_client
                                    .mark_supported(&ctx.profile, beta, ProbeSource::RequestSuccess)
                                    .await;
                            }
                            // Process the successful retry response.
                            let response_bytes = output.body().as_ref();
                            return match serde_json::from_slice::<Value>(response_bytes) {
                                Ok(resp) => {
                                    let (input, output_tok, cache_read, cache_write, stop_reason) =
                                        extract_response_metadata(&resp);
                                    state.metrics.record_tokens(
                                        original_model,
                                        input as u64,
                                        output_tok as u64,
                                        cache_read as u64,
                                        cache_write as u64,
                                    );
                                    state.metrics.record_tools(&req_info.tool_names);
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
                                    tracing::error!(%e, "Failed to parse Bedrock retry response");
                                    error_response(
                                        StatusCode::BAD_GATEWAY,
                                        "api_error",
                                        "Failed to parse upstream response",
                                    )
                                }
                            };
                        }
                        Err(_) => {
                            // Retry failed — fall through and bubble the original error.
                        }
                    }
                }
            }
        }
        result
    } else {
        result
    };

    match result {
        Ok(output) => {
            // Opportunistic learning: betas whose cache state was None are now known to work.
            if let Some(ref ctx) = beta_ctx {
                for beta in &ctx.unknown_betas {
                    ctx.endpoint_client
                        .mark_supported(&ctx.profile, beta, ProbeSource::RequestSuccess)
                        .await;
                }
            }

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
            let model_prefix = bedrock_model
                .find('.')
                .map(|i| &bedrock_model[..i])
                .unwrap_or(bedrock_model);
            let (status, message, is_throttle) =
                map_bedrock_error(&e, Some((original_model, model_prefix)));
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

/// Strip the given `rejected_betas` from the `anthropic_beta` array in `body_bytes`.
///
/// Returns the re-serialized bytes, or `None` if the body can't be parsed.
fn rebuild_body_without_betas(body_bytes: &[u8], rejected: &[String]) -> Option<Vec<u8>> {
    let mut body: Value = serde_json::from_slice(body_bytes).ok()?;
    if let Some(arr) = body
        .get_mut("anthropic_beta")
        .and_then(|v| v.as_array_mut())
    {
        arr.retain(|v| {
            v.as_str()
                .map(|s| !rejected.iter().any(|r| r == s))
                .unwrap_or(true)
        });
    }
    serde_json::to_vec(&body).ok()
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
    beta_ctx: Option<BetaRetryContext>,
) -> Response {
    let result = runtime_client
        .invoke_model_with_response_stream()
        .model_id(bedrock_model)
        .content_type("application/json")
        .body(Blob::new(body_bytes.clone()))
        .send()
        .await;

    // Check for ValidationException naming a known beta — retry once with those betas stripped.
    let (result, beta_ctx) = if let Err(ref e) = result {
        use aws_sdk_bedrockruntime::error::SdkError;
        let is_ve = matches!(
            e,
            SdkError::ServiceError(svc) if svc.err().is_validation_exception()
        );
        if is_ve {
            if let Some(ref ctx) = beta_ctx {
                let err_str = format!("{e:?}");
                let rejected = beta_filter::parse_rejected_betas(&err_str, &ctx.forwarded_betas);
                if !rejected.is_empty() {
                    tracing::info!(
                        model = %bedrock_model,
                        rejected = ?rejected,
                        "ValidationException named betas (stream) — marking unsupported and retrying"
                    );
                    for beta in &rejected {
                        ctx.endpoint_client
                            .mark_unsupported(&ctx.profile, beta, ProbeSource::RequestRejection)
                            .await;
                    }
                    let retry_bytes =
                        rebuild_body_without_betas(&body_bytes, &rejected).unwrap_or_default();
                    if !retry_bytes.is_empty() {
                        let retry_result = runtime_client
                            .invoke_model_with_response_stream()
                            .model_id(bedrock_model)
                            .content_type("application/json")
                            .body(Blob::new(retry_bytes))
                            .send()
                            .await;
                        // Build an updated ctx with the rejected betas removed from unknown_betas
                        let remaining_ctx = BetaRetryContext {
                            endpoint_client: Arc::clone(&ctx.endpoint_client),
                            profile: ctx.profile.clone(),
                            unknown_betas: ctx
                                .unknown_betas
                                .iter()
                                .filter(|b| !rejected.contains(b))
                                .cloned()
                                .collect(),
                            forwarded_betas: ctx.forwarded_betas.clone(),
                        };
                        (retry_result, Some(remaining_ctx))
                    } else {
                        (result, beta_ctx)
                    }
                } else {
                    (result, beta_ctx)
                }
            } else {
                (result, beta_ctx)
            }
        } else {
            (result, beta_ctx)
        }
    } else {
        (result, beta_ctx)
    };

    match result {
        Ok(output) => {
            // Opportunistic learning: Bedrock accepted the request, so unknown betas are now known
            // to work. Fire mark_supported before spawning the stream task.
            if let Some(ref ctx) = beta_ctx {
                for beta in &ctx.unknown_betas {
                    ctx.endpoint_client
                        .mark_supported(&ctx.profile, beta, ProbeSource::RequestSuccess)
                        .await;
                }
            }

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
            let (status, message, is_throttle) = {
                let model_prefix = bedrock_model
                    .find('.')
                    .map(|i| &bedrock_model[..i])
                    .unwrap_or(bedrock_model);
                map_bedrock_error(&e, Some((original_model, model_prefix)))
            };
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
    beta_retry_ctx: Option<BetaRetryContext>,
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
            .body(Blob::new(body_bytes.clone()))
            .send()
            .await;

        // On the first iteration, apply the beta retry pattern:
        // if Bedrock rejects with a ValidationException naming a beta, strip
        // that beta, mark it unsupported, and retry once.  On success, mark
        // any unknown betas as supported.
        let result = if loop_iteration == 1 {
            if let Err(ref e) = result {
                use aws_sdk_bedrockruntime::error::SdkError;
                let is_ve = matches!(
                    e,
                    SdkError::ServiceError(svc) if svc.err().is_validation_exception()
                );
                if is_ve {
                    if let Some(ref ctx) = beta_retry_ctx {
                        let err_str = format!("{e:?}");
                        let rejected =
                            beta_filter::parse_rejected_betas(&err_str, &ctx.forwarded_betas);
                        if !rejected.is_empty() {
                            tracing::info!(
                                model = %bedrock_model,
                                rejected = ?rejected,
                                "ValidationException named betas (web-search path) — marking unsupported and retrying"
                            );
                            for beta in &rejected {
                                ctx.endpoint_client
                                    .mark_unsupported(
                                        &ctx.profile,
                                        beta,
                                        ProbeSource::RequestRejection,
                                    )
                                    .await;
                            }
                            // Strip the rejected betas from bedrock_body for this and
                            // all subsequent iterations in the loop.
                            bedrock_body
                                .anthropic_beta
                                .retain(|b| !rejected.iter().any(|r| r == b));
                            let retry_bytes = rebuild_body_without_betas(&body_bytes, &rejected)
                                .unwrap_or_default();
                            if !retry_bytes.is_empty() {
                                let retry_result = runtime_client
                                    .invoke_model()
                                    .model_id(bedrock_model)
                                    .content_type("application/json")
                                    .accept("application/json")
                                    .body(Blob::new(retry_bytes))
                                    .send()
                                    .await;
                                if retry_result.is_ok() {
                                    // Opportunistic learning: remaining unknown betas work.
                                    let remaining_unknown: Vec<String> = ctx
                                        .unknown_betas
                                        .iter()
                                        .filter(|b| !rejected.contains(b))
                                        .cloned()
                                        .collect();
                                    for beta in &remaining_unknown {
                                        ctx.endpoint_client
                                            .mark_supported(
                                                &ctx.profile,
                                                beta,
                                                ProbeSource::RequestSuccess,
                                            )
                                            .await;
                                    }
                                }
                                retry_result
                            } else {
                                result
                            }
                        } else {
                            result
                        }
                    } else {
                        result
                    }
                } else {
                    result
                }
            } else {
                // First iteration succeeded — opportunistic learning for unknown betas.
                if let Some(ref ctx) = beta_retry_ctx {
                    for beta in &ctx.unknown_betas {
                        ctx.endpoint_client
                            .mark_supported(&ctx.profile, beta, ProbeSource::RequestSuccess)
                            .await;
                    }
                }
                result
            }
        } else {
            result
        };

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
                let (status, message, is_throttle) = map_bedrock_error(&e, None);
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
    // Hoist team values so we can use them in the block response if team is the binding constraint.
    let mut team_binding: Option<(f64, f64, BudgetPeriod)> = None;
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

        // If team blocks but user doesn't, record team as the binding constraint
        let prev_decision = decision.clone();
        decision = budget::most_restrictive(decision, team_decision.clone());
        if matches!(team_decision, BudgetDecision::Block { .. })
            && !matches!(prev_decision, BudgetDecision::Block { .. })
        {
            team_binding = Some((team_spend, team_limit, team_period));
        }
    }

    // Build budget status for headers -- use team values when team is the binding constraint
    let (status_limit, status_spend, status_period) = if let Some((ts, tl, tp)) = team_binding {
        (tl, ts, tp)
    } else if let Some(limit) = user_limit {
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
                resets: status_period.period_next_start().to_rfc3339(),
                shaped_rpm: Some(rpm),
            }))
        }
        BudgetDecision::Notify { .. } => Ok(Some(BudgetStatus {
            percent,
            remaining_usd: remaining,
            status: "warning",
            resets: status_period.period_next_start().to_rfc3339(),
            shaped_rpm: None,
        })),
        BudgetDecision::Allow => Ok(Some(BudgetStatus {
            percent,
            remaining_usd: remaining,
            status: "ok",
            resets: status_period.period_next_start().to_rfc3339(),
            shaped_rpm: None,
        })),
    }
}

/// Check team budget for a virtual key that has only team_id (no user assignment).
async fn check_team_budget_only(
    state: &GatewayState,
    team_id: Uuid,
) -> Result<Option<BudgetStatus>, Response> {
    use crate::budget::{self, BudgetDecision, BudgetPeriod, PolicyRule};

    let pool = state.db().await;
    let pool = &pool;

    let team = match crate::db::teams::get_team(pool, team_id).await {
        Ok(Some(t)) => t,
        _ => return Ok(None),
    };
    let Some(team_limit) = team.budget_amount_usd else {
        return Ok(None);
    };

    let team_period = BudgetPeriod::parse(&team.budget_period);
    let team_spend = match state.budget_cache.get_team_spend(team_id).await {
        Some(s) => s,
        None => {
            let s = crate::db::budget::get_team_spend(pool, team_id, team_period)
                .await
                .unwrap_or(0.0);
            state.budget_cache.set_team_spend(team_id, s).await;
            s
        }
    };

    let team_policy: Vec<PolicyRule> = team
        .budget_policy
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_else(budget::preset_standard);

    let decision = budget::evaluate(&team_policy, team_spend, team_limit);

    match &decision {
        BudgetDecision::Notify { threshold_percent }
        | BudgetDecision::Shape {
            threshold_percent, ..
        }
        | BudgetDecision::Block { threshold_percent } => {
            let event_type = match &decision {
                BudgetDecision::Notify { .. } => "team_notify",
                BudgetDecision::Shape { .. } => "team_shape",
                BudgetDecision::Block { .. } => "team_block",
                _ => unreachable!(),
            };
            let _ = crate::db::budget::insert_event(
                pool,
                None,
                Some(team_id),
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

    let percent = (team_spend / team_limit) * 100.0;
    let remaining = (team_limit - team_spend).max(0.0);

    match decision {
        BudgetDecision::Block { threshold_percent } => {
            tracing::warn!(
                %team_id,
                spend = team_spend,
                limit = team_limit,
                threshold = threshold_percent,
                "Team budget blocked (virtual key)"
            );
            Err(budget_block_response(team_spend, team_limit, team_period))
        }
        BudgetDecision::Shape {
            threshold_percent,
            rpm,
        } => {
            tracing::info!(
                %team_id,
                rpm,
                threshold = threshold_percent,
                "Team budget shaping active (virtual key)"
            );
            Ok(Some(BudgetStatus {
                percent,
                remaining_usd: remaining,
                status: "shaped",
                resets: team_period.period_next_start().to_rfc3339(),
                shaped_rpm: Some(rpm),
            }))
        }
        BudgetDecision::Notify { .. } => Ok(Some(BudgetStatus {
            percent,
            remaining_usd: remaining,
            status: "warning",
            resets: team_period.period_next_start().to_rfc3339(),
            shaped_rpm: None,
        })),
        BudgetDecision::Allow => Ok(Some(BudgetStatus {
            percent,
            remaining_usd: remaining,
            status: "ok",
            resets: team_period.period_next_start().to_rfc3339(),
            shaped_rpm: None,
        })),
    }
}

fn budget_block_response(spend: f64, limit: f64, period: crate::budget::BudgetPeriod) -> Response {
    let resets = period.period_next_start().to_rfc3339();
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

/// Build a 400 response indicating the requested model is not available on any
/// of the team's configured endpoints.
pub(crate) fn build_model_unavailable_error(model: &str) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        &format!(
            "Model '{model}' is not available on any of your team's configured endpoints. Contact your gateway administrator."
        ),
    )
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
    model_context: Option<(&str, &str)>,
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
    let message = if status == StatusCode::BAD_REQUEST
        && message.contains("model identifier is invalid")
        && let Some((model, prefix)) = model_context
    {
        format!(
            "{model} is not available on your endpoint ({prefix} region). \
             Use /model to select a different model, or ask your admin to add a global endpoint."
        )
    } else {
        message
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

// ── Slice 2: dynamic list_models tests ──

#[cfg(test)]
mod tests_list_models_slice2 {
    use super::*;
    use axum::extract::State;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicI64};
    use std::time::Instant;

    /// Build a minimal GatewayState for unit-testing list_models.
    ///
    /// Only the fields that list_models actually reads are meaningful here:
    /// - `endpoint_pool.default_available_models` — models returned for unauthenticated callers
    /// - `endpoint_pool` clients — per-endpoint available_models
    /// - `model_cache` — used by bedrock_to_anthropic for reverse mapping
    ///
    /// All other fields are zeroed/stubbed so we never touch the network or DB.
    pub(super) fn make_test_state(default_models: Vec<String>) -> Arc<GatewayState> {
        let (metrics, _provider) = crate::telemetry::Metrics::new(None).unwrap();
        let metrics = Arc::new(metrics);
        let db_pool = Arc::new(tokio::sync::RwLock::new(
            // A deliberately invalid pool — list_models must not touch the DB for the
            // unauthenticated path we're testing.
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_lazy("postgres://localhost/invalid_test_db")
                .unwrap(),
        ));
        let spend_tracker = Arc::new(crate::spend::SpendTracker::new(
            Arc::clone(&db_pool),
            Arc::clone(&metrics),
        ));
        let endpoint_pool = Arc::new(crate::endpoint::EndpointPool::new());

        // Pre-populate default_available_models synchronously via blocking write.
        // We use `futures::executor::block_on` here to satisfy the async RwLock.
        // This is acceptable inside a `#[cfg(test)]` helper that runs inside tokio.
        let pool_clone = Arc::clone(&endpoint_pool);
        let default_models_clone = default_models.clone();
        // tokio::sync::RwLock::blocking_write is not available; use try_write since
        // no contention exists yet.
        {
            let mut guard = pool_clone
                .default_available_models
                .try_write()
                .expect("No contention expected in test setup");
            *guard = default_models_clone;
        }

        let aws_config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .build();

        let bedrock_client = aws_sdk_bedrockruntime::Client::from_conf(
            aws_sdk_bedrockruntime::Config::builder()
                .behavior_version(aws_sdk_bedrockruntime::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let bedrock_control_client = aws_sdk_bedrock::Client::from_conf(
            aws_sdk_bedrock::Config::builder()
                .behavior_version(aws_sdk_bedrock::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let pricing_client = Arc::new(aws_sdk_pricing::Client::from_conf(
            aws_sdk_pricing::Config::builder()
                .behavior_version(aws_sdk_pricing::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        ));

        Arc::new(GatewayState {
            bedrock_client,
            bedrock_control_client,
            model_cache: crate::translate::models::ModelCache::new(),
            config: crate::config::GatewayConfig {
                host: "127.0.0.1".to_string(),
                port: 8080,
                admin_username: "admin".to_string(),
                admin_password: "admin".to_string(),
                bedrock_routing_prefix: "us".to_string(),
                database_url: "postgres://localhost/test".to_string(),
                admin_users: vec![],
                notification_url: None,
                rds_iam_auth: false,
                database_host: None,
                database_port: 5432,
                database_name: "test".to_string(),
                database_user: "test".to_string(),
                pricing_refresh_interval: 86400,
                pricing_refresh_enabled: false,
            },
            key_cache: crate::auth::KeyCache::new(),
            rate_limiter: crate::ratelimit::RateLimiter::new(),
            idp_validator: Arc::new(crate::auth::oidc::MultiIdpValidator::new()),
            db_pool: Arc::clone(&db_pool),
            spend_tracker,
            metrics: Arc::clone(&metrics),
            virtual_keys_enabled: AtomicBool::new(false),
            admin_login_enabled: AtomicBool::new(false),
            cache_version: AtomicI64::new(0),
            session_token_ttl_hours: AtomicI64::new(24),
            session_signing_key: "test-signing-key".to_string(),
            cli_sessions: crate::api::cli_auth::new_session_store(),
            setup_tokens: tokio::sync::RwLock::new(HashMap::new()),
            http_client: reqwest::Client::new(),
            budget_cache: Arc::new(crate::budget::BudgetSpendCache::new(30)),
            sns_client: None,
            eb_client: None,
            quota_cache: None,
            bedrock_health: tokio::sync::RwLock::new(None),
            endpoint_pool,
            endpoint_stats: Arc::new(crate::endpoint::stats::EndpointStats::new()),
            aws_config,
            started_at: Instant::now(),
            login_attempts: tokio::sync::Mutex::new(vec![]),
            pricing_client,
        })
    }

    // ── Test 1: Unauthenticated request returns gateway-wide union ──────────────────
    //
    // When no Authorization / x-api-key header is provided, list_models must fall
    // back to the gateway-wide union: models stored in
    // `endpoint_pool.default_available_models`.
    #[tokio::test]
    async fn test_list_models_unauthenticated_returns_gateway_wide_union() {
        let models = vec![
            "us.anthropic.claude-sonnet-4-6-20250514".to_string(),
            "us.anthropic.claude-opus-4-7".to_string(),
        ];
        let state = make_test_state(models);

        let resp = list_models(State(state)).await;

        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "unauthenticated list_models must return 200"
        );

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        assert!(
            !data.is_empty(),
            "gateway-wide models must be present when default_available_models is populated"
        );

        // The two models we seeded should appear (after Bedrock → Anthropic mapping).
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert!(
            ids.contains(&"claude-sonnet-4-6-20250514"),
            "us.anthropic.claude-sonnet-4-6-20250514 must map to claude-sonnet-4-6-20250514"
        );
        assert!(
            ids.contains(&"claude-opus-4-7"),
            "us.anthropic.claude-opus-4-7 must map to claude-opus-4-7"
        );
    }

    // ── Test 2: Empty cache returns empty list ───────────────────────────────────────
    //
    // After Slice 4, when `default_available_models` is empty (before the health
    // loop has run), list_models must return an empty data array rather than the
    // old hardcoded fallback list.
    //
    // The health loop fires on its first tick immediately at startup, so the empty
    // window is short in practice. Claude Code handles an empty list gracefully.
    #[tokio::test]
    async fn test_list_models_empty_cache_returns_empty_list() {
        // Pass an empty default_available_models — simulates the cold-start window.
        let state = make_test_state(vec![]);

        let resp = list_models(State(state)).await;

        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "empty-cache list_models must still return 200"
        );

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");

        // After Slice 4 the hardcoded fallback is gone: empty cache → empty list.
        assert!(
            data.is_empty(),
            "list_models must return an empty list when default_available_models is empty (no hardcoded fallback)"
        );

        assert_eq!(json["has_more"], false, "has_more must be false");
        assert!(
            json["first_id"].is_null(),
            "first_id must be null for empty list"
        );
        assert!(
            json["last_id"].is_null(),
            "last_id must be null for empty list"
        );
    }

    // ── Test 3: Response format matches Anthropic API ────────────────────────────────
    //
    // The response envelope must match the Anthropic models API format exactly so
    // Claude Code and other clients can parse it without modification.
    #[tokio::test]
    async fn test_list_models_response_format_matches_anthropic_api() {
        let state = make_test_state(vec!["us.anthropic.claude-sonnet-4-6-20250514".to_string()]);

        let resp = list_models(State(state)).await;

        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        // Top-level envelope fields
        assert_eq!(
            json["has_more"], false,
            "has_more must be false (no pagination)"
        );
        assert!(
            json["first_id"].is_string() || json["first_id"].is_null(),
            "first_id must be a string or null"
        );
        assert!(
            json["last_id"].is_string() || json["last_id"].is_null(),
            "last_id must be a string or null"
        );

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        assert!(!data.is_empty());

        // Each model object must have the required fields
        for model in data {
            assert!(
                model["id"].is_string(),
                "each model must have a string 'id' field"
            );
            assert!(
                model["display_name"].is_string(),
                "each model must have a string 'display_name' field"
            );
            assert_eq!(
                model["type"], "model",
                "each model must have type == \"model\""
            );
            assert!(
                model["created_at"].is_string(),
                "each model must have a string 'created_at' field"
            );
        }

        // first_id and last_id must match the first and last data element
        assert_eq!(
            json["first_id"],
            data.first().unwrap()["id"],
            "first_id must equal the id of the first model in data"
        );
        assert_eq!(
            json["last_id"],
            data.last().unwrap()["id"],
            "last_id must equal the id of the last model in data"
        );
    }

    // ── Test 4: Models are deduplicated across endpoints ─────────────────────────────
    //
    // If the same Bedrock profile ID appears in multiple sources (e.g. the same
    // model is available on two different endpoints), it must appear only once in
    // the response.
    #[tokio::test]
    async fn test_list_models_deduplication() {
        // Seed default_available_models with two identical Bedrock profile IDs.
        // The new list_models implementation must return each Anthropic ID exactly once.
        let state = make_test_state(vec![
            "us.anthropic.claude-sonnet-4-6-20250514".to_string(),
            "us.anthropic.claude-sonnet-4-6-20250514".to_string(), // exact duplicate
            "eu.anthropic.claude-sonnet-4-6-20250514".to_string(), // same model, different region prefix
        ]);

        let resp = list_models(State(state)).await;

        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");

        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        // Count how many times claude-sonnet-4-6-20250514 appears
        let count = ids
            .iter()
            .filter(|&&id| id == "claude-sonnet-4-6-20250514")
            .count();
        assert_eq!(
            count, 1,
            "claude-sonnet-4-6-20250514 must appear exactly once even when \
             present on multiple endpoints or under different region prefixes"
        );
    }

    // ── Test 5: Bedrock IDs mapped back to Anthropic format ──────────────────────────
    //
    // The response must contain Anthropic-format model IDs (e.g. "claude-sonnet-4-6-20250514"),
    // not raw Bedrock profile IDs (e.g. "us.anthropic.claude-sonnet-4-6-20250514").
    #[tokio::test]
    async fn test_list_models_bedrock_ids_mapped_to_anthropic_format() {
        let state = make_test_state(vec![
            // Full Bedrock cross-region inference profile IDs
            "us.anthropic.claude-sonnet-4-6-20250514".to_string(),
            "us.anthropic.claude-opus-4-7".to_string(),
            "us.anthropic.claude-haiku-4-5-20251001-v1:0".to_string(),
        ]);

        let resp = list_models(State(state)).await;

        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");

        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        // No raw Bedrock IDs (those contain dots) must appear in the response
        for id in &ids {
            assert!(
                !id.starts_with("us.") && !id.starts_with("eu.") && !id.starts_with("ap."),
                "response must not contain raw Bedrock profile IDs; found '{id}'"
            );
        }

        // The mapped Anthropic IDs must be present
        assert!(
            ids.contains(&"claude-sonnet-4-6-20250514"),
            "us.anthropic.claude-sonnet-4-6-20250514 must map to claude-sonnet-4-6-20250514"
        );
        assert!(
            ids.contains(&"claude-opus-4-7"),
            "us.anthropic.claude-opus-4-7 must map to claude-opus-4-7"
        );
        assert!(
            ids.contains(&"claude-haiku-4-5-20251001"),
            "us.anthropic.claude-haiku-4-5-20251001-v1:0 must map to claude-haiku-4-5-20251001"
        );
    }
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
            anthropic_beta: vec![],
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

    /// Build a minimal GatewayState for tests that need to call list_models.
    /// Pre-populates the endpoint pool with known models so list_models returns
    /// a non-empty list (the hardcoded fallback was removed in Slice 4).
    fn make_minimal_state_for_list_models() -> Arc<GatewayState> {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, AtomicI64};
        use std::time::Instant;

        let (metrics, _provider) = crate::telemetry::Metrics::new(None).unwrap();
        let metrics = Arc::new(metrics);
        let db_pool = Arc::new(tokio::sync::RwLock::new(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_lazy("postgres://localhost/invalid_test_db")
                .unwrap(),
        ));
        let spend_tracker = Arc::new(crate::spend::SpendTracker::new(
            Arc::clone(&db_pool),
            Arc::clone(&metrics),
        ));
        let endpoint_pool = Arc::new(crate::endpoint::EndpointPool::new());

        // Pre-populate the cache so list_models returns models (no hardcoded fallback).
        {
            let mut guard = endpoint_pool
                .default_available_models
                .try_write()
                .expect("No contention expected in test setup");
            *guard = vec![
                "us.anthropic.claude-sonnet-4-6-20250514".to_string(),
                "us.anthropic.claude-opus-4-6-20250605".to_string(),
                "us.anthropic.claude-haiku-4-5-20251001".to_string(),
            ];
        }

        let aws_config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .build();
        let bedrock_client = aws_sdk_bedrockruntime::Client::from_conf(
            aws_sdk_bedrockruntime::Config::builder()
                .behavior_version(aws_sdk_bedrockruntime::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let bedrock_control_client = aws_sdk_bedrock::Client::from_conf(
            aws_sdk_bedrock::Config::builder()
                .behavior_version(aws_sdk_bedrock::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let pricing_client = Arc::new(aws_sdk_pricing::Client::from_conf(
            aws_sdk_pricing::Config::builder()
                .behavior_version(aws_sdk_pricing::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        ));
        Arc::new(GatewayState {
            bedrock_client,
            bedrock_control_client,
            model_cache: crate::translate::models::ModelCache::new(),
            config: crate::config::GatewayConfig {
                host: "127.0.0.1".to_string(),
                port: 8080,
                admin_username: "admin".to_string(),
                admin_password: "admin".to_string(),
                bedrock_routing_prefix: "us".to_string(),
                database_url: "postgres://localhost/test".to_string(),
                admin_users: vec![],
                notification_url: None,
                rds_iam_auth: false,
                database_host: None,
                database_port: 5432,
                database_name: "test".to_string(),
                database_user: "test".to_string(),
                pricing_refresh_interval: 86400,
                pricing_refresh_enabled: false,
            },
            key_cache: crate::auth::KeyCache::new(),
            rate_limiter: crate::ratelimit::RateLimiter::new(),
            idp_validator: Arc::new(crate::auth::oidc::MultiIdpValidator::new()),
            db_pool: Arc::clone(&db_pool),
            spend_tracker,
            metrics: Arc::clone(&metrics),
            virtual_keys_enabled: AtomicBool::new(false),
            admin_login_enabled: AtomicBool::new(false),
            cache_version: AtomicI64::new(0),
            session_token_ttl_hours: AtomicI64::new(24),
            session_signing_key: "test-signing-key".to_string(),
            cli_sessions: crate::api::cli_auth::new_session_store(),
            setup_tokens: tokio::sync::RwLock::new(HashMap::new()),
            http_client: reqwest::Client::new(),
            budget_cache: Arc::new(crate::budget::BudgetSpendCache::new(30)),
            sns_client: None,
            eb_client: None,
            quota_cache: None,
            bedrock_health: tokio::sync::RwLock::new(None),
            endpoint_pool,
            endpoint_stats: Arc::new(crate::endpoint::stats::EndpointStats::new()),
            aws_config,
            started_at: Instant::now(),
            login_attempts: tokio::sync::Mutex::new(vec![]),
            pricing_client,
        })
    }

    #[tokio::test]
    async fn test_list_models_response() {
        let state = make_minimal_state_for_list_models();
        let response = list_models(State(state)).await;
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
        let state = make_minimal_state_for_list_models();
        let response = list_models(State(state)).await;
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

// ── Dynamic Model Availability — Slice 3 tests ──
// Test 5: clear error response when no endpoint supports the requested model.
//
// Depends on:
//   • `EndpointPool::filter_by_model` (src/endpoint/mod.rs) — tests 1-4
//   • `build_model_unavailable_error` — a new pub(crate) function in this file
//     that returns HTTP 400 with body:
//       {"error": {"type": "invalid_request_error", "message": "Model '<name>' is not available ..."}}
#[cfg(test)]
mod tests_model_filtering_slice3 {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;

    /// When no endpoint in the team supports the requested model, the messages
    /// handler must return HTTP 400 with `"invalid_request_error"` and an error
    /// message that names the model.
    ///
    /// This test calls `build_model_unavailable_error(model_name)` — a
    /// `pub(crate)` function in `src/api/handlers.rs`.
    #[tokio::test]
    async fn test_model_unavailable_error_shape() {
        let response = build_model_unavailable_error("claude-sonnet-4-6-20250514");

        let (parts, body) = response.into_parts();

        assert_eq!(
            parts.status,
            StatusCode::BAD_REQUEST,
            "model unavailable error must use HTTP 400 status"
        );

        let body_bytes = to_bytes(body, usize::MAX)
            .await
            .expect("body must be readable");
        let json: Value = serde_json::from_slice(&body_bytes).expect("body must be valid JSON");

        assert_eq!(
            json["error"]["type"].as_str(),
            Some("invalid_request_error"),
            "error type must be 'invalid_request_error'"
        );

        let message = json["error"]["message"]
            .as_str()
            .expect("error.message must be a string");
        assert!(
            message.contains("claude-sonnet-4-6-20250514"),
            "error message must contain the model name; got: {message}"
        );
    }

    /// The error message produced by `build_model_unavailable_error` must
    /// direct the user to contact their gateway administrator, not just report
    /// a bare model name. This validates the full guidance text per the spec.
    #[tokio::test]
    async fn test_model_unavailable_error_message_contains_guidance() {
        let response = build_model_unavailable_error("claude-opus-4-7");

        let (_, body) = response.into_parts();
        let body_bytes = to_bytes(body, usize::MAX)
            .await
            .expect("body must be readable");
        let json: Value = serde_json::from_slice(&body_bytes).expect("body must be valid JSON");

        let message = json["error"]["message"]
            .as_str()
            .expect("error.message must be a string");

        // Per spec: message should mention the model and guide users to contact admin
        assert!(
            message.contains("claude-opus-4-7"),
            "error message must name the requested model; got: {message}"
        );
        assert!(
            message.to_lowercase().contains("endpoint")
                || message.to_lowercase().contains("administrator")
                || message.to_lowercase().contains("admin"),
            "error message must direct the user to contact an administrator or mention endpoints; got: {message}"
        );
    }
}

// ── Slice 4: Remove hardcoded model list ────────────────────────────────────────
//
// These tests verify the post-Slice-4 behavior:
//   - When the cache is empty, list_models returns an empty data array (NOT the
//     old 7-entry hardcoded list).
//   - When the cache has exactly N entries, the response has exactly N models —
//     proving no hardcoded fallback inflates the count.
//   - A pre-populated cache is the sole source of truth; list_models does not
//     fall back to any static data.
#[cfg(test)]
mod tests_slice4_no_hardcoded {
    use super::*;
    use axum::extract::State;
    use serde_json::Value;

    /// Build a minimal GatewayState for unit-testing list_models.
    fn make_test_state(default_models: Vec<String>) -> Arc<GatewayState> {
        super::tests_list_models_slice2::make_test_state(default_models)
    }

    // ── Test 1: Empty cache returns empty data array ─────────────────────────────
    //
    // After Slice 4, when both `default_available_models` and all endpoint client
    // `available_models` are empty, `list_models` must return:
    //   {"data": [], "has_more": false, "first_id": null, "last_id": null}
    #[tokio::test]
    async fn test_empty_cache_returns_empty_data_array() {
        let state = make_test_state(vec![]); // no models in cache

        let resp = list_models(State(state)).await;

        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "list_models must always return 200 even when cache is empty"
        );

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");

        assert_eq!(
            data.len(),
            0,
            "empty cache must yield an empty data array; \
             got {} entries (hardcoded fallback must be removed)",
            data.len()
        );

        assert_eq!(
            json["has_more"], false,
            "has_more must be false when data is empty"
        );

        assert!(
            json["first_id"].is_null(),
            "first_id must be null when there are no models"
        );
        assert!(
            json["last_id"].is_null(),
            "last_id must be null when there are no models"
        );
    }

    // ── Test 2: Cache with N models returns exactly N models ─────────────────────
    //
    // Seeding 3 distinct Bedrock profile IDs must produce exactly 3 models in the
    // response — not 3 + 7 (the old hardcoded count) and not 10 or anything else.
    //
    // This proves the hardcoded list is not being appended or merged into the result
    // even when the cache is non-empty.
    #[tokio::test]
    async fn test_cache_with_three_models_returns_exactly_three() {
        let state = make_test_state(vec![
            "us.anthropic.claude-sonnet-4-6-20250514".to_string(),
            "us.anthropic.claude-opus-4-7".to_string(),
            "us.anthropic.claude-haiku-4-5-20251001".to_string(),
        ]);

        let resp = list_models(State(state)).await;

        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");

        assert_eq!(
            data.len(),
            3,
            "seeding 3 Bedrock profile IDs must yield exactly 3 models in the response; \
             got {} — the hardcoded 7-entry fallback must not contribute any entries",
            data.len()
        );
    }

    // ── Test 3: Pre-populated cache is the sole source of truth ──────────────────
    //
    // When the cache holds specific models, the response must contain exactly and
    // only those models (mapped to Anthropic format). No extra models must appear
    // from any static fallback.
    //
    // This test verifies both the positive (seeded models present) and negative
    // (no unexpected extras from HARDCODED_MODELS) sides of the invariant.
    #[tokio::test]
    async fn test_prepopulated_cache_is_sole_source_of_truth() {
        // Seed exactly two well-known models that happen to also be in
        // HARDCODED_MODELS, so we can confirm the count doesn't double up.
        let state = make_test_state(vec![
            "us.anthropic.claude-sonnet-4-6-20250514".to_string(),
            "us.anthropic.claude-opus-4-7".to_string(),
        ]);

        let resp = list_models(State(state)).await;

        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");

        // Positive: both seeded models must be present (Bedrock → Anthropic mapped)
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert!(
            ids.contains(&"claude-sonnet-4-6-20250514"),
            "cache-seeded claude-sonnet-4-6-20250514 must appear in response"
        );
        assert!(
            ids.contains(&"claude-opus-4-7"),
            "cache-seeded claude-opus-4-7 must appear in response"
        );

        // Negative: count must equal exactly 2 — the hardcoded list must not
        // contribute any of the 5 remaining models it contains beyond these two.
        assert_eq!(
            data.len(),
            2,
            "response must contain exactly the 2 cache-seeded models; \
             got {} — HARDCODED_MODELS must be fully removed as a data source \
             (the old hardcoded list had 7 entries)",
            data.len()
        );

        // Envelope integrity
        assert_eq!(json["has_more"], false);
        assert!(
            json["first_id"].is_string(),
            "first_id must be a non-null string when data is non-empty"
        );
        assert!(
            json["last_id"].is_string(),
            "last_id must be a non-null string when data is non-empty"
        );
    }
}

// ── Slice 5: `/v1/models` advertises suffix variants from cache ─────────────────────────────
//
// These tests exercise a pure helper function that the Builder must extract:
//
//   `pub(crate) fn build_models_response_with_variants(
//       pairs: &[(String, String)],           // (anthropic_id, bedrock_profile_id)
//       capability_lookup: impl Fn(&str, &str) -> Option<bool>,  // (profile, beta) -> Option<bool>
//       suffix_map: &[(&str, &str, &str)],    // (suffix, beta_name, display_suffix)
//   ) -> serde_json::Value`
//
// Design rationale:
//   - `capability_lookup` is SYNC (not async). The production caller pre-snapshots
//     the async RwLock in `list_models` and hands a closure over the snapshot to
//     this pure function. Tests can then pass a simple closure over a HashMap
//     without any async machinery.
//   - `suffix_map` is an explicit parameter (not a direct read of the `SUFFIX_BETA_MAP`
//     const) so Test 8 can inject a synthetic 2-entry slice without modifying
//     production constants.
//   - `pairs` uses the `(anthropic_id, bedrock_profile_id)` shape because the
//     production caller already iterates bedrock_ids → anthropic mapping and needs
//     to keep both sides for the capability lookup.
//
// All tests are `#[test]` (not `#[tokio::test]`) because the helper is sync.
// Tests are tagged `offline` in their doc comments to signal they require no DB/AWS.
#[cfg(test)]
mod tests_t5_suffix_variants {
    use super::*;
    use serde_json::Value;
    use std::collections::HashMap;

    // ── helper to call the function under test ────────────────────────────────────

    /// Thin wrapper so tests don't need to repeat the turbofish / type annotation
    /// for the closure.
    fn run(
        pairs: &[(&str, &str)],
        capability_lookup: impl Fn(&str, &str) -> Option<bool>,
        suffix_map: &[(&str, &str, &str)],
    ) -> Value {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        build_models_response_with_variants(&owned, capability_lookup, suffix_map)
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 1 [offline]: variant emitted when capability cache says Some(true)
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // Input:  one model pair; cache returns `Some(true)` for its beta.
    // Expect: response contains BOTH the bare entry and the `[1m]` suffixed variant.
    //         The variant's display name is "<existing> (1M context)".
    #[test]
    fn variants_emitted_for_supported_betas() {
        let pairs = &[("claude-opus-4-7", "us.anthropic.claude-opus-4-7")];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        let json = run(
            pairs,
            |profile, beta| {
                if profile == "us.anthropic.claude-opus-4-7" && beta == "context-1m-2025-08-07" {
                    Some(true)
                } else {
                    None
                }
            },
            suffix_map,
        );

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        assert!(
            ids.contains(&"claude-opus-4-7"),
            "bare entry must be present; got ids: {ids:?}"
        );
        assert!(
            ids.contains(&"claude-opus-4-7[1m]"),
            "[1m] variant must be emitted when cache returns Some(true); got ids: {ids:?}"
        );

        // Verify the display name of the variant.
        let variant = data
            .iter()
            .find(|m| m["id"].as_str() == Some("claude-opus-4-7[1m]"))
            .expect("[1m] variant entry must exist in data");

        let display = variant["display_name"]
            .as_str()
            .expect("display_name must be a string");
        assert!(
            display.contains("1M context"),
            "variant display_name must contain '1M context'; got: '{display}'"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 2 [offline]: no variant when cache says Some(false)
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // Input:  one model pair; cache returns `Some(false)` for its beta.
    // Expect: only the bare entry — no `[1m]` variant.
    #[test]
    fn no_variants_when_unsupported() {
        let pairs = &[("claude-opus-4-7", "us.anthropic.claude-opus-4-7")];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        let json = run(pairs, |_profile, _beta| Some(false), suffix_map);

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        assert!(
            ids.contains(&"claude-opus-4-7"),
            "bare entry must always be present; got ids: {ids:?}"
        );
        assert!(
            !ids.contains(&"claude-opus-4-7[1m]"),
            "[1m] variant must NOT be emitted when cache returns Some(false); got ids: {ids:?}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 3 [offline]: no variant when cache is absent (None)
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // Bootstrap window safety: when the cache has no entry for a (profile, beta)
    // pair, the helper must NOT emit a suffix variant. False positives are worse
    // than false negatives here — advertising a model variant that doesn't work
    // would break the user's workflow silently.
    //
    // Input:  one model pair; cache returns `None` for everything.
    // Expect: only the bare entry.
    #[test]
    fn no_variants_when_cache_absent() {
        let pairs = &[("claude-opus-4-7", "us.anthropic.claude-opus-4-7")];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        let json = run(
            pairs,
            |_profile, _beta| None, // cache empty — bootstrap window
            suffix_map,
        );

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        assert_eq!(
            ids.len(),
            1,
            "only the bare entry must appear when cache is absent (bootstrap window); got ids: {ids:?}"
        );
        assert!(
            ids.contains(&"claude-opus-4-7"),
            "bare entry must be present; got ids: {ids:?}"
        );
        assert!(
            !ids.contains(&"claude-opus-4-7[1m]"),
            "[1m] variant must NOT be emitted when cache returns None; got ids: {ids:?}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 4 [offline]: variants emitted per-model independently
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // Input:  two models — opus (1M-capable) and haiku (not capable).
    // Expect: 3 entries total: opus bare, opus[1m], haiku bare (no haiku[1m]).
    #[test]
    fn variants_for_subset_of_models() {
        let pairs = &[
            ("claude-opus-4-7", "us.anthropic.claude-opus-4-7"),
            (
                "claude-haiku-4-5-20251001",
                "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            ),
        ];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        // Build a lookup map: opus → true, haiku → false
        let mut cap_map: HashMap<(&str, &str), Option<bool>> = HashMap::new();
        cap_map.insert(
            ("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07"),
            Some(true),
        );
        cap_map.insert(
            (
                "us.anthropic.claude-haiku-4-5-20251001-v1:0",
                "context-1m-2025-08-07",
            ),
            Some(false),
        );

        let json = run(
            pairs,
            |profile, beta| cap_map.get(&(profile, beta)).copied().unwrap_or(None),
            suffix_map,
        );

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        assert_eq!(
            ids.len(),
            3,
            "expected 3 entries: opus bare + opus[1m] + haiku bare; got ids: {ids:?}"
        );
        assert!(
            ids.contains(&"claude-opus-4-7"),
            "opus bare must be present"
        );
        assert!(
            ids.contains(&"claude-opus-4-7[1m]"),
            "opus [1m] variant must be present"
        );
        assert!(
            ids.contains(&"claude-haiku-4-5-20251001"),
            "haiku bare must be present"
        );
        assert!(
            !ids.contains(&"claude-haiku-4-5-20251001[1m]"),
            "haiku [1m] variant must NOT be present (cache says false)"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 5 [offline]: bare entries appear before any suffix variants
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // Sort contract: all bare entries must appear before any `[...]` suffix
    // variant, and within each group entries must be alphabetically ordered.
    //
    // Input:  two 1M-capable models.
    // Expect: sorted as [claude-haiku-4-5-..., claude-opus-4-7, claude-haiku-4-5-...[1m], claude-opus-4-7[1m]]
    #[test]
    fn bare_entries_first_then_variants() {
        let pairs = &[
            ("claude-opus-4-7", "us.anthropic.claude-opus-4-7"),
            (
                "claude-haiku-4-5-20251001",
                "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            ),
        ];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        let json = run(
            pairs,
            |_profile, _beta| Some(true), // all models are 1M-capable
            suffix_map,
        );

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        // All bare entries must appear before any variant entry
        let first_variant_pos = ids.iter().position(|id| id.contains('['));
        let last_bare_pos = ids.iter().rposition(|id| !id.contains('['));

        assert!(
            first_variant_pos.is_some(),
            "at least one variant must be present in output; got ids: {ids:?}"
        );
        assert!(
            last_bare_pos.is_some(),
            "at least one bare entry must be present in output; got ids: {ids:?}"
        );

        assert!(
            last_bare_pos.unwrap() < first_variant_pos.unwrap(),
            "all bare entries must precede all suffix variant entries in the sorted output; \
             got ordering: {ids:?}"
        );

        // Alphabetic within bare group
        let bare_ids: Vec<&str> = ids.iter().copied().filter(|id| !id.contains('[')).collect();
        let mut sorted_bare = bare_ids.clone();
        sorted_bare.sort_unstable();
        assert_eq!(
            bare_ids, sorted_bare,
            "bare entries must be alphabetically sorted within their group; got: {bare_ids:?}"
        );

        // Alphabetic within variant group
        let variant_ids: Vec<&str> = ids.iter().copied().filter(|id| id.contains('[')).collect();
        let mut sorted_variants = variant_ids.clone();
        sorted_variants.sort_unstable();
        assert_eq!(
            variant_ids, sorted_variants,
            "suffix variant entries must be alphabetically sorted within their group; got: {variant_ids:?}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 6 [offline]: variant display name uses display_suffix from suffix_map
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // The display name of a variant entry must be "<existing display> (display_suffix)"
    // where `display_suffix` comes from the third tuple element of the `suffix_map`
    // parameter — it is NOT hardcoded as "1M context" in the helper body.
    //
    // This test uses a synthetic suffix_map entry with a distinct display label to
    // prove the label flows from the map, not from a hardcoded string.
    #[test]
    fn display_name_includes_capability_suffix() {
        let pairs = &[("claude-opus-4-7", "us.anthropic.claude-opus-4-7")];

        // Use a recognisably different display suffix to confirm it comes from the map.
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        let json = run(pairs, |_profile, _beta| Some(true), suffix_map);

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let variant = data
            .iter()
            .find(|m| m["id"].as_str() == Some("claude-opus-4-7[1m]"))
            .expect("[1m] variant entry must be present");

        let display = variant["display_name"]
            .as_str()
            .expect("display_name must be a string");

        // Bare display name for claude-opus-4-7 (from model_display_name)
        let bare_display = model_display_name("claude-opus-4-7");

        let expected = format!("{} (1M context)", bare_display);
        assert_eq!(
            display, expected,
            "variant display_name must be '<base display> (display_suffix)'; got: '{display}'"
        );

        // Now confirm the label flows from suffix_map by using a different map entry.
        let custom_suffix_map: &[(&str, &str, &str)] =
            &[("[1m]", "context-1m-2025-08-07", "Extended Context")];

        let json2 = run(pairs, |_profile, _beta| Some(true), custom_suffix_map);

        let data2 = json2["data"].as_array().unwrap();
        let variant2 = data2
            .iter()
            .find(|m| m["id"].as_str() == Some("claude-opus-4-7[1m]"))
            .expect("[1m] variant must be present with custom suffix map");

        let display2 = variant2["display_name"].as_str().unwrap();
        let expected2 = format!("{} (Extended Context)", bare_display);
        assert_eq!(
            display2, expected2,
            "variant display_name must use the display_suffix from the suffix_map parameter, \
             not a hardcoded string; got: '{display2}'"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 7 [offline]: same Anthropic ID from multiple profiles → no duplicates
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // Mirrors the existing `seen.insert(id.clone())` dedup logic.
    // Input: the same Anthropic model backed by two different Bedrock profile IDs
    //        (e.g. us. and global. prefixes). Both profiles say Some(true).
    // Expect: exactly ONE bare entry and ONE [1m] variant — deduplicated by Anthropic ID.
    #[test]
    fn data_has_no_duplicates_when_same_anthropic_id_from_multiple_profiles() {
        let pairs = &[
            ("claude-opus-4-7", "us.anthropic.claude-opus-4-7"),
            ("claude-opus-4-7", "global.anthropic.claude-opus-4-7"),
        ];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        let json = run(
            pairs,
            |_profile, _beta| Some(true), // both profiles say supported
            suffix_map,
        );

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        let bare_count = ids.iter().filter(|&&id| id == "claude-opus-4-7").count();
        let variant_count = ids
            .iter()
            .filter(|&&id| id == "claude-opus-4-7[1m]")
            .count();

        assert_eq!(
            bare_count, 1,
            "claude-opus-4-7 bare entry must appear exactly once even when backed by multiple profiles; \
             got {bare_count} copies; ids: {ids:?}"
        );
        assert_eq!(
            variant_count, 1,
            "claude-opus-4-7[1m] variant must appear exactly once even when multiple profiles support it; \
             got {variant_count} copies; ids: {ids:?}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 8 [offline]: multiple suffix_map entries emit independent variants
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // Confirms that `suffix_map` is the single source of truth for which variants
    // to advertise. By injecting a two-entry synthetic slice we verify both
    // suffix entries are independently processed.
    //
    // NOTE: This test is only possible because the helper accepts `suffix_map` as
    // an explicit parameter rather than reading `crate::endpoint::SUFFIX_BETA_MAP`
    // directly. If the Builder implements the helper to read the const directly,
    // this test cannot be written without modifying the const — which would be
    // a design problem. The parameter approach is the correct design.
    #[test]
    fn multiple_suffix_map_entries_emit_independent_variants() {
        let pairs = &[("claude-opus-4-7", "us.anthropic.claude-opus-4-7")];

        // Synthetic 2-entry suffix map — NOT production constants.
        let synthetic_suffix_map: &[(&str, &str, &str)] = &[
            ("[1m]", "context-1m-2025-08-07", "1M context"),
            ("[2m]", "context-2m-2025-99-99", "2M context"), // hypothetical future beta
        ];

        // Both betas supported
        let json = run(pairs, |_profile, _beta| Some(true), synthetic_suffix_map);

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        assert!(
            ids.contains(&"claude-opus-4-7"),
            "bare entry must be present"
        );
        assert!(
            ids.contains(&"claude-opus-4-7[1m]"),
            "[1m] variant must be emitted for first suffix map entry; got ids: {ids:?}"
        );
        assert!(
            ids.contains(&"claude-opus-4-7[2m]"),
            "[2m] variant must be emitted for second suffix map entry; got ids: {ids:?}"
        );

        // Only first beta supported → only [1m] variant, not [2m]
        let json2 = run(
            pairs,
            |_profile, beta| {
                if beta == "context-1m-2025-08-07" {
                    Some(true)
                } else {
                    Some(false)
                }
            },
            synthetic_suffix_map,
        );

        let data2 = json2["data"].as_array().unwrap();
        let ids2: Vec<&str> = data2.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert!(
            ids2.contains(&"claude-opus-4-7[1m]"),
            "[1m] must appear when its beta is true"
        );
        assert!(
            !ids2.contains(&"claude-opus-4-7[2m]"),
            "[2m] must NOT appear when its beta is false; got ids: {ids2:?}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 9 [offline]: first_id and last_id reflect emitted data after sort
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // The response envelope fields `first_id` and `last_id` must equal
    // `data[0].id` and `data[data.len()-1].id` after variant emission and sort.
    #[test]
    fn first_id_and_last_id_reflect_emitted_data() {
        let pairs = &[
            ("claude-opus-4-7", "us.anthropic.claude-opus-4-7"),
            (
                "claude-haiku-4-5-20251001",
                "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            ),
        ];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        // Opus is 1M-capable; haiku is not.
        let json = run(
            pairs,
            |profile, beta| {
                if profile == "us.anthropic.claude-opus-4-7" && beta == "context-1m-2025-08-07" {
                    Some(true)
                } else {
                    Some(false)
                }
            },
            suffix_map,
        );

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        assert!(
            !data.is_empty(),
            "data must be non-empty for this test to be meaningful"
        );

        let first_id_in_data = data.first().unwrap()["id"].as_str().unwrap();
        let last_id_in_data = data.last().unwrap()["id"].as_str().unwrap();

        assert_eq!(
            json["first_id"].as_str(),
            Some(first_id_in_data),
            "first_id in envelope must equal the id of the first entry in data array; \
             envelope says '{:?}', data[0].id is '{first_id_in_data}'",
            json["first_id"]
        );
        assert_eq!(
            json["last_id"].as_str(),
            Some(last_id_in_data),
            "last_id in envelope must equal the id of the last entry in data array; \
             envelope says '{:?}', data[last].id is '{last_id_in_data}'",
            json["last_id"]
        );
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Test 10 [offline]: empty input returns empty response
    // ─────────────────────────────────────────────────────────────────────────────
    //
    // When `pairs` is empty (bootstrap window or no endpoints configured),
    // the helper must return the same empty-list envelope as before.
    // This verifies that the variant-emission logic does not break the empty case.
    #[test]
    fn empty_input_returns_empty_response() {
        let pairs: &[(&str, &str)] = &[];
        let suffix_map: &[(&str, &str, &str)] = &[("[1m]", "context-1m-2025-08-07", "1M context")];

        let json = run(pairs, |_profile, _beta| Some(true), suffix_map);

        let data = json["data"]
            .as_array()
            .expect("response must have 'data' array");
        assert!(
            data.is_empty(),
            "empty input must produce empty 'data' array; got: {data:?}"
        );

        assert_eq!(
            json["has_more"], false,
            "has_more must be false for empty response"
        );
        assert!(
            json["first_id"].is_null(),
            "first_id must be null for empty response; got: {:?}",
            json["first_id"]
        );
        assert!(
            json["last_id"].is_null(),
            "last_id must be null for empty response; got: {:?}",
            json["last_id"]
        );
    }
}
