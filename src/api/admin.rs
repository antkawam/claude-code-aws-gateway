use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::CachedKey;
use crate::db;
use crate::proxy::GatewayState;

/// Format an AWS SDK error into a clean, user-facing message.
/// Extracts the service error code + message when available; falls back to
/// a human-readable description for dispatch/credentials failures.
fn aws_err<E>(e: &aws_sdk_bedrock::error::SdkError<E>) -> String
where
    E: aws_sdk_bedrock::error::ProvideErrorMetadata + std::fmt::Debug,
{
    use aws_sdk_bedrock::error::ProvideErrorMetadata;
    if let Some(code) = e.code() {
        let msg = e.message().unwrap_or("no details");
        return format!("{code}: {msg}");
    }
    let debug = format!("{e:?}");
    if debug.contains("CredentialsNotLoaded")
        || debug.contains("NoCredentials")
        || debug.contains("InvalidClientTokenId")
    {
        "CredentialsError: no valid credentials — check role ARN and trust policy".to_string()
    } else if debug.contains("DispatchFailure") || debug.contains("ConnectorError") {
        "DispatchFailure: connection error — check region and network access".to_string()
    } else {
        debug
    }
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub name: Option<String>,
    pub user_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub rate_limit_rpm: Option<i32>,
}

pub async fn create_key(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<CreateKeyRequest>,
) -> Response {
    let (_, role, caller_user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    let pool = state.db().await;
    let pool = &pool;

    // Members always own the key they create; admins can assign to any user
    let effective_user_id = if role == "admin" {
        body.user_id.or(caller_user_id)
    } else {
        caller_user_id
    };

    // Auto-resolve team_id from user if not explicitly provided
    let effective_team_id = if body.team_id.is_some() {
        body.team_id
    } else if let Some(uid) = effective_user_id {
        sqlx::query_scalar::<_, Option<Uuid>>("SELECT team_id FROM users WHERE id = $1")
            .bind(uid)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten()
    } else {
        None
    };

    match db::keys::create_key(
        pool,
        body.name.as_deref(),
        effective_user_id,
        effective_team_id,
        body.rate_limit_rpm,
    )
    .await
    {
        Ok((raw_key, vk)) => {
            // Update in-memory cache
            state
                .key_cache
                .insert(
                    vk.key_hash.clone(),
                    CachedKey {
                        id: vk.id,
                        name: vk.name.clone(),
                        user_id: vk.user_id,
                        team_id: vk.team_id,
                        rate_limit_rpm: vk.rate_limit_rpm,
                    },
                )
                .await;

            tracing::info!(key_id = %vk.id, prefix = %vk.key_prefix, "Created virtual key");

            (
                StatusCode::CREATED,
                Json(json!({
                    "key": raw_key,
                    "id": vk.id,
                    "prefix": vk.key_prefix,
                    "name": vk.name,
                    "created_at": vk.created_at,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(%e, "Failed to create key");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

pub async fn list_keys(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    let (_, role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    let pool = state.db().await;
    let pool = &pool;

    match db::keys::list_keys(pool).await {
        Ok(keys) => {
            // Members only see their own keys; admins see all
            let filtered: Vec<_> = if role == "admin" {
                keys
            } else {
                keys.into_iter().filter(|k| k.user_id == user_id).collect()
            };
            let keys_json: Vec<_> = filtered
                .iter()
                .map(|k| {
                    json!({
                        "id": k.id,
                        "prefix": k.key_prefix,
                        "name": k.name,
                        "user_id": k.user_id,
                        "is_active": k.is_active,
                        "rate_limit_rpm": k.rate_limit_rpm,
                        "created_at": k.created_at,
                        "expires_at": k.expires_at,
                    })
                })
                .collect();
            Json(json!({ "keys": keys_json })).into_response()
        }
        Err(e) => {
            tracing::error!(%e, "Failed to list keys");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

pub async fn revoke_key(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> Response {
    let (_, role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    let pool = state.db().await;
    let pool = &pool;

    // Members can only revoke their own keys
    if role != "admin"
        && let Err(resp) = verify_key_ownership(pool, key_id, user_id).await
    {
        return resp;
    }

    match db::keys::revoke_key(pool, key_id).await {
        Ok(true) => {
            if let Err(e) = state.key_cache.load_from_db(pool).await {
                tracing::warn!(%e, "Failed to reload key cache after revocation");
            }
            tracing::info!(%key_id, "Revoked virtual key");
            Json(json!({ "revoked": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "Key not found"),
        Err(e) => {
            tracing::error!(%e, "Failed to revoke key");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

pub async fn delete_key(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> Response {
    let (_, role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    let pool = state.db().await;
    let pool = &pool;

    // Members can only delete their own keys
    if role != "admin"
        && let Err(resp) = verify_key_ownership(pool, key_id, user_id).await
    {
        return resp;
    }

    match db::keys::delete_key(pool, key_id).await {
        Ok(true) => {
            if let Err(e) = state.key_cache.load_from_db(pool).await {
                tracing::warn!(%e, "Failed to reload key cache after deletion");
            }
            tracing::info!(%key_id, "Deleted virtual key");
            Json(json!({ "deleted": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "Key not found"),
        Err(e) => {
            tracing::error!(%e, "Failed to delete key");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// Verify that a key belongs to the given user_id.
async fn verify_key_ownership(
    pool: &sqlx::PgPool,
    key_id: Uuid,
    user_id: Option<Uuid>,
) -> Result<(), Response> {
    match db::keys::list_keys(pool).await {
        Ok(keys) => {
            if let Some(key) = keys.iter().find(|k| k.id == key_id)
                && key.user_id == user_id
            {
                return Ok(());
            }
            Err(error_response(StatusCode::NOT_FOUND, "Key not found"))
        }
        Err(e) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
        )),
    }
}

// --- Teams ---

#[derive(Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
}

pub async fn create_team(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<CreateTeamRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::teams::create_team(pool, &body.name).await {
        Ok(team) => (StatusCode::CREATED, Json(json!(team))).into_response(),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn list_teams(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::teams::list_teams(pool).await {
        Ok(teams) => Json(json!({ "teams": teams })).into_response(),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn delete_team(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(team_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::teams::delete_team(pool, team_id).await {
        Ok(true) => Json(json!({ "deleted": true })).into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "Team not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// --- Users ---

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub team_id: Option<Uuid>,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "member".to_string()
}

pub async fn create_user(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::users::create_user(pool, &body.email, body.team_id, &body.role).await {
        Ok(user) => (StatusCode::CREATED, Json(json!(user))).into_response(),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn list_users(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::users::list_users(pool).await {
        Ok(users) => Json(json!({ "users": users })).into_response(),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub role: String,
}

#[derive(Deserialize)]
pub struct UpdateUserTeamRequest {
    pub team_id: Option<Uuid>,
}

pub async fn update_user(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateUserRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    if body.role != "admin" && body.role != "member" {
        return error_response(StatusCode::BAD_REQUEST, "Role must be 'admin' or 'member'");
    }
    match db::users::update_user_role(pool, user_id, &body.role).await {
        Ok(true) => {
            tracing::info!(%user_id, role = %body.role, "Updated user role");
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn delete_user(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::users::delete_user(pool, user_id).await {
        Ok(true) => Json(json!({ "deleted": true })).into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn update_user_team(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateUserTeamRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::users::update_user_team(pool, user_id, body.team_id).await {
        Ok(true) => {
            tracing::info!(%user_id, team_id = ?body.team_id, "Updated user team");
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// --- Team Budget Management ---

#[derive(Deserialize)]
pub struct UpdateTeamBudgetRequest {
    pub budget_amount_usd: Option<f64>,
    #[serde(default = "default_monthly")]
    pub budget_period: String,
    /// Policy preset name ("standard", "soft", "shaped") or custom rules.
    pub budget_policy: Option<serde_json::Value>,
    pub default_user_budget_usd: Option<f64>,
    #[serde(default = "default_both")]
    pub notify_recipients: String,
}

fn default_monthly() -> String {
    "monthly".to_string()
}
fn default_both() -> String {
    "both".to_string()
}

/// Resolve a budget_policy value: preset name string → rules JSON, or validate custom array.
#[allow(clippy::result_large_err)]
fn resolve_policy_json(
    value: &Option<serde_json::Value>,
) -> Result<Option<serde_json::Value>, Response> {
    match value {
        Some(v) if v.is_string() => {
            let preset_name = v.as_str().unwrap();
            let rules = match preset_name {
                "standard" => crate::budget::preset_standard(),
                "soft" => crate::budget::preset_soft(),
                "shaped" => crate::budget::preset_shaped(),
                _ => {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "Unknown preset. Use 'standard', 'soft', 'shaped', or provide custom rules.",
                    ));
                }
            };
            Ok(Some(serde_json::to_value(rules).unwrap()))
        }
        Some(v) if v.is_array() => {
            match serde_json::from_value::<Vec<crate::budget::PolicyRule>>(v.clone()) {
                Ok(rules) => {
                    if let Err(e) = crate::budget::validate_policy(&rules) {
                        return Err(error_response(StatusCode::BAD_REQUEST, &e));
                    }
                    Ok(Some(v.clone()))
                }
                Err(e) => Err(error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid policy rules: {}", e),
                )),
            }
        }
        _ => Ok(None),
    }
}

pub async fn update_team_budget(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(team_id): Path<Uuid>,
    Json(body): Json<UpdateTeamBudgetRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    // Validate period
    if !["daily", "weekly", "monthly"].contains(&body.budget_period.as_str()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "budget_period must be 'daily', 'weekly', or 'monthly'",
        );
    }

    // Validate notify_recipients
    if !["both", "user", "admin"].contains(&body.notify_recipients.as_str()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "notify_recipients must be 'both', 'user', or 'admin'",
        );
    }

    let policy_json = match resolve_policy_json(&body.budget_policy) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    match db::teams::update_team_budget(
        pool,
        team_id,
        body.budget_amount_usd,
        &body.budget_period,
        policy_json,
        body.default_user_budget_usd,
        &body.notify_recipients,
    )
    .await
    {
        Ok(true) => {
            tracing::info!(
                %team_id,
                budget = ?body.budget_amount_usd,
                period = %body.budget_period,
                "Updated team budget"
            );
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "Team not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn get_team_analytics(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(team_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    let team = match db::teams::get_team(pool, team_id).await {
        Ok(Some(t)) => t,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "Team not found"),
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let users = match db::budget::get_team_analytics(pool, team_id).await {
        Ok(u) => u,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    Json(json!({
        "team": team,
        "users": users,
    }))
    .into_response()
}

// --- Default Budget Policy ---

pub async fn get_default_budget(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    let amount = db::settings::get_setting(pool, "default_budget_usd")
        .await
        .ok()
        .flatten()
        .and_then(|s| s.parse::<f64>().ok());
    let period = db::settings::get_setting(pool, "default_budget_period")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "monthly".to_string());
    let policy: Option<serde_json::Value> =
        db::settings::get_setting(pool, "default_budget_policy")
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok());
    let notify = db::settings::get_setting(pool, "default_budget_notify")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "both".to_string());
    let affected = db::users::count_users_without_team(pool).await.unwrap_or(0);

    Json(json!({
        "budget_amount_usd": amount,
        "budget_period": period,
        "budget_policy": policy,
        "notify_recipients": notify,
        "affected_user_count": affected,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct UpdateDefaultBudgetRequest {
    pub budget_amount_usd: Option<f64>,
    #[serde(default = "default_monthly")]
    pub budget_period: String,
    pub budget_policy: Option<serde_json::Value>,
    #[serde(default = "default_both")]
    pub notify_recipients: String,
}

pub async fn update_default_budget(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<UpdateDefaultBudgetRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    if !["daily", "weekly", "monthly"].contains(&body.budget_period.as_str()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "budget_period must be 'daily', 'weekly', or 'monthly'",
        );
    }
    if !["both", "user", "admin"].contains(&body.notify_recipients.as_str()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "notify_recipients must be 'both', 'user', or 'admin'",
        );
    }

    let policy_json = match resolve_policy_json(&body.budget_policy) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    // Store each field in proxy_settings
    let amount_str = body
        .budget_amount_usd
        .map(|a| a.to_string())
        .unwrap_or_default();
    let policy_str = policy_json
        .map(|v| serde_json::to_string(&v).unwrap())
        .unwrap_or_default();

    let results = futures::future::join_all(vec![
        db::settings::set_setting(pool, "default_budget_usd", &amount_str),
        db::settings::set_setting(pool, "default_budget_period", &body.budget_period),
        db::settings::set_setting(pool, "default_budget_policy", &policy_str),
        db::settings::set_setting(pool, "default_budget_notify", &body.notify_recipients),
    ])
    .await;

    if let Some(err) = results.into_iter().find_map(|r| r.err()) {
        return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }

    tracing::info!(
        budget = ?body.budget_amount_usd,
        period = %body.budget_period,
        "Updated default budget policy"
    );
    Json(json!({ "updated": true })).into_response()
}

pub async fn get_analytics_overview(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    match db::budget::get_analytics_overview(pool).await {
        Ok(teams) => Json(json!({ "teams": teams })).into_response(),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn get_budget_status(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    let (identity, _, _) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let pool = state.db().await;
    let pool = &pool;

    let user = match db::users::get_user_by_email(pool, &identity).await {
        Ok(Some(u)) => u,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    // Resolve effective limit: user > team default > global default
    let team = if let Some(tid) = user.team_id {
        db::teams::get_team(pool, tid).await.ok().flatten()
    } else {
        None
    };

    let global_default = if user.spend_limit_monthly_usd.is_none()
        && team
            .as_ref()
            .and_then(|t| t.default_user_budget_usd)
            .is_none()
    {
        crate::api::handlers::load_default_budget(pool).await
    } else {
        None
    };

    let (effective_limit, effective_period, limit_source, limit_source_name) =
        if let Some(limit) = user.spend_limit_monthly_usd {
            let period = crate::budget::BudgetPeriod::parse(&user.budget_period);
            (Some(limit), period, "explicit", None)
        } else if let Some(ref t) = team {
            if let Some(limit) = t.default_user_budget_usd {
                let period = crate::budget::BudgetPeriod::parse(&t.budget_period);
                (Some(limit), period, "team", Some(t.name.clone()))
            } else if let Some(ref gd) = global_default {
                (Some(gd.0), gd.1, "default", None)
            } else {
                let period = crate::budget::BudgetPeriod::parse(&user.budget_period);
                (None, period, "unlimited", None)
            }
        } else if let Some(ref gd) = global_default {
            (Some(gd.0), gd.1, "default", None)
        } else {
            let period = crate::budget::BudgetPeriod::parse(&user.budget_period);
            (None, period, "unlimited", None)
        };

    let spend = db::budget::get_user_spend(pool, &identity, effective_period)
        .await
        .unwrap_or(0.0);

    // Resolve policy: team > global default > preset standard
    let policy: Vec<crate::budget::PolicyRule> = team
        .as_ref()
        .and_then(|t| t.budget_policy.as_ref())
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .or_else(|| global_default.as_ref().and_then(|gd| gd.2.clone()))
        .unwrap_or_else(crate::budget::preset_standard);

    // Team-level budget consumption
    let team_budget = if let Some(ref t) = team {
        if let Some(team_limit) = t.budget_amount_usd {
            let team_period = crate::budget::BudgetPeriod::parse(&t.budget_period);
            let team_spend = db::budget::get_team_spend(pool, t.id, team_period)
                .await
                .unwrap_or(0.0);
            let team_pct = if team_limit > 0.0 {
                (team_spend / team_limit * 100.0 * 10.0).round() / 10.0
            } else {
                0.0
            };
            Some(json!({
                "spend_usd": (team_spend * 100.0).round() / 100.0,
                "percent": team_pct,
            }))
        } else {
            None
        }
    } else {
        None
    };

    let recent_events = db::budget::get_user_events(pool, &identity, 10)
        .await
        .unwrap_or_default();

    let mut team_json = team.as_ref().map(|t| {
        json!({
            "id": t.id,
            "name": t.name,
            "budget_amount_usd": t.budget_amount_usd,
            "budget_period": t.budget_period,
        })
    });
    // Merge team budget spend/percent into team object
    if let (Some(tj), Some(tb)) = (&mut team_json, team_budget)
        && let Some(obj) = tj.as_object_mut()
    {
        obj.insert("spend_usd".to_string(), tb["spend_usd"].clone());
        obj.insert("percent".to_string(), tb["percent"].clone());
    }

    Json(json!({
        "user": identity,
        "period": effective_period.as_str(),
        "period_start": effective_period.period_start().to_rfc3339(),
        "spend_usd": (spend * 100.0).round() / 100.0,
        "limit_usd": effective_limit,
        "percent": effective_limit.map(|l| if l > 0.0 { (spend / l * 100.0 * 10.0).round() / 10.0 } else { 0.0 }),
        "limit_source": limit_source,
        "limit_source_name": limit_source_name,
        "policy": policy,
        "team": team_json,
        "recent_events": recent_events,
    }))
    .into_response()
}

pub async fn export_analytics_csv(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<ExportQueryParams>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    let days = params.days.unwrap_or(30);
    let rows = match db::budget::get_spend_export(pool, days).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let mut csv = String::from(
        "recorded_at,user,team,model,input_tokens,output_tokens,cache_read,cache_write,cost_usd,duration_ms\n",
    );
    for row in &rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{:.4},{}\n",
            row.recorded_at.to_rfc3339(),
            row.user_identity.as_deref().unwrap_or(""),
            row.team_name.as_deref().unwrap_or(""),
            row.model,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.cost_usd.unwrap_or(0.0),
            row.duration_ms,
        ));
    }

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/csv")
        .header(
            "content-disposition",
            format!("attachment; filename=\"ccag-spend-{}d.csv\"", days),
        )
        .body(axum::body::Body::from(csv))
        .unwrap()
}

#[derive(Deserialize)]
pub struct ExportQueryParams {
    pub days: Option<i32>,
}

// --- Spend (analytics only — detailed spend removed; dashboard covers it) ---

/// Update a user's monthly spend limit.
pub async fn update_user_spend_limit(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateSpendLimitRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    match sqlx::query("UPDATE users SET spend_limit_monthly_usd = $1 WHERE id = $2")
        .bind(body.limit_usd)
        .bind(user_id)
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::info!(%user_id, limit = ?body.limit_usd, "Updated user spend limit");
            Json(json!({ "updated": true })).into_response()
        }
        Ok(_) => error_response(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Get request analytics summary. Always scoped to the logged-in user.
pub async fn get_analytics(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<AnalyticsQueryParams>,
) -> Response {
    let (admin_identity, _, _) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let pool = state.db().await;
    let pool = &pool;

    let days = params.days.unwrap_or(7);
    let days_str = days.to_string();

    // Determine if we use absolute date range or relative days
    let use_date_range = params.from.is_some() && params.to.is_some();
    let from_str = params.from.unwrap_or_default();
    let to_str = params.to.unwrap_or_default();

    // Build time clause: $1 (and $2 for date range), user is always the param after
    let (time_clause, user_param) = if use_date_range {
        (
            "recorded_at >= $1::timestamptz AND recorded_at < ($2::timestamptz + interval '1 day')"
                .to_string(),
            "$3",
        )
    } else {
        (
            "recorded_at >= now() - ($1 || ' days')::interval".to_string(),
            "$2",
        )
    };
    let user_clause = format!("AND user_identity = {user_param}");

    // Resolve granularity: auto, hour, day, week
    let granularity = match params.granularity.as_deref().unwrap_or("auto") {
        "hour" => "hour",
        "day" => "day",
        "week" => "week",
        _ => {
            if days <= 3 {
                "hour"
            } else if days <= 90 {
                "day"
            } else {
                "week"
            }
        }
    };

    // Aggregate totals
    let totals_query = format!(
        r#"SELECT
            COUNT(*)::bigint as total_requests,
            SUM(input_tokens)::bigint as total_input_tokens,
            SUM(output_tokens)::bigint as total_output_tokens,
            SUM(cache_read_tokens)::bigint as total_cache_read_tokens,
            SUM(cache_write_tokens)::bigint as total_cache_write_tokens,
            AVG(duration_ms)::integer as avg_duration_ms,
            SUM(CASE WHEN streaming THEN 1 ELSE 0 END)::bigint as streaming_requests,
            SUM(CASE WHEN thinking_enabled THEN 1 ELSE 0 END)::bigint as thinking_requests,
            SUM(tool_count)::bigint as total_tool_calls,
            COUNT(DISTINCT user_identity)::bigint as unique_users,
            SUM(estimate_cost_usd(model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens))::float8 as total_cost_usd,
            SUM(estimate_cost_usd(model, cache_read_tokens, 0, 0, 0)
              - estimate_cost_usd(model, 0, 0, cache_read_tokens, 0))::float8 as cache_savings_usd
        FROM spend_log
        WHERE {time_clause} {user_clause}"#,
    );
    let totals = if use_date_range {
        sqlx::query_as::<_, AnalyticsTotalsRow>(&totals_query)
            .bind(&from_str)
            .bind(&to_str)
            .bind(&admin_identity)
            .fetch_optional(pool)
            .await
    } else {
        sqlx::query_as::<_, AnalyticsTotalsRow>(&totals_query)
            .bind(&days_str)
            .bind(&admin_identity)
            .fetch_optional(pool)
            .await
    };

    // Top tools
    let tools_query = format!(
        r#"SELECT tool_name, COUNT(*)::bigint as usage_count
        FROM spend_log, UNNEST(tool_names) as tool_name
        WHERE {time_clause} {user_clause}
        GROUP BY tool_name ORDER BY usage_count DESC LIMIT 20"#,
    );
    let top_tools = if use_date_range {
        sqlx::query_as::<_, (String, Option<i64>)>(&tools_query)
            .bind(&from_str)
            .bind(&to_str)
            .bind(&admin_identity)
            .fetch_all(pool)
            .await
    } else {
        sqlx::query_as::<_, (String, Option<i64>)>(&tools_query)
            .bind(&days_str)
            .bind(&admin_identity)
            .fetch_all(pool)
            .await
    };

    // Model breakdown
    let models_query = format!(
        r#"SELECT
            model,
            COUNT(*)::bigint as request_count,
            SUM(input_tokens)::bigint as total_input,
            SUM(output_tokens)::bigint as total_output,
            SUM(cache_read_tokens)::bigint as total_cache_read,
            SUM(cache_write_tokens)::bigint as total_cache_write,
            AVG(duration_ms)::integer as avg_duration_ms,
            SUM(CASE WHEN streaming THEN 1 ELSE 0 END)::bigint as streaming_count,
            SUM(estimate_cost_usd(model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens))::float8 as cost_usd
        FROM spend_log
        WHERE {time_clause} {user_clause}
        GROUP BY model ORDER BY cost_usd DESC NULLS LAST"#,
    );
    let model_stats = if use_date_range {
        sqlx::query_as::<_, ModelStatsExtRow>(&models_query)
            .bind(&from_str)
            .bind(&to_str)
            .bind(&admin_identity)
            .fetch_all(pool)
            .await
    } else {
        sqlx::query_as::<_, ModelStatsExtRow>(&models_query)
            .bind(&days_str)
            .bind(&admin_identity)
            .fetch_all(pool)
            .await
    };

    // Time-series trends (granularity-aware)
    let trend_fmt = match granularity {
        "hour" => "YYYY-MM-DD\"T\"HH24:00:00",
        "week" => "IYYY-\"W\"IW",
        _ => "YYYY-MM-DD",
    };
    let trend_query = format!(
        r#"SELECT
            to_char(date_trunc('{granularity}', recorded_at), '{trend_fmt}') as bucket,
            COUNT(*)::bigint as request_count,
            SUM(input_tokens + output_tokens + cache_read_tokens + cache_write_tokens)::bigint as total_tokens,
            SUM(estimate_cost_usd(model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens))::float8 as cost_usd
        FROM spend_log
        WHERE {time_clause} {user_clause}
        GROUP BY date_trunc('{granularity}', recorded_at) ORDER BY bucket"#,
    );
    let trend = if use_date_range {
        sqlx::query_as::<_, TrendRow>(&trend_query)
            .bind(&from_str)
            .bind(&to_str)
            .bind(&admin_identity)
            .fetch_all(pool)
            .await
    } else {
        sqlx::query_as::<_, TrendRow>(&trend_query)
            .bind(&days_str)
            .bind(&admin_identity)
            .fetch_all(pool)
            .await
    };

    // Build response
    let t = match totals {
        Ok(row) => row,
        Err(e) => {
            tracing::error!(%e, "Analytics totals query failed");
            None
        }
    };
    let total_input = t.as_ref().and_then(|t| t.total_input_tokens).unwrap_or(0);
    let total_cache_read = t
        .as_ref()
        .and_then(|t| t.total_cache_read_tokens)
        .unwrap_or(0);
    let cache_hit_rate = if (total_input + total_cache_read) > 0 {
        (total_cache_read as f64 / (total_input + total_cache_read) as f64 * 100.0 * 10.0).round()
            / 10.0
    } else {
        0.0
    };

    let tools_json: Vec<_> = match top_tools {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(%e, "Analytics top_tools query failed");
            vec![]
        }
    }
    .iter()
    .map(|(name, count)| {
        json!({
            "name": name,
            "count": count,
            "is_mcp": name.starts_with("mcp__"),
        })
    })
    .collect();

    let models_json: Vec<_> = match model_stats {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(%e, "Analytics model_stats query failed");
            vec![]
        }
    }
    .iter()
    .map(|r| {
        json!({
            "model": r.model,
            "request_count": r.request_count,
            "total_input": r.total_input,
            "total_output": r.total_output,
            "total_cache_read": r.total_cache_read,
            "total_cache_write": r.total_cache_write,
            "avg_duration_ms": r.avg_duration_ms,
            "streaming_count": r.streaming_count,
            "cost_usd": r.cost_usd.map(|v| (v * 100.0).round() / 100.0),
        })
    })
    .collect();

    let trend_json: Vec<_> = match trend {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(%e, "Analytics trend query failed");
            vec![]
        }
    }
    .iter()
    .map(|r| {
        json!({
            "bucket": r.bucket,
            "requests": r.request_count,
            "tokens": r.total_tokens,
            "cost_usd": r.cost_usd.map(|v| (v * 10000.0).round() / 10000.0),
        })
    })
    .collect();

    Json(json!({
        "days": days,
        "granularity": granularity,
        "scope": "user",
        "user": admin_identity,
        "totals": {
            "requests": t.as_ref().and_then(|t| t.total_requests).unwrap_or(0),
            "input_tokens": total_input,
            "output_tokens": t.as_ref().and_then(|t| t.total_output_tokens).unwrap_or(0),
            "cache_read_tokens": total_cache_read,
            "cache_write_tokens": t.as_ref().and_then(|t| t.total_cache_write_tokens).unwrap_or(0),
            "cache_hit_rate_pct": cache_hit_rate,
            "avg_duration_ms": t.as_ref().and_then(|t| t.avg_duration_ms).unwrap_or(0),
            "streaming_requests": t.as_ref().and_then(|t| t.streaming_requests).unwrap_or(0),
            "thinking_requests": t.as_ref().and_then(|t| t.thinking_requests).unwrap_or(0),
            "total_tool_calls": t.as_ref().and_then(|t| t.total_tool_calls).unwrap_or(0),
            "unique_users": t.as_ref().and_then(|t| t.unique_users).unwrap_or(0),
            "total_cost_usd": t.as_ref().and_then(|t| t.total_cost_usd).map(|v| (v * 100.0).round() / 100.0).unwrap_or(0.0),
            "cache_savings_usd": t.as_ref().and_then(|t| t.cache_savings_usd).map(|v| (v * 100.0).round() / 100.0).unwrap_or(0.0),
        },
        "top_tools": tools_json,
        "models": models_json,
        "trend": trend_json,
        // Keep "daily" as alias for backward compat
        "daily": trend_json,
    })).into_response()
}

#[derive(Deserialize)]
pub struct AnalyticsQueryParams {
    pub days: Option<i32>,
    pub granularity: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateSpendLimitRequest {
    pub limit_usd: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct AnalyticsTotalsRow {
    total_requests: Option<i64>,
    total_input_tokens: Option<i64>,
    total_output_tokens: Option<i64>,
    total_cache_read_tokens: Option<i64>,
    total_cache_write_tokens: Option<i64>,
    avg_duration_ms: Option<i32>,
    streaming_requests: Option<i64>,
    thinking_requests: Option<i64>,
    total_tool_calls: Option<i64>,
    unique_users: Option<i64>,
    total_cost_usd: Option<f64>,
    cache_savings_usd: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct ModelStatsExtRow {
    model: String,
    request_count: Option<i64>,
    total_input: Option<i64>,
    total_output: Option<i64>,
    total_cache_read: Option<i64>,
    total_cache_write: Option<i64>,
    avg_duration_ms: Option<i32>,
    streaming_count: Option<i64>,
    cost_usd: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct TrendRow {
    bucket: String,
    request_count: Option<i64>,
    total_tokens: Option<i64>,
    cost_usd: Option<f64>,
}

// --- Org Analytics ---

#[derive(Deserialize)]
pub struct OrgAnalyticsParams {
    pub days: Option<i32>,
    pub granularity: Option<String>,
    pub team: Option<String>,
    pub user: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

fn build_org_filter(params: &OrgAnalyticsParams) -> db::org_analytics::OrgAnalyticsFilter {
    // Convert from/to date range to relative days if provided
    let days = if let (Some(from), Some(to)) = (&params.from, &params.to) {
        // Parse dates and compute days from now to the 'from' date
        if let (Ok(from_date), Ok(to_date)) = (
            chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d"),
            chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d"),
        ) {
            let today = chrono::Utc::now().date_naive();
            // Days = from now back to the 'from' date (inclusive of the full range)
            let days_back = (today - from_date).num_days().max(1) as i32;
            // But also ensure we cover the 'to' date (which may be today or earlier)
            let _range_days = (to_date - from_date).num_days().max(1);
            days_back
        } else {
            params.days.unwrap_or(7)
        }
    } else {
        params.days.unwrap_or(7)
    };

    db::org_analytics::OrgAnalyticsFilter {
        days,
        granularity: params
            .granularity
            .clone()
            .unwrap_or_else(|| "auto".to_string()),
        team: params.team.clone(),
        user: params.user.clone(),
        model: params.model.clone(),
        endpoint: params.endpoint.clone(),
        from: params.from.clone(),
        to: params.to.clone(),
    }
}

pub async fn get_org_overview(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<OrgAnalyticsParams>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    let filter = build_org_filter(&params);

    let overview = match db::org_analytics::org_overview(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let filters = match db::org_analytics::org_filter_options(pool).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    Json(json!({
        "overview": overview,
        "filters": filters,
    }))
    .into_response()
}

pub async fn get_org_spend(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<OrgAnalyticsParams>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    let filter = build_org_filter(&params);

    let timeseries = match db::org_analytics::org_spend_timeseries(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let by_team = match db::org_analytics::org_spend_by_team(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let by_user = match db::org_analytics::org_spend_by_user(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let by_model = match db::org_analytics::org_spend_by_model(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let budget_status = match db::org_analytics::org_budget_status(pool).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let forecast = match db::org_analytics::org_spend_forecast(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    Json(json!({
        "timeseries": timeseries,
        "by_team": by_team,
        "by_user": by_user,
        "by_model": by_model,
        "budget_status": budget_status,
        "forecast": forecast,
    }))
    .into_response()
}

pub async fn get_org_activity(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<OrgAnalyticsParams>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    let filter = build_org_filter(&params);

    let active_users = match db::org_analytics::org_active_users_timeseries(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let heatmap = match db::org_analytics::org_hourly_heatmap(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    Json(json!({
        "active_users": active_users,
        "heatmap": heatmap,
    }))
    .into_response()
}

pub async fn get_org_models(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<OrgAnalyticsParams>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    let filter = build_org_filter(&params);

    let model_mix = match db::org_analytics::org_model_mix(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let latency = match db::org_analytics::org_latency_percentiles(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let cache_rate = match db::org_analytics::org_cache_rate_timeseries(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let token_breakdown = match db::org_analytics::org_token_breakdown(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let endpoints = match db::org_analytics::org_endpoint_utilization(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    Json(json!({
        "model_mix": model_mix,
        "latency": latency,
        "cache_rate": cache_rate,
        "token_breakdown": token_breakdown,
        "endpoints": endpoints,
    }))
    .into_response()
}

pub async fn get_org_tools(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<OrgAnalyticsParams>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    let filter = build_org_filter(&params);

    let mcp_servers = match db::org_analytics::org_mcp_servers(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let top_tools = match db::org_analytics::org_top_tools(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mcp_adoption = match db::org_analytics::org_mcp_adoption(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let totals = match db::org_analytics::org_tool_totals(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    Json(json!({
        "mcp_servers": mcp_servers,
        "top_tools": top_tools,
        "mcp_adoption": mcp_adoption,
        "totals": totals,
    }))
    .into_response()
}

pub async fn export_org_csv(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<OrgAnalyticsParams>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    let filter = build_org_filter(&params);

    let rows = match db::org_analytics::org_export(pool, &filter).await {
        Ok(r) => r,
        Err(e) => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let mut csv = String::from(
        "recorded_at,user,team,model,input_tokens,output_tokens,cache_read,cache_write,cost_usd,duration_ms,tool_count,endpoint\n",
    );
    for row in &rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{:.4},{},{},{}\n",
            row.recorded_at.to_rfc3339(),
            row.user_identity.as_deref().unwrap_or(""),
            row.team_name.as_deref().unwrap_or(""),
            row.model,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.cost_usd.unwrap_or(0.0),
            row.duration_ms.unwrap_or(0),
            row.tool_count,
            row.endpoint_name.as_deref().unwrap_or(""),
        ));
    }

    let days = filter.days;
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/csv")
        .header(
            "content-disposition",
            format!("attachment; filename=\"ccag-org-spend-{}d.csv\"", days),
        )
        .body(axum::body::Body::from(csv))
        .unwrap()
}

// --- Identity Providers ---

#[derive(Deserialize)]
pub struct CreateIdpRequest {
    pub name: String,
    pub issuer_url: String,
    pub client_id: Option<String>,
    pub audience: Option<String>,
    pub jwks_url: Option<String>,
    #[serde(default = "default_flow_type")]
    pub flow_type: String,
    #[serde(default)]
    pub auto_provision: bool,
    #[serde(default = "default_role")]
    pub default_role: String,
    pub allowed_domains: Option<Vec<String>>,
    pub user_claim: Option<String>,
    pub scopes: Option<String>,
}

fn default_flow_type() -> String {
    "device_code".to_string()
}

pub async fn create_idp(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<CreateIdpRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::idp::create_idp(
        pool,
        &body.name,
        &body.issuer_url,
        body.client_id.as_deref(),
        body.audience.as_deref(),
        body.jwks_url.as_deref(),
        &body.flow_type,
        body.auto_provision,
        &body.default_role,
        body.allowed_domains.as_deref(),
        body.user_claim.as_deref(),
        body.scopes.as_deref(),
    )
    .await
    {
        Ok(idp) => {
            tracing::info!(id = %idp.id, name = %idp.name, "Created IDP");
            let _ = db::settings::bump_cache_version(pool).await;
            (StatusCode::CREATED, Json(json!(idp))).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn list_idps(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;
    let pool = &pool;

    match db::idp::list_idps(pool).await {
        Ok(idps) => {
            let idps_json: Vec<_> = idps.iter().map(|idp| json!(idp)).collect();
            Json(json!({ "idps": idps_json })).into_response()
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct UpdateIdpRequest {
    pub name: String,
    pub issuer_url: String,
    pub client_id: Option<String>,
    pub audience: Option<String>,
    pub jwks_url: Option<String>,
    #[serde(default = "default_flow_type")]
    pub flow_type: String,
    #[serde(default)]
    pub auto_provision: bool,
    #[serde(default = "default_role")]
    pub default_role: String,
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub user_claim: Option<String>,
    pub scopes: Option<String>,
}

fn default_true() -> bool {
    true
}

pub async fn update_idp(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(idp_id): Path<Uuid>,
    Json(body): Json<UpdateIdpRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::idp::update_idp(
        pool,
        idp_id,
        &body.name,
        &body.issuer_url,
        body.client_id.as_deref(),
        body.audience.as_deref(),
        body.jwks_url.as_deref(),
        &body.flow_type,
        body.auto_provision,
        &body.default_role,
        body.allowed_domains.as_deref(),
        body.enabled,
        body.user_claim.as_deref(),
        body.scopes.as_deref(),
    )
    .await
    {
        Ok(true) => {
            let _ = db::settings::bump_cache_version(pool).await;
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "IDP not found or is system-managed"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn delete_idp(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(idp_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::idp::delete_idp(pool, idp_id).await {
        Ok(true) => {
            let _ = db::settings::bump_cache_version(pool).await;
            Json(json!({ "deleted": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "IDP not found or is system-managed"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// --- Settings ---

#[derive(Deserialize)]
pub struct UpdateSettingRequest {
    pub value: String,
}

pub async fn get_settings(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let websearch_mode = db::settings::get_setting(&pool, "websearch_mode")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "enabled".to_string());
    Json(json!({
        "virtual_keys_enabled": state.virtual_keys_enabled(),
        "admin_login_enabled": state.admin_login_enabled(),
        "session_token_ttl_hours": state.session_token_ttl_hours.load(std::sync::atomic::Ordering::Relaxed),
        "websearch_mode": websearch_mode,
    }))
    .into_response()
}

pub async fn update_setting(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(body): Json<UpdateSettingRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    // Validate known settings
    match key.as_str() {
        "virtual_keys_enabled" | "admin_login_enabled" => {
            if body.value != "true" && body.value != "false" {
                return error_response(StatusCode::BAD_REQUEST, "Value must be 'true' or 'false'");
            }
        }
        "session_token_ttl_hours" => match body.value.parse::<u64>() {
            Ok(v) if (1..=8760).contains(&v) => {}
            _ => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Value must be a number between 1 and 8760",
                );
            }
        },
        _ => return error_response(StatusCode::BAD_REQUEST, &format!("Unknown setting: {key}")),
    }

    match db::settings::set_setting(pool, &key, &body.value).await {
        Ok(()) => {
            // Immediately update local state
            if key == "virtual_keys_enabled" {
                state.set_virtual_keys_enabled(body.value == "true");
            } else if key == "admin_login_enabled" {
                state.set_admin_login_enabled(body.value == "true");
            } else if key == "session_token_ttl_hours"
                && let Ok(v) = body.value.parse::<i64>()
            {
                state
                    .session_token_ttl_hours
                    .store(v, std::sync::atomic::Ordering::Relaxed);
            }
            tracing::info!(key = %key, value = %body.value, "Setting updated");
            Json(json!({ "updated": true })).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn check_admin_auth(headers: &HeaderMap, state: &GatewayState) -> Result<(), Response> {
    check_admin_auth_identity(headers, state).await.map(|_| ())
}

/// Like check_admin_auth but returns the admin's identity (sub claim).
pub async fn check_admin_auth_identity(
    headers: &HeaderMap,
    state: &GatewayState,
) -> Result<String, Response> {
    let (sub, role, _) = check_auth_identity(headers, state).await?;
    if role != "admin" {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "Admin access required",
        ));
    }
    Ok(sub)
}

/// Authenticate any user (admin or member). Returns (sub, role, user_id).
pub async fn check_auth_identity(
    headers: &HeaderMap,
    state: &GatewayState,
) -> Result<(String, String, Option<Uuid>), Response> {
    let provided = headers
        .get("x-api-key")
        .or_else(|| headers.get("authorization"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s));

    let key = match provided {
        Some(k) => k,
        None => {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "Missing authentication",
            ));
        }
    };

    // Try gateway session token first (cheap HMAC check)
    if let Ok(identity) = crate::auth::session::validate(&state.session_signing_key, key) {
        let role = match super::handlers::resolve_oidc_role(state, &identity).await {
            Ok(r) => r,
            Err(msg) => {
                return Err(error_response(StatusCode::FORBIDDEN, &msg));
            }
        };
        let pool = state.db().await;
        let user_id = crate::db::users::get_user_by_email(&pool, identity.user_id())
            .await
            .ok()
            .flatten()
            .map(|u| u.id);
        return Ok((identity.user_id().to_string(), role, user_id));
    }

    // Try OIDC token (external IDP)
    if state.idp_validator.idp_count().await > 0
        && let Ok(identity) = state.idp_validator.validate_token(key).await
    {
        let role = match super::handlers::resolve_oidc_role(state, &identity).await {
            Ok(r) => r,
            Err(msg) => {
                return Err(error_response(StatusCode::FORBIDDEN, &msg));
            }
        };
        let pool = state.db().await;
        let user_id = crate::db::users::get_user_by_email(&pool, identity.user_id())
            .await
            .ok()
            .flatten()
            .map(|u| u.id);
        return Ok((identity.user_id().to_string(), role, user_id));
    }

    Err(error_response(
        StatusCode::UNAUTHORIZED,
        "Invalid credentials",
    ))
}

fn admin_error(status: StatusCode, message: &str) -> Response {
    tracing::error!(status = %status.as_u16(), %message, "Admin API error");
    error_response(status, message)
}

/// Generate a one-time setup token for a virtual key.
/// The token can be used with `/auth/setup?token=...` to get a setup script
/// with the VK embedded. Token expires after 5 minutes and is single-use.
///
/// The portal passes the raw key in the POST body (it has it from key creation).
/// We can't recover the raw key from the stored hash.
pub async fn create_setup_token(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (_, role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    let pool = state.db().await;
    let pool = &pool;

    let raw_key = match body.get("raw_key").and_then(|v| v.as_str()) {
        Some(k) => k.to_string(),
        None => return error_response(StatusCode::BAD_REQUEST, "raw_key is required"),
    };

    // Verify key exists, is active, and belongs to the caller (unless admin)
    match db::keys::list_keys(pool).await {
        Ok(keys) => {
            if let Some(key) = keys.iter().find(|k| k.id == key_id && k.is_active) {
                if role != "admin" && key.user_id != user_id {
                    return error_response(StatusCode::NOT_FOUND, "Key not found or revoked");
                }
            } else {
                return error_response(StatusCode::NOT_FOUND, "Key not found or revoked");
            }
        }
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }

    // Generate token
    let token = format!("st_{}", uuid::Uuid::new_v4().simple());

    // Clean up expired tokens and insert new one into DB
    crate::db::setup_tokens::cleanup_expired(pool).await.ok();
    if let Err(e) = crate::db::setup_tokens::create(pool, &token, &raw_key).await {
        tracing::error!("Failed to create setup token: {e:?}");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create setup token",
        );
    }

    tracing::info!(key_id = %key_id, "Generated setup token for virtual key");

    (
        StatusCode::OK,
        Json(json!({
            "token": token,
            "expires_in_secs": crate::proxy::SETUP_TOKEN_TTL_SECS,
        })),
    )
        .into_response()
}

pub async fn validate_bedrock(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    // List inference profiles to check connectivity and model availability
    match state
        .bedrock_control_client
        .list_inference_profiles()
        .send()
        .await
    {
        Ok(output) => {
            let profiles: Vec<_> = output
                .inference_profile_summaries()
                .iter()
                .map(|p| {
                    json!({
                        "name": p.inference_profile_name(),
                        "id": p.inference_profile_id(),
                        "status": format!("{:?}", p.status()),
                        "type": format!("{:?}", p.r#type()),
                    })
                })
                .collect();

            let profile_count = profiles.len();
            Json(json!({
                "status": "connected",
                "region": state.config.bedrock_routing_prefix,
                "inference_profiles": profiles,
                "profile_count": profile_count,
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "status": "error",
            "error": format!("{e}"),
        }))
        .into_response(),
    }
}

// --- Endpoints ---

pub async fn list_endpoints(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    match db::endpoints::list_endpoints(pool).await {
        Ok(endpoints) => {
            let mut enriched = Vec::new();
            for ep in &endpoints {
                let health = if let Some(client) = state.endpoint_pool.get_client(ep.id).await {
                    if client
                        .last_health_check
                        .load(std::sync::atomic::Ordering::Relaxed)
                        == 0
                    {
                        "unknown"
                    } else if client.healthy.load(std::sync::atomic::Ordering::Relaxed) {
                        "healthy"
                    } else {
                        "unhealthy"
                    }
                } else {
                    "not_loaded"
                };
                enriched.push(json!({
                    "id": ep.id,
                    "name": ep.name,
                    "role_arn": ep.role_arn,
                    "external_id": ep.external_id,
                    "inference_profile_arn": ep.inference_profile_arn,
                    "region": ep.region,
                    "routing_prefix": ep.routing_prefix,
                    "priority": ep.priority,
                    "is_default": ep.is_default,
                    "enabled": ep.enabled,
                    "health": health,
                    "created_at": ep.created_at,
                }));
            }
            Json(json!({ "endpoints": enriched })).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct CreateEndpointRequest {
    pub name: String,
    pub role_arn: Option<String>,
    pub external_id: Option<String>,
    pub inference_profile_arn: Option<String>,
    pub region: String,
    pub routing_prefix: String,
    #[serde(default)]
    pub priority: i32,
}

pub async fn create_endpoint(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<CreateEndpointRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    match db::endpoints::create_endpoint(
        pool,
        &body.name,
        body.role_arn.as_deref(),
        body.external_id.as_deref(),
        body.inference_profile_arn.as_deref(),
        &body.region,
        &body.routing_prefix,
        body.priority,
    )
    .await
    {
        Ok(endpoint) => {
            tracing::info!(id = %endpoint.id, name = %endpoint.name, "Created endpoint");
            // Reload endpoints into pool
            if let Ok(endpoints) = db::endpoints::get_enabled_endpoints(pool).await {
                state
                    .endpoint_pool
                    .load_endpoints(endpoints, &state.aws_config)
                    .await;
            }
            (StatusCode::CREATED, Json(json!(endpoint))).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct UpdateEndpointRequest {
    pub name: String,
    pub role_arn: Option<String>,
    pub external_id: Option<String>,
    pub inference_profile_arn: Option<String>,
    pub region: String,
    pub routing_prefix: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

pub async fn update_endpoint(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(endpoint_id): Path<Uuid>,
    Json(body): Json<UpdateEndpointRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    match db::endpoints::update_endpoint(
        pool,
        endpoint_id,
        &body.name,
        body.role_arn.as_deref(),
        body.external_id.as_deref(),
        body.inference_profile_arn.as_deref(),
        &body.region,
        &body.routing_prefix,
        body.priority,
        body.enabled,
    )
    .await
    {
        Ok(true) => {
            tracing::info!(%endpoint_id, "Updated endpoint");
            // Reload endpoints into pool
            if let Ok(endpoints) = db::endpoints::get_enabled_endpoints(pool).await {
                state
                    .endpoint_pool
                    .load_endpoints(endpoints, &state.aws_config)
                    .await;
            }
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "Endpoint not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn delete_endpoint(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(endpoint_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    match db::endpoints::delete_endpoint(pool, endpoint_id).await {
        Ok(true) => {
            tracing::info!(%endpoint_id, "Deleted endpoint");
            // Reload endpoints into pool
            if let Ok(endpoints) = db::endpoints::get_enabled_endpoints(pool).await {
                state
                    .endpoint_pool
                    .load_endpoints(endpoints, &state.aws_config)
                    .await;
            }
            Json(json!({ "deleted": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "Endpoint not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn get_team_endpoints(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(team_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    let routing_strategy = db::teams::get_team(pool, team_id)
        .await
        .ok()
        .flatten()
        .map(|t| t.routing_strategy)
        .unwrap_or_else(|| "sticky_user".to_string());

    match db::endpoints::get_team_endpoints(pool, team_id).await {
        Ok(endpoints) => {
            Json(json!({ "endpoints": endpoints, "routing_strategy": routing_strategy }))
                .into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct TeamEndpointAssignment {
    pub endpoint_id: Uuid,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Deserialize)]
pub struct SetTeamEndpointsRequest {
    #[serde(default)]
    pub routing_strategy: Option<String>,
    pub endpoints: Vec<TeamEndpointAssignment>,
}

pub async fn set_team_endpoints(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(team_id): Path<Uuid>,
    Json(body): Json<SetTeamEndpointsRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;

    let valid_strategies = ["primary_fallback", "sticky_user", "round_robin"];
    if let Some(ref strategy) = body.routing_strategy {
        if !valid_strategies.contains(&strategy.as_str()) {
            return admin_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid routing_strategy '{strategy}'"),
            );
        }
        if let Err(e) = db::teams::update_team_routing_strategy(pool, team_id, strategy).await {
            return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    }

    let assignments: Vec<(Uuid, i32)> = body
        .endpoints
        .iter()
        .map(|a| (a.endpoint_id, a.priority))
        .collect();
    match db::endpoints::set_team_endpoints(pool, team_id, &assignments).await {
        Ok(()) => {
            tracing::info!(%team_id, count = assignments.len(), "Set team endpoint assignments");
            // Reload endpoint pool so default_endpoint_id stays current
            if let Ok(all_endpoints) = db::endpoints::list_endpoints(pool).await {
                state
                    .endpoint_pool
                    .load_endpoints(all_endpoints, &state.aws_config)
                    .await;
            }
            Json(json!({ "updated": true })).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn set_default_endpoint(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(endpoint_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let pool = &pool;
    match db::endpoints::set_default_endpoint(pool, endpoint_id).await {
        Ok(true) => {
            tracing::info!(%endpoint_id, "Set default endpoint");
            // Reload pool so default_endpoint_id is updated in memory
            if let Ok(all_endpoints) = db::endpoints::list_endpoints(pool).await {
                state
                    .endpoint_pool
                    .load_endpoints(all_endpoints, &state.aws_config)
                    .await;
            }
            Json(json!({ "updated": true })).into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "Endpoint not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn get_endpoint_quotas(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(endpoint_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    // Get the endpoint's client (which has its own credentials/region)
    let client = match state.endpoint_pool.get_client(endpoint_id).await {
        Some(c) => c,
        None => return error_response(StatusCode::NOT_FOUND, "Endpoint not loaded"),
    };

    // Use the endpoint's own quota cache (persists across requests, 5-min TTL)
    match client.quota_cache.get_bedrock_quotas().await {
        Ok(data) => Json(data).into_response(),
        Err(e) => {
            let is_access_issue = e.contains("AccessDenied")
                || e.contains("not authorized")
                || e.contains("AccessDeniedException")
                || e.contains("UnauthorizedAccess")
                || e.contains("service error");
            let hint = if is_access_issue {
                "The endpoint's IAM role is missing the servicequotas:ListServiceQuotas permission. \
                 Quota visibility is optional — inference still works without it. \
                 Add this permission to see RPM/TPM limits in the portal."
            } else {
                "Failed to load quota data. This is optional — inference is not affected."
            };
            Json(json!({
                "error": { "message": hint, "detail": e, "type": "quota_access" },
                "type": "error"
            }))
            .into_response()
        }
    }
}

// --- Model Availability (Phase 4) ---

/// List available models for a specific endpoint via ListInferenceProfiles.
pub async fn get_endpoint_models(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(endpoint_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let client = match state.endpoint_pool.get_client(endpoint_id).await {
        Some(c) => c,
        None => return error_response(StatusCode::NOT_FOUND, "Endpoint not loaded"),
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if let Some(arn) = &client.config.inference_profile_arn {
        // Application inference profile: validate the specific ARN exists and is accessible
        match client
            .control_client
            .get_inference_profile()
            .inference_profile_identifier(arn)
            .send()
            .await
        {
            Ok(profile) => {
                client
                    .last_health_check
                    .store(now_secs, std::sync::atomic::Ordering::Relaxed);
                crate::endpoint::EndpointPool::mark_healthy(&client);
                let models: Vec<_> = profile
                    .models()
                    .iter()
                    .map(|m| json!({ "arn": m.model_arn() }))
                    .collect();
                Json(json!({
                    "endpoint_id": endpoint_id,
                    "endpoint_name": client.config.name,
                    "profile_arn": arn,
                    "profile_name": profile.inference_profile_name(),
                    "profile_status": format!("{:?}", profile.status()),
                    "models": models,
                    "model_count": models.len(),
                }))
                .into_response()
            }
            Err(e) => {
                client
                    .last_health_check
                    .store(now_secs, std::sync::atomic::Ordering::Relaxed);
                crate::endpoint::EndpointPool::mark_unhealthy(&client);
                admin_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("Inference profile not accessible: {}", aws_err(&e)),
                )
            }
        }
    } else {
        // Standard CRI: list system-defined inference profiles to verify credentials/region
        match client.control_client.list_inference_profiles().send().await {
            Ok(output) => {
                client
                    .last_health_check
                    .store(now_secs, std::sync::atomic::Ordering::Relaxed);
                crate::endpoint::EndpointPool::mark_healthy(&client);
                let models: Vec<_> = output
                    .inference_profile_summaries()
                    .iter()
                    .map(|p| {
                        json!({
                            "name": p.inference_profile_name(),
                            "id": p.inference_profile_id(),
                            "status": format!("{:?}", p.status()),
                            "type": format!("{:?}", p.r#type()),
                        })
                    })
                    .collect();
                let model_count = models.len();
                Json(json!({
                    "endpoint_id": endpoint_id,
                    "endpoint_name": client.config.name,
                    "models": models,
                    "model_count": model_count,
                }))
                .into_response()
            }
            Err(e) => {
                client
                    .last_health_check
                    .store(now_secs, std::sync::atomic::Ordering::Relaxed);
                crate::endpoint::EndpointPool::mark_unhealthy(&client);
                admin_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("Failed to list inference profiles: {}", aws_err(&e)),
                )
            }
        }
    }
}

/// Aggregate model availability across all healthy endpoints.
pub async fn get_all_models(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let clients = state.endpoint_pool.get_all_clients().await;
    let mut endpoint_models = Vec::new();

    for client in &clients {
        if !client.healthy.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        match client.control_client.list_inference_profiles().send().await {
            Ok(output) => {
                let models: Vec<_> = output
                    .inference_profile_summaries()
                    .iter()
                    .map(|p| {
                        json!({
                            "name": p.inference_profile_name(),
                            "id": p.inference_profile_id(),
                            "status": format!("{:?}", p.status()),
                        })
                    })
                    .collect();
                endpoint_models.push(json!({
                    "endpoint_id": client.config.id,
                    "endpoint_name": client.config.name,
                    "region": client.config.region,
                    "model_count": models.len(),
                    "models": models,
                }));
            }
            Err(e) => {
                endpoint_models.push(json!({
                    "endpoint_id": client.config.id,
                    "endpoint_name": client.config.name,
                    "region": client.config.region,
                    "error": format!("{e}"),
                }));
            }
        }
    }

    Json(json!({
        "endpoints": endpoint_models,
    }))
    .into_response()
}

// --- Bedrock Quotas ---

pub async fn get_bedrock_quotas(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let cache = match &state.quota_cache {
        Some(c) => c,
        None => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Quota service not available",
            );
        }
    };
    match cache.get_bedrock_quotas().await {
        Ok(data) => Json(data).into_response(),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

// --- Search Provider (user self-service) ---

/// Get all search provider configurations for the current user.
pub async fn get_search_providers(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    let (_sub, _role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let pool = state.db().await;
    let pool = &pool;
    let user_id = match user_id {
        Some(id) => id,
        None => return error_response(StatusCode::NOT_FOUND, "User not found in database"),
    };

    match db::search_providers::get_all_by_user_id(pool, user_id).await {
        Ok(configs) => {
            let providers: Vec<_> = configs
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "provider_type": c.provider_type,
                        "has_api_key": c.api_key.is_some(),
                        "api_url": c.api_url,
                        "max_results": c.max_results,
                        "enabled": c.enabled,
                        "created_at": c.created_at,
                        "updated_at": c.updated_at,
                    })
                })
                .collect();
            Json(json!({ "providers": providers })).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct SetSearchProviderRequest {
    pub provider_type: String,
    pub api_key: Option<String>,
    pub api_url: Option<String>,
    #[serde(default = "default_max_results")]
    pub max_results: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}
fn default_max_results() -> i32 {
    5
}

/// Set or update a search provider configuration (keyed by provider_type).
pub async fn set_search_provider(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<SetSearchProviderRequest>,
) -> Response {
    let (_sub, _role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let pool = state.db().await;
    let pool = &pool;
    let user_id = match user_id {
        Some(id) => id,
        None => return error_response(StatusCode::NOT_FOUND, "User not found in database"),
    };

    let valid_types = ["duckduckgo", "tavily", "serper", "custom"];
    if !valid_types.contains(&body.provider_type.as_str()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "Invalid provider_type. Must be one of: {}",
                valid_types.join(", ")
            ),
        );
    }

    match body.provider_type.as_str() {
        "tavily" | "serper" => {
            if body.api_key.as_ref().is_none_or(|k| k.is_empty()) {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("{} requires an API key", body.provider_type),
                );
            }
        }
        "custom" => {
            if body.api_url.as_ref().is_none_or(|u| u.is_empty()) {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Custom provider requires an API URL",
                );
            }
        }
        _ => {}
    }

    match db::search_providers::upsert(
        pool,
        user_id,
        &body.provider_type,
        body.api_key.as_deref(),
        body.api_url.as_deref(),
        body.max_results.clamp(1, 20),
        body.enabled,
    )
    .await
    {
        Ok(config) => {
            tracing::info!(
                user_id = %user_id,
                provider = %config.provider_type,
                "User search provider configured"
            );
            Json(json!({
                "provider": {
                    "id": config.id,
                    "provider_type": config.provider_type,
                    "has_api_key": config.api_key.is_some(),
                    "api_url": config.api_url,
                    "max_results": config.max_results,
                    "enabled": config.enabled,
                    "updated_at": config.updated_at,
                }
            }))
            .into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct ActivateProviderRequest {
    pub provider_type: String,
}

/// Activate a search provider (disable all others for this user).
pub async fn activate_search_provider(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<ActivateProviderRequest>,
) -> Response {
    let (_sub, _role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let pool = state.db().await;
    let pool = &pool;
    let user_id = match user_id {
        Some(id) => id,
        None => return error_response(StatusCode::NOT_FOUND, "User not found in database"),
    };

    match db::search_providers::activate(pool, user_id, &body.provider_type).await {
        Ok(Some(config)) => {
            tracing::info!(
                user_id = %user_id,
                provider = %config.provider_type,
                "Search provider activated"
            );
            Json(json!({
                "provider": {
                    "id": config.id,
                    "provider_type": config.provider_type,
                    "enabled": config.enabled,
                }
            }))
            .into_response()
        }
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            &format!(
                "Provider '{}' not configured. Save it first.",
                body.provider_type
            ),
        ),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Delete a specific search provider configuration by type.
pub async fn delete_search_provider(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(provider_type): Path<String>,
) -> Response {
    let (_sub, _role, user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let pool = state.db().await;
    let pool = &pool;
    let user_id = match user_id {
        Some(id) => id,
        None => return error_response(StatusCode::NOT_FOUND, "User not found in database"),
    };

    match db::search_providers::delete_by_user_and_type(pool, user_id, &provider_type).await {
        Ok(true) => {
            tracing::info!(user_id = %user_id, provider = %provider_type, "Search provider config deleted");
            Json(json!({ "deleted": true })).into_response()
        }
        Ok(false) => {
            Json(json!({ "deleted": false, "message": "No config to delete" })).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Test a search provider config by running a sample query.
pub async fn test_search_provider(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<SetSearchProviderRequest>,
) -> Response {
    let (_sub, _role, _user_id) = match check_auth_identity(&headers, &state).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    let provider = match body.provider_type.as_str() {
        "duckduckgo" => crate::websearch::SearchProvider::DuckDuckGo {
            max_results: body.max_results.clamp(1, 5) as usize,
        },
        "tavily" => {
            let api_key = match &body.api_key {
                Some(k) if !k.is_empty() => k.clone(),
                _ => {
                    return Json(json!({
                        "success": false,
                        "error": "Tavily requires an API key",
                    }))
                    .into_response();
                }
            };
            crate::websearch::SearchProvider::Tavily {
                api_key,
                max_results: body.max_results.clamp(1, 5) as usize,
            }
        }
        "serper" => {
            let api_key = match &body.api_key {
                Some(k) if !k.is_empty() => k.clone(),
                _ => {
                    return Json(json!({
                        "success": false,
                        "error": "Serper requires an API key",
                    }))
                    .into_response();
                }
            };
            crate::websearch::SearchProvider::Serper {
                api_key,
                max_results: body.max_results.clamp(1, 5) as usize,
            }
        }
        "custom" => {
            let api_url = match &body.api_url {
                Some(u) if !u.is_empty() => u.clone(),
                _ => {
                    return Json(json!({
                        "success": false,
                        "error": "Custom provider requires an API URL",
                    }))
                    .into_response();
                }
            };
            crate::websearch::SearchProvider::Custom {
                api_url,
                api_key: body.api_key.clone(),
                max_results: body.max_results.clamp(1, 5) as usize,
            }
        }
        _ => {
            return Json(json!({
                "success": false,
                "error": format!("Unknown provider: {}", body.provider_type),
            }))
            .into_response();
        }
    };

    match provider.validate(&state.http_client).await {
        Ok(results) => Json(json!({
            "success": true,
            "result_count": results.len(),
            "results": results.iter().map(|r| json!({
                "title": r.title,
                "url": r.url,
                "snippet": if r.snippet.len() > 200 { format!("{}...", &r.snippet[..200]) } else { r.snippet.clone() },
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string(),
        }))
        .into_response(),
    }
}

/// GET /admin/health/status — comprehensive health snapshot
pub async fn get_health_status(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;

    // Gateway uptime
    let uptime_seconds = state.started_at.elapsed().as_secs();

    // Database health
    let (db_status, db_pool_size, db_pool_idle) = {
        let size = pool.size();
        let idle = pool.num_idle() as u32;
        match sqlx::query("SELECT 1").execute(&pool).await {
            Ok(_) => ("ok", size, idle),
            Err(_) => ("error", size, idle),
        }
    };

    // Bedrock health (from cached probe)
    let (bedrock_status, bedrock_last_check) = {
        let cached = state.bedrock_health.read().await;
        match *cached {
            Some((instant, healthy)) => {
                let age_secs = instant.elapsed().as_secs();
                let last_check = chrono::Utc::now()
                    - chrono::TimeDelta::try_seconds(age_secs as i64).unwrap_or_default();
                (
                    if healthy { "ok" } else { "unhealthy" },
                    Some(last_check.to_rfc3339()),
                )
            }
            None => ("unknown", None),
        }
    };

    // In-flight requests (read from metric — approximate)
    // We report the gauge value from the telemetry system
    let in_flight = 0_i64; // Gauge value not directly readable; report as informational

    // Per-endpoint stats
    let all_stats = state.endpoint_stats.get_all_stats().await;
    let clients = state.endpoint_pool.get_all_clients().await;

    let mut endpoints = Vec::new();
    for client in &clients {
        let ep = &client.config;
        let healthy = client.healthy.load(std::sync::atomic::Ordering::Relaxed);
        let last_check_epoch = client
            .last_health_check
            .load(std::sync::atomic::Ordering::Relaxed);
        let last_health_check = if last_check_epoch > 0 {
            chrono::DateTime::from_timestamp(last_check_epoch, 0).map(|dt| dt.to_rfc3339())
        } else {
            None
        };

        let stats = all_stats.get(&ep.id).cloned().unwrap_or(
            crate::endpoint::stats::EndpointStatSnapshot {
                throttle_count_1h: 0,
                error_count_1h: 0,
                request_count: 0,
            },
        );

        // Read cached quotas (non-blocking)
        let quotas = match client.quota_cache.get_cached().await {
            Some(q) => q
                .get("quotas")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            None => Vec::new(),
        };

        endpoints.push(json!({
            "id": ep.id,
            "name": ep.name,
            "region": ep.region,
            "role_arn": ep.role_arn,
            "routing_prefix": ep.routing_prefix,
            "health": if healthy { "healthy" } else if last_check_epoch == 0 { "unknown" } else { "unhealthy" },
            "last_health_check": last_health_check,
            "is_default": ep.is_default,
            "enabled": ep.enabled,
            "stats": stats,
            "quotas": quotas,
        }));
    }

    // Overall gateway status: degraded if DB or Bedrock is down
    let gateway_status = if db_status == "ok" && bedrock_status != "unhealthy" {
        "ok"
    } else {
        "degraded"
    };

    Json(json!({
        "gateway": { "status": gateway_status, "uptime_seconds": uptime_seconds },
        "database": { "status": db_status, "pool_size": db_pool_size, "pool_idle": db_pool_idle },
        "bedrock": { "status": bedrock_status, "last_check": bedrock_last_check },
        "endpoints": endpoints,
        "in_flight_requests": in_flight,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    }))
    .into_response()
}

// ── Notification Config ───────────────────────────────────────

/// Validation regex for SNS ARN.
fn is_valid_sns_arn(arn: &str) -> bool {
    regex::Regex::new(r"^arn:aws:sns:[a-z0-9-]+:\d{12}:.+$")
        .unwrap()
        .is_match(arn)
}

/// Validation regex for EventBridge bus ARN.
fn is_valid_eventbridge_arn(arn: &str) -> bool {
    regex::Regex::new(r"^arn:aws:events:[a-z0-9-]+:\d{12}:event-bus/.+$")
        .unwrap()
        .is_match(arn)
}

const VALID_CATEGORIES: &[&str] = &["budget", "rate_limit"];

fn validate_categories(categories: &serde_json::Value) -> Result<(), String> {
    let arr = categories
        .as_array()
        .ok_or("event_categories must be a JSON array")?;
    if arr.is_empty() {
        return Err("event_categories must not be empty".to_string());
    }
    for v in arr {
        let s = v.as_str().ok_or("each category must be a string")?;
        if !VALID_CATEGORIES.contains(&s) {
            return Err(format!(
                "invalid category '{}', must be one of: {:?}",
                s, VALID_CATEGORIES
            ));
        }
    }
    Ok(())
}

fn validate_destination(dest_type: &str, dest_value: &str) -> Result<(), String> {
    match dest_type {
        "webhook" => {
            if !dest_value.starts_with("https://") {
                return Err("Webhook URL must start with https://".to_string());
            }
        }
        "sns" => {
            if !is_valid_sns_arn(dest_value) {
                return Err("Invalid SNS topic ARN format".to_string());
            }
        }
        "eventbridge" => {
            if !is_valid_eventbridge_arn(dest_value) {
                return Err("Invalid EventBridge event bus ARN format".to_string());
            }
        }
        _ => {
            return Err(format!(
                "Unknown destination type '{}'. Must be webhook, sns, or eventbridge",
                dest_type
            ));
        }
    }
    Ok(())
}

/// GET /admin/notifications/config
pub async fn get_notification_config(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;

    let (active, draft) = match db::notification_config::get_both(&pool).await {
        Ok(configs) => configs,
        Err(e) => {
            tracing::error!(%e, "Failed to get notification config");
            return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    };

    let deliveries = db::notification_config::get_recent_deliveries(&pool, 20)
        .await
        .unwrap_or_default();

    let env_fallback = state.config.notification_url.as_deref();

    Json(json!({
        "active": active,
        "draft": draft,
        "delivery_history": deliveries,
        "env_fallback": env_fallback,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SaveNotificationConfigRequest {
    pub destination_type: String,
    pub destination_value: String,
    #[serde(default = "default_categories")]
    pub event_categories: serde_json::Value,
}

fn default_categories() -> serde_json::Value {
    serde_json::json!(["budget"])
}

/// PUT /admin/notifications/config — save/update draft
pub async fn save_notification_config(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<SaveNotificationConfigRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    if let Err(msg) = validate_destination(&body.destination_type, &body.destination_value) {
        return error_response(StatusCode::BAD_REQUEST, &msg);
    }
    if let Err(msg) = validate_categories(&body.event_categories) {
        return error_response(StatusCode::BAD_REQUEST, &msg);
    }

    let pool = state.db().await;
    match db::notification_config::upsert_draft(
        &pool,
        &body.destination_type,
        &body.destination_value,
        &body.event_categories,
    )
    .await
    {
        Ok(config) => {
            tracing::info!(
                dest_type = %config.destination_type,
                "Notification draft saved"
            );
            (StatusCode::OK, Json(json!(config))).into_response()
        }
        Err(e) => {
            tracing::error!(%e, "Failed to save notification draft");
            admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// DELETE /admin/notifications/config — remove active config (deactivate)
pub async fn delete_notification_config(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;

    let active_deleted = db::notification_config::delete_active(&pool)
        .await
        .unwrap_or(false);

    tracing::info!(active_deleted, "Notification active config deleted");

    Json(json!({
        "active_deleted": active_deleted,
    }))
    .into_response()
}

/// DELETE /admin/notifications/draft — discard draft
pub async fn delete_notification_draft(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;
    let draft_deleted = db::notification_config::delete_draft(&pool)
        .await
        .unwrap_or(false);

    tracing::info!(draft_deleted, "Notification draft discarded");

    Json(json!({
        "draft_deleted": draft_deleted,
    }))
    .into_response()
}

/// POST /admin/notifications/test — test deliver to draft config
pub async fn test_notification(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;

    let draft = match db::notification_config::get_draft(&pool).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return error_response(StatusCode::BAD_REQUEST, "No draft config to test");
        }
        Err(e) => {
            return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    };

    // Build synthetic test payload
    let payload = crate::budget::notifications::NotificationPayload {
        source: "ccag".to_string(),
        version: "1".to_string(),
        category: "budget".to_string(),
        event_type: "budget_warning".to_string(),
        severity: "warning".to_string(),
        user_identity: Some("test@example.com".to_string()),
        team_id: None,
        team_name: Some("test-team".to_string()),
        detail: crate::budget::notifications::NotificationDetail {
            threshold_percent: 80,
            spend_usd: 40.0,
            limit_usd: 50.0,
            percent: 80.0,
            period: "weekly".to_string(),
            period_start: chrono::Utc::now().to_rfc3339(),
        },
        timestamp: chrono::Utc::now().to_rfc3339(),
    };

    let delivery_http = crate::budget::notifications::delivery_http_client();
    let (success, error, duration_ms) = crate::budget::notifications::deliver(
        &delivery_http,
        &state.sns_client,
        &state.eb_client,
        &draft.destination_type,
        &draft.destination_value,
        &payload,
    )
    .await;

    // Record test result
    let _ = db::notification_config::update_test_result(&pool, "draft", success, error.as_deref())
        .await;

    // Log the test delivery
    let payload_json = serde_json::to_value(&payload).unwrap_or_default();
    let _ = db::notification_config::log_delivery(
        &pool,
        None,
        &draft.destination_type,
        &draft.destination_value,
        "test",
        &payload_json,
        if success { "success" } else { "failure" },
        error.as_deref(),
        duration_ms,
    )
    .await;

    Json(json!({
        "success": success,
        "error": error,
        "duration_ms": duration_ms,
    }))
    .into_response()
}

/// POST /admin/notifications/activate — promote draft to active
pub async fn activate_notification(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;

    // Verify draft exists and has been tested successfully
    let draft = match db::notification_config::get_draft(&pool).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return error_response(StatusCode::BAD_REQUEST, "No draft config to activate");
        }
        Err(e) => {
            return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    };

    if draft.last_test_success != Some(true) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Draft must pass a test before activation. Use POST /admin/notifications/test first.",
        );
    }

    match db::notification_config::activate_draft(&pool).await {
        Ok(Some(config)) => {
            tracing::info!(
                dest_type = %config.destination_type,
                "Notification config activated"
            );
            (StatusCode::OK, Json(json!(config))).into_response()
        }
        Ok(None) => error_response(StatusCode::BAD_REQUEST, "No draft to activate"),
        Err(e) => {
            tracing::error!(%e, "Failed to activate notification config");
            admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

#[derive(Deserialize)]
pub struct UpdateCategoriesRequest {
    pub event_categories: serde_json::Value,
}

/// PUT /admin/notifications/categories — toggle categories on active config
pub async fn update_notification_categories(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<UpdateCategoriesRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    if let Err(msg) = validate_categories(&body.event_categories) {
        return error_response(StatusCode::BAD_REQUEST, &msg);
    }

    let pool = state.db().await;
    match db::notification_config::update_event_categories(&pool, &body.event_categories).await {
        Ok(true) => {
            tracing::info!("Notification categories updated");
            Json(json!({"updated": true})).into_response()
        }
        Ok(false) => error_response(
            StatusCode::NOT_FOUND,
            "No active notification config to update",
        ),
        Err(e) => {
            tracing::error!(%e, "Failed to update notification categories");
            admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": { "type": "admin_error", "message": message }
        })),
    )
        .into_response()
}

// ============================================================
// Websearch Admin Mode
// ============================================================

#[derive(Deserialize)]
pub struct WebsearchModeRequest {
    pub mode: String,
    pub provider: Option<serde_json::Value>,
}

pub async fn get_websearch_mode(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    let pool = state.db().await;
    let mode = db::settings::get_setting(&pool, "websearch_mode")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "enabled".to_string());

    if mode == "global" {
        let provider = db::settings::get_setting(&pool, "websearch_global_provider")
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

        let mut resp = json!({ "mode": mode });
        if let Some(mut p) = provider {
            // Mask the api_key: replace with has_api_key boolean
            if let Some(obj) = p.as_object_mut() {
                let has_key = obj
                    .get("api_key")
                    .map(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                    .unwrap_or(false);
                obj.remove("api_key");
                obj.insert("has_api_key".to_string(), serde_json::Value::Bool(has_key));
            }
            resp["provider"] = p;
        }
        Json(resp).into_response()
    } else {
        Json(json!({ "mode": mode })).into_response()
    }
}

pub async fn set_websearch_mode(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<WebsearchModeRequest>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }

    // Validate mode
    match body.mode.as_str() {
        "enabled" | "disabled" | "global" => {}
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Invalid websearch mode '{}'. Must be one of: enabled, disabled, global",
                    body.mode
                ),
            );
        }
    }

    // When mode is "global", require provider config with a valid provider_type
    if body.mode == "global" {
        match &body.provider {
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Provider configuration is required when mode is 'global'",
                );
            }
            Some(provider) => {
                let valid_types = ["duckduckgo", "tavily", "serper", "custom"];
                match provider.get("provider_type").and_then(|v| v.as_str()) {
                    Some(pt) if valid_types.contains(&pt) => {}
                    Some(pt) => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            &format!(
                                "Invalid provider_type '{}'. Must be one of: duckduckgo, tavily, serper, custom",
                                pt
                            ),
                        );
                    }
                    None => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "provider_type is required in the provider configuration. Must be one of: duckduckgo, tavily, serper, custom",
                        );
                    }
                }
            }
        }
    }

    let pool = state.db().await;

    // Store the mode
    if let Err(e) = db::settings::set_setting(&pool, "websearch_mode", &body.mode).await {
        return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }

    // Store provider config when mode is "global"
    if body.mode == "global"
        && let Some(provider) = &body.provider
    {
        let provider_json = serde_json::to_string(provider).unwrap_or_default();
        if let Err(e) =
            db::settings::set_setting(&pool, "websearch_global_provider", &provider_json).await
        {
            return admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    }

    tracing::info!(mode = %body.mode, "Websearch mode updated");
    Json(json!({ "updated": true })).into_response()
}

// ============================================================
// SCIM Token Admin Endpoints
// ============================================================

/// POST /admin/idps/{idp_id}/scim-tokens — Generate a SCIM token for an IDP
pub async fn create_scim_token(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(idp_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let name = body.get("name").and_then(|v| v.as_str());

    // Get admin identity for created_by field
    let created_by = check_admin_auth_identity(&headers, &state)
        .await
        .unwrap_or_else(|_| "admin".to_string());

    match crate::db::scim_tokens::create_scim_token(&pool, idp_id, name, &created_by).await {
        Ok((raw_token, record)) => (
            StatusCode::CREATED,
            Json(json!({
                "id": record.id,
                "token": raw_token,
                "token_prefix": record.token_prefix,
                "idp_id": record.idp_id,
                "name": record.name,
                "created_at": record.created_at.to_rfc3339(),
            })),
        )
            .into_response(),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /admin/idps/{idp_id}/scim-tokens — List SCIM tokens for an IDP
pub async fn list_scim_tokens(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(idp_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match crate::db::scim_tokens::list_scim_tokens(&pool, Some(idp_id)).await {
        Ok(tokens) => {
            let items: Vec<serde_json::Value> = tokens
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "idp_id": t.idp_id,
                        "token_prefix": t.token_prefix,
                        "name": t.name,
                        "created_by": t.created_by,
                        "enabled": t.enabled,
                        "last_used_at": t.last_used_at.map(|ts| ts.to_rfc3339()),
                        "created_at": t.created_at.to_rfc3339(),
                    })
                })
                .collect();
            Json(json!({ "tokens": items })).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// DELETE /admin/idps/{idp_id}/scim-tokens/{token_id} — Revoke a SCIM token
pub async fn revoke_scim_token(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path((_idp_id, token_id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match crate::db::scim_tokens::revoke_scim_token(&pool, token_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => admin_error(StatusCode::NOT_FOUND, "Token not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /admin/idps/{idp_id}/scim-admin-groups — Get admin group mappings
pub async fn get_scim_admin_groups(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(idp_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT scim_admin_groups FROM identity_providers WHERE id = $1",
    )
    .bind(idp_id)
    .fetch_optional(&pool)
    .await
    {
        Ok(Some(groups)) => Json(json!({ "groups": groups })).into_response(),
        Ok(None) => admin_error(StatusCode::NOT_FOUND, "IDP not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// PUT /admin/idps/{idp_id}/scim-admin-groups — Set admin group mappings
pub async fn set_scim_admin_groups(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(idp_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let groups = body.get("groups").cloned().unwrap_or(json!([]));

    // Validate it's an array of strings
    if !groups.is_array() || groups.as_array().unwrap().iter().any(|v| !v.is_string()) {
        return admin_error(
            StatusCode::BAD_REQUEST,
            "groups must be an array of strings",
        );
    }

    match sqlx::query("UPDATE identity_providers SET scim_admin_groups = $1 WHERE id = $2")
        .bind(&groups)
        .bind(idp_id)
        .execute(&pool)
        .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            crate::db::settings::bump_cache_version(&pool).await.ok();
            Json(json!({ "groups": groups })).into_response()
        }
        Ok(_) => admin_error(StatusCode::NOT_FOUND, "IDP not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /admin/teams/{team_id}/members — List team members
pub async fn list_team_members(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(team_id): Path<Uuid>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match crate::db::teams::get_team_members(&pool, team_id).await {
        Ok(members) => {
            let items: Vec<serde_json::Value> = members
                .iter()
                .map(|u| {
                    json!({
                        "id": u.id,
                        "email": u.email,
                        "role": u.role,
                        "active": u.active,
                        "created_at": u.created_at.to_rfc3339(),
                    })
                })
                .collect();
            Json(json!({ "members": items })).into_response()
        }
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// POST /admin/teams/{team_id}/members — Add a member to a team
pub async fn add_team_member(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(team_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let user_id = match body
        .get("user_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => id,
        None => return admin_error(StatusCode::BAD_REQUEST, "user_id is required"),
    };
    match crate::db::teams::add_team_member(&pool, team_id, user_id).await {
        Ok(true) => {
            crate::db::settings::bump_cache_version(&pool).await.ok();
            Json(json!({ "added": true })).into_response()
        }
        Ok(false) => admin_error(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// DELETE /admin/teams/{team_id}/members/{user_id} — Remove a member from a team
pub async fn remove_team_member(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path((team_id, user_id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    match crate::db::teams::remove_team_member(&pool, team_id, user_id).await {
        Ok(true) => {
            crate::db::settings::bump_cache_version(&pool).await.ok();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => admin_error(StatusCode::NOT_FOUND, "Member not found in team"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// PUT /admin/idps/{idp_id}/scim — Enable/disable SCIM for an IDP
pub async fn update_idp_scim(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(idp_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = check_admin_auth(&headers, &state).await {
        return resp;
    }
    let pool = state.db().await;
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match sqlx::query("UPDATE identity_providers SET scim_enabled = $1 WHERE id = $2")
        .bind(enabled)
        .bind(idp_id)
        .execute(&pool)
        .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            let _ = crate::db::settings::bump_cache_version(&pool).await;
            Json(json!({ "updated": true })).into_response()
        }
        Ok(_) => error_response(StatusCode::NOT_FOUND, "IDP not found"),
        Err(e) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
