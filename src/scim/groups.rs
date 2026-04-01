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
use crate::scim::types::{ScimGroup, ScimMemberRef};
use crate::scim::{SCIM_CONTENT_TYPE, ScimError, ScimListResponse};

/// Query params for GET /scim/v2/Groups
#[derive(Debug, Deserialize)]
pub struct ListGroupsParams {
    pub filter: Option<String>,
    #[serde(rename = "startIndex")]
    pub start_index: Option<i64>,
    pub count: Option<i64>,
    #[serde(rename = "excludedAttributes")]
    pub excluded_attributes: Option<String>,
}

/// Query params for GET /scim/v2/Groups/{id}
#[derive(Debug, Deserialize)]
pub struct GetGroupParams {
    #[serde(rename = "excludedAttributes")]
    pub excluded_attributes: Option<String>,
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

/// Check whether "members" is excluded via the `excludedAttributes` query param.
fn members_excluded(excluded_attributes: Option<&str>) -> bool {
    excluded_attributes
        .map(|s| s.split(',').any(|attr| attr.trim() == "members"))
        .unwrap_or(false)
}

/// Fetch and populate member list for a ScimGroup.
async fn populate_members(
    pool: &sqlx::PgPool,
    group: &mut ScimGroup,
    team_id: Uuid,
) -> Result<(), ScimError> {
    let members = crate::db::teams::get_team_members(pool, team_id)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?;
    group.members = Some(
        members
            .iter()
            .map(|u| ScimMemberRef {
                value: u.id.to_string(),
                display: Some(u.email.clone()),
                ref_uri: Some(format!("/scim/v2/Users/{}", u.id)),
            })
            .collect(),
    );
    Ok(())
}

/// Extract member UUIDs from a JSON array of `{"value": "<uuid>"}` objects.
fn extract_member_ids(value: &serde_json::Value) -> Result<Vec<Uuid>, ScimError> {
    let arr = value
        .as_array()
        .ok_or_else(|| ScimError::bad_request("members value must be an array"))?;
    arr.iter()
        .map(|item| {
            let id_str = item
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ScimError::bad_request("member must have a 'value' field"))?;
            Uuid::parse_str(id_str)
                .map_err(|_| ScimError::bad_request(format!("Invalid member ID: {id_str}")))
        })
        .collect()
}

/// Parse UUID from a path filter expression like `members[value eq "uuid"]`.
fn extract_member_id_from_path(path: &str) -> Option<Uuid> {
    let inner = path
        .strip_prefix("members[value eq \"")?
        .strip_suffix("\"]")?;
    Uuid::parse_str(inner).ok()
}

/// GET /scim/v2/Groups — List/filter groups
pub async fn list_groups(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Query(params): Query<ListGroupsParams>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    let filter = match params.filter.as_deref() {
        Some(f) => Some(parse_filter(f).map_err(ScimError::invalid_filter)?),
        None => None,
    };

    let start_index = params.start_index.unwrap_or(1).max(1);
    let limit = params.count.unwrap_or(100).clamp(1, 100);
    let offset = start_index - 1;

    let (teams, total) =
        crate::db::teams::list_teams_for_idp(&pool, auth.idp_id, filter.as_ref(), offset, limit)
            .await
            .map_err(|e| ScimError::internal(format!("Database error: {e}")))?;

    let exclude_members = members_excluded(params.excluded_attributes.as_deref());

    let mut scim_groups: Vec<ScimGroup> = teams.iter().map(ScimGroup::from_db_team).collect();

    if !exclude_members {
        for (group, team) in scim_groups.iter_mut().zip(teams.iter()) {
            populate_members(&pool, group, team.id).await?;
        }
    }

    let list_resp = ScimListResponse::new(scim_groups, total, start_index);
    scim_response(StatusCode::OK, &list_resp)
}

/// POST /scim/v2/Groups — Create a group (CCAG team)
pub async fn create_group(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    let display_name = body
        .get("displayName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ScimError::bad_request("displayName is required"))?
        .to_string();

    let external_id = body.get("externalId").and_then(|v| v.as_str());

    // Check uniqueness by external_id within IDP
    if let Some(ext_id) = external_id
        && let Ok(Some(_)) =
            crate::db::teams::get_team_by_external_id(&pool, ext_id, auth.idp_id).await
    {
        return Err(ScimError::conflict(format!(
            "Group with externalId {ext_id} already exists"
        )));
    }

    let team = crate::db::teams::create_scim_team(
        &pool,
        &display_name,
        external_id,
        Some(&display_name),
        auth.idp_id,
    )
    .await
    .map_err(|e| ScimError::internal(format!("Failed to create group: {e}")))?;

    // Assign initial members if provided
    if let Some(members_val) = body.get("members")
        && !members_val.is_null()
    {
        let member_ids = extract_member_ids(members_val)?;
        if !member_ids.is_empty() {
            crate::db::teams::set_team_members(&pool, team.id, &member_ids)
                .await
                .map_err(|e| ScimError::internal(format!("Failed to set members: {e}")))?;
        }
    }

    let mut scim_group = ScimGroup::from_db_team(&team);
    populate_members(&pool, &mut scim_group, team.id).await?;

    let location = scim_group.meta.location.clone();
    let mut resp = scim_response(StatusCode::CREATED, &scim_group)?;
    if let Ok(header_value) = HeaderValue::from_str(&location) {
        resp.headers_mut()
            .insert(axum::http::header::LOCATION, header_value);
    }
    Ok(resp)
}

/// GET /scim/v2/Groups/{id} — Get group by ID
pub async fn get_group(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
    Query(params): Query<GetGroupParams>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    let team = crate::db::teams::get_team(&pool, id)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
        .ok_or_else(|| ScimError::not_found("Group not found"))?;

    // Verify scope
    if team.scim_managed && team.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("Group not found"));
    }

    let mut scim_group = ScimGroup::from_db_team(&team);

    let exclude_members = members_excluded(params.excluded_attributes.as_deref());
    if !exclude_members {
        populate_members(&pool, &mut scim_group, team.id).await?;
    }

    scim_response(StatusCode::OK, &scim_group)
}

/// PUT /scim/v2/Groups/{id} — Full replacement including member list
pub async fn replace_group(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Fetch existing team, verify scope
    let existing = crate::db::teams::get_team(&pool, id)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
        .ok_or_else(|| ScimError::not_found("Group not found"))?;

    if existing.scim_managed && existing.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("Group not found"));
    }

    let display_name = body
        .get("displayName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ScimError::bad_request("displayName is required"))?
        .to_string();

    let external_id = body.get("externalId").and_then(|v| v.as_str());

    let team = crate::db::teams::update_scim_team(
        &pool,
        id,
        &display_name,
        external_id,
        Some(&display_name),
    )
    .await
    .map_err(|e| ScimError::internal(format!("Failed to update group: {e}")))?
    .ok_or_else(|| ScimError::not_found("Group not found"))?;

    // Replace member list atomically
    let member_ids = match body.get("members") {
        Some(v) if !v.is_null() => extract_member_ids(v)?,
        _ => vec![],
    };
    crate::db::teams::set_team_members(&pool, id, &member_ids)
        .await
        .map_err(|e| ScimError::internal(format!("Failed to set members: {e}")))?;

    let mut scim_group = ScimGroup::from_db_team(&team);
    populate_members(&pool, &mut scim_group, id).await?;

    scim_response(StatusCode::OK, &scim_group)
}

/// PATCH /scim/v2/Groups/{id} — Partial update
/// Returns 204 No Content (Entra ID requirement).
pub async fn patch_group(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
    Json(body): Json<crate::scim::types::ScimPatchRequest>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Fetch existing team, verify scope
    let team = crate::db::teams::get_team(&pool, id)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
        .ok_or_else(|| ScimError::not_found("Group not found"))?;

    if team.scim_managed && team.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("Group not found"));
    }

    let mut name = team
        .display_name
        .clone()
        .unwrap_or_else(|| team.name.clone());
    let mut external_id = team.external_id.clone();

    for op in &body.operations {
        let op_lower = op.op.to_lowercase();

        match op_lower.as_str() {
            "replace" | "add" => {
                let path = op.path.as_deref().unwrap_or("");

                if path.is_empty() {
                    // Path-less format (Entra ID / Okta): value is an object
                    let value = op.value.as_ref().ok_or_else(|| {
                        ScimError::bad_request("Missing 'value' in PATCH operation")
                    })?;
                    if let Some(obj) = value.as_object() {
                        for (key, val) in obj {
                            match key.as_str() {
                                "displayName" => {
                                    name = val
                                        .as_str()
                                        .ok_or_else(|| {
                                            ScimError::bad_request("displayName must be a string")
                                        })?
                                        .to_string();
                                }
                                "externalId" => {
                                    external_id = val.as_str().map(str::to_string);
                                }
                                _ => {
                                    // Ignore unknown attributes in path-less format
                                }
                            }
                        }
                    } else {
                        return Err(ScimError::bad_request(
                            "PATCH operation with no path requires an object value",
                        ));
                    }
                } else if path == "displayName" {
                    let value = op.value.as_ref().ok_or_else(|| {
                        ScimError::bad_request("Missing 'value' in PATCH operation")
                    })?;
                    name = value
                        .as_str()
                        .ok_or_else(|| ScimError::bad_request("displayName must be a string"))?
                        .to_string();
                } else if path == "externalId" {
                    let value = op.value.as_ref().ok_or_else(|| {
                        ScimError::bad_request("Missing 'value' in PATCH operation")
                    })?;
                    external_id = value.as_str().map(str::to_string);
                } else if path == "members" {
                    // Add or replace members
                    let value = op.value.as_ref().ok_or_else(|| {
                        ScimError::bad_request("Missing 'value' in PATCH operation")
                    })?;
                    let member_ids = extract_member_ids(value)?;
                    if op_lower == "replace" {
                        // Replace all members
                        crate::db::teams::set_team_members(&pool, id, &member_ids)
                            .await
                            .map_err(|e| {
                                ScimError::internal(format!("Failed to set members: {e}"))
                            })?;
                    } else {
                        // Add members
                        for uid in member_ids {
                            crate::db::teams::add_team_member(&pool, id, uid)
                                .await
                                .map_err(|e| {
                                    ScimError::internal(format!("Failed to add member: {e}"))
                                })?;
                        }
                    }
                } else {
                    // Unknown path — ignore for permissive IdP compatibility
                }
            }
            "remove" => {
                let path = op.path.as_deref().unwrap_or("");

                // Check for filter path: members[value eq "uuid"]
                if let Some(user_id) = extract_member_id_from_path(path) {
                    crate::db::teams::remove_team_member(&pool, id, user_id)
                        .await
                        .map_err(|e| {
                            ScimError::internal(format!("Failed to remove member: {e}"))
                        })?;
                } else if path == "members" {
                    // Remove with value array
                    if let Some(value) = &op.value {
                        let member_ids = extract_member_ids(value)?;
                        for uid in member_ids {
                            crate::db::teams::remove_team_member(&pool, id, uid)
                                .await
                                .map_err(|e| {
                                    ScimError::internal(format!("Failed to remove member: {e}"))
                                })?;
                        }
                    } else {
                        // Remove all members
                        crate::db::teams::set_team_members(&pool, id, &[])
                            .await
                            .map_err(|e| {
                                ScimError::internal(format!("Failed to clear members: {e}"))
                            })?;
                    }
                } else {
                    // Unknown remove path — ignore
                }
            }
            _ => {
                return Err(ScimError::bad_request(format!(
                    "Unsupported operation '{}' for Group PATCH",
                    op.op
                )));
            }
        }
    }

    // Persist name / externalId changes
    crate::db::teams::update_scim_team(&pool, id, &name, external_id.as_deref(), Some(&name))
        .await
        .map_err(|e| ScimError::internal(format!("Failed to update group: {e}")))?
        .ok_or_else(|| ScimError::not_found("Group not found"))?;

    // Return 204 No Content (Entra ID requirement)
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// DELETE /scim/v2/Groups/{id} — Unassign all members and delete the team
pub async fn delete_group(
    State(state): State<Arc<GatewayState>>,
    auth: ScimAuth,
    Path(id): Path<Uuid>,
) -> Result<Response, ScimError> {
    let pool = state.db().await;

    // Verify the team exists and is in scope
    let team = crate::db::teams::get_team(&pool, id)
        .await
        .map_err(|e| ScimError::internal(format!("Database error: {e}")))?
        .ok_or_else(|| ScimError::not_found("Group not found"))?;

    if team.scim_managed && team.idp_id != Some(auth.idp_id) {
        return Err(ScimError::not_found("Group not found"));
    }

    // Unassign all members first
    crate::db::teams::set_team_members(&pool, id, &[])
        .await
        .map_err(|e| ScimError::internal(format!("Failed to unassign members: {e}")))?;

    // Delete the team
    crate::db::teams::delete_team(&pool, id)
        .await
        .map_err(|e| ScimError::internal(format!("Failed to delete group: {e}")))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}
