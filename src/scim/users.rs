use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::proxy::GatewayState;
use crate::scim::auth::ScimAuth;
use crate::scim::filter::parse_filter;
use crate::scim::types::ScimUser;
use crate::scim::{SCIM_CONTENT_TYPE, ScimError, ScimListResponse};

/// Query params for GET /scim/v2/Users
#[derive(Debug, Deserialize)]
pub struct ListUsersParams {
    pub filter: Option<String>,
    #[serde(rename = "startIndex")]
    pub start_index: Option<i64>,
    pub count: Option<i64>,
}

/// Build a SCIM response with the correct Content-Type header.
fn scim_response<T: serde::Serialize>(status: StatusCode, body: &T) -> Result<Response, ScimError> {
    let json = serde_json::to_string(body).map_err(|e| ScimError::internal(e.to_string()))?;
    let mut resp = (status, json).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(SCIM_CONTENT_TYPE),
    );
    Ok(resp)
}

/// GET /scim/v2/Users — List/filter users
pub async fn list_users(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Query(params): Query<ListUsersParams>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Parse filter if present
    let filter = match params.filter.as_deref() {
        Some(f) => Some(parse_filter(f).map_err(ScimError::invalid_filter)?),
        None => None,
    };

    // SCIM pagination: startIndex is 1-based; convert to 0-based offset
    let start_index = params.start_index.unwrap_or(1).max(1);
    let limit = params.count.unwrap_or(100).clamp(1, 100);
    let offset = start_index - 1;

    let (users, total) =
        crate::db::users::list_users_for_idp(&pool, auth.idp_id, filter.as_ref(), offset, limit)
            .await
            .map_err(|e| ScimError::internal(format!("Database error: {e}")))?;

    let scim_users: Vec<ScimUser> = users.iter().map(ScimUser::from_db_user).collect();
    let list_resp = ScimListResponse::new(scim_users, total, start_index);
    scim_response(StatusCode::OK, &list_resp)
}

/// POST /scim/v2/Users — Create a user
pub async fn create_user(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Extract required userName
    let user_name = body
        .get("userName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ScimError::bad_request("userName is required"))?
        .to_string();

    // Optional fields
    let external_id = body.get("externalId").and_then(|v| v.as_str());
    let display_name = body.get("displayName").and_then(|v| v.as_str());
    let given_name = body
        .get("name")
        .and_then(|n| n.get("givenName"))
        .and_then(|v| v.as_str());
    let family_name = body
        .get("name")
        .and_then(|n| n.get("familyName"))
        .and_then(|v| v.as_str());

    // Check uniqueness by external_id within IDP
    if let Some(ext_id) = external_id
        && let Ok(Some(_)) =
            crate::db::users::get_user_by_external_id(&pool, ext_id, auth.idp_id).await
    {
        return Err(ScimError::conflict(format!(
            "User with externalId {ext_id} already exists"
        )));
    }

    // Check uniqueness by email
    if let Ok(Some(_)) = crate::db::users::get_user_by_email(&pool, &user_name).await {
        return Err(ScimError::conflict(format!(
            "User with userName {user_name} already exists"
        )));
    }

    // Get the IDP's default_role
    let role = get_idp_default_role(&pool, auth.idp_id).await?;

    let user = crate::db::users::create_scim_user(
        &pool,
        &user_name,
        external_id,
        display_name,
        given_name,
        family_name,
        &role,
        auth.idp_id,
    )
    .await
    .map_err(|e| ScimError::internal(format!("Failed to create user: {e}")))?;

    let scim_user = ScimUser::from_db_user(&user);
    let location = scim_user.meta.location.clone();

    let mut resp = scim_response(StatusCode::CREATED, &scim_user)?;
    if let Ok(header_value) = HeaderValue::from_str(&location) {
        resp.headers_mut()
            .insert(axum::http::header::LOCATION, header_value);
    }
    Ok(resp)
}

/// GET /scim/v2/Users/{id} — Get user by ID
pub async fn get_user(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    let user = sqlx::query_as::<_, crate::db::schema::User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
        .ok_or_else(|| ScimError::not_found("User not found"))?;

    // Verify scope: user must belong to this IDP or not be SCIM-managed
    if user.scim_managed && user.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("User not found"));
    }

    let scim_user = ScimUser::from_db_user(&user);
    scim_response(StatusCode::OK, &scim_user)
}

/// PUT /scim/v2/Users/{id} — Replace user
pub async fn replace_user(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Fetch existing user, verify scope
    let existing =
        sqlx::query_as::<_, crate::db::schema::User>("SELECT * FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&pool)
            .await
            .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
            .ok_or_else(|| ScimError::not_found("User not found"))?;

    if existing.scim_managed && existing.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("User not found"));
    }

    // Extract fields from body
    let user_name = body
        .get("userName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ScimError::bad_request("userName is required"))?
        .to_string();

    let external_id = body.get("externalId").and_then(|v| v.as_str());
    let display_name = body.get("displayName").and_then(|v| v.as_str());
    let given_name = body
        .get("name")
        .and_then(|n| n.get("givenName"))
        .and_then(|v| v.as_str());
    let family_name = body
        .get("name")
        .and_then(|n| n.get("familyName"))
        .and_then(|v| v.as_str());
    let active = match body.get("active") {
        Some(v) => parse_bool_value(v).unwrap_or(true),
        None => true,
    };

    let updated = crate::db::users::update_scim_user(
        &pool,
        id,
        &user_name,
        external_id,
        display_name,
        given_name,
        family_name,
        active,
    )
    .await
    .map_err(|e| ScimError::internal(format!("Failed to update user: {e}")))?
    .ok_or_else(|| ScimError::not_found("User not found"))?;

    let scim_user = ScimUser::from_db_user(&updated);
    scim_response(StatusCode::OK, &scim_user)
}

/// PATCH /scim/v2/Users/{id} — Partial update
pub async fn patch_user(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
    Json(body): Json<crate::scim::types::ScimPatchRequest>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Fetch existing user, verify scope
    let user = sqlx::query_as::<_, crate::db::schema::User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
        .ok_or_else(|| ScimError::not_found("User not found"))?;

    if user.scim_managed && user.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("User not found"));
    }

    // Apply patch operations to mutable working state
    let mut email = user.email.clone();
    let mut external_id = user.external_id.clone();
    let mut display_name = user.display_name.clone();
    let mut given_name = user.given_name.clone();
    let mut family_name = user.family_name.clone();
    let mut active = user.active;

    for op in &body.operations {
        let op_lower = op.op.to_lowercase();

        let value = op
            .value
            .as_ref()
            .ok_or_else(|| ScimError::bad_request("Missing 'value' in PATCH operation"))?;

        match op_lower.as_str() {
            "add" | "replace" => {
                let path = op.path.as_deref().unwrap_or("");

                if path.is_empty() {
                    // Entra ID path-less format: value is an object of attribute key/values
                    if let Some(obj) = value.as_object() {
                        for (key, val) in obj {
                            apply_patch_field(
                                key,
                                val,
                                &mut email,
                                &mut external_id,
                                &mut display_name,
                                &mut given_name,
                                &mut family_name,
                                &mut active,
                            )?;
                        }
                    } else {
                        return Err(ScimError::bad_request(
                            "PATCH operation with no path requires an object value",
                        ));
                    }
                } else {
                    apply_patch_field(
                        path,
                        value,
                        &mut email,
                        &mut external_id,
                        &mut display_name,
                        &mut given_name,
                        &mut family_name,
                        &mut active,
                    )?;
                }
            }
            "remove" => {
                // Clear the attribute at path (ignore missing path for remove)
                let path = op.path.as_deref().unwrap_or("");
                match path {
                    "active" => {
                        // Cannot remove active flag; ignore
                    }
                    "userName" => {
                        // Removing userName is not allowed (required field)
                        return Err(ScimError::bad_request(
                            "Cannot remove required attribute 'userName'",
                        ));
                    }
                    "displayName" | r#"emails[type eq "work"].value"# => {
                        display_name = None;
                    }
                    "name.givenName" => {
                        given_name = None;
                    }
                    "name.familyName" => {
                        family_name = None;
                    }
                    "externalId" => {
                        external_id = None;
                    }
                    _ => {
                        // Silently ignore unknown paths for remove (permissive)
                    }
                }
            }
            _ => {
                return Err(ScimError::bad_request(format!(
                    "Unsupported operation '{}' for User PATCH",
                    op.op
                )));
            }
        }
    }

    // Persist updated user
    let updated = crate::db::users::update_scim_user(
        &pool,
        id,
        &email,
        external_id.as_deref(),
        display_name.as_deref(),
        given_name.as_deref(),
        family_name.as_deref(),
        active,
    )
    .await
    .map_err(|e| ScimError::internal(format!("Failed to update user: {e}")))?
    .ok_or_else(|| ScimError::not_found("User not found"))?;

    let scim_user = ScimUser::from_db_user(&updated);
    scim_response(StatusCode::OK, &scim_user)
}

/// DELETE /scim/v2/Users/{id} — Soft-delete (deactivate)
pub async fn delete_user(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Verify user exists and is in scope
    let user = sqlx::query_as::<_, crate::db::schema::User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
        .ok_or_else(|| ScimError::not_found("User not found"))?;

    if user.scim_managed && user.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("User not found"));
    }

    crate::db::users::set_user_active(&pool, id, false)
        .await
        .map_err(|e| ScimError::internal(format!("Failed to deactivate user: {e}")))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Parse a boolean value that may be a JSON boolean or a string ("True"/"False").
/// Entra ID sends boolean values as strings in PATCH operations.
fn parse_bool_value(value: &serde_json::Value) -> Result<bool, ScimError> {
    if let Some(b) = value.as_bool() {
        return Ok(b);
    }
    if let Some(s) = value.as_str() {
        match s.to_lowercase().as_str() {
            "true" => return Ok(true),
            "false" => return Ok(false),
            _ => {}
        }
    }
    Err(ScimError::bad_request(
        "'active' value must be a boolean or boolean string",
    ))
}

/// Apply a single PATCH field update by key name.
///
/// Used for both path-based operations (`{"path": "active", "value": false}`) and
/// path-less object-value operations (`{"value": {"active": false, ...}}`).
#[allow(clippy::too_many_arguments)]
fn apply_patch_field(
    key: &str,
    value: &serde_json::Value,
    email: &mut String,
    external_id: &mut Option<String>,
    display_name: &mut Option<String>,
    given_name: &mut Option<String>,
    family_name: &mut Option<String>,
    active: &mut bool,
) -> Result<(), ScimError> {
    match key {
        "active" => {
            *active = parse_bool_value(value)?;
        }
        "userName" | r#"emails[type eq "work"].value"# => {
            *email = value
                .as_str()
                .ok_or_else(|| ScimError::bad_request("Email/userName value must be a string"))?
                .to_string();
        }
        "displayName" => {
            *display_name = value.as_str().map(str::to_string);
        }
        "name.givenName" => {
            *given_name = value.as_str().map(str::to_string);
        }
        "name.familyName" => {
            *family_name = value.as_str().map(str::to_string);
        }
        "externalId" => {
            *external_id = value.as_str().map(str::to_string);
        }
        "name" => {
            // Entra ID path-less format may include a nested "name" object
            if let Some(obj) = value.as_object() {
                if let Some(gn) = obj.get("givenName") {
                    *given_name = gn.as_str().map(str::to_string);
                }
                if let Some(fn_) = obj.get("familyName") {
                    *family_name = fn_.as_str().map(str::to_string);
                }
            }
        }
        _ => {
            // Unknown path — return an error so callers know something was unexpected
            return Err(ScimError::bad_request(format!(
                "Unsupported PATCH path: '{key}'"
            )));
        }
    }
    Ok(())
}

/// Look up the default_role for an IDP.
async fn get_idp_default_role(pool: &sqlx::PgPool, idp_id: Uuid) -> Result<String, ScimError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT default_role FROM identity_providers WHERE id = $1")
            .bind(idp_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ScimError::internal(format!("Database error: {e}")))?;

    Ok(row.map(|(r,)| r).unwrap_or_else(|| "member".to_string()))
}
