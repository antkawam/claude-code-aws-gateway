use sqlx::PgPool;
use uuid::Uuid;

use super::schema::{ScimGroupRow, User};
use crate::scim::filter::ScimFilter;

/// Create a SCIM group.
pub async fn create_scim_group(
    pool: &PgPool,
    display_name: &str,
    external_id: Option<&str>,
    idp_id: Uuid,
) -> anyhow::Result<ScimGroupRow> {
    let row = sqlx::query_as::<_, ScimGroupRow>(
        r#"INSERT INTO scim_groups (display_name, external_id, idp_id)
           VALUES ($1, $2, $3)
           RETURNING *"#,
    )
    .bind(display_name)
    .bind(external_id)
    .bind(idp_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Get a SCIM group by ID.
pub async fn get_scim_group(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<ScimGroupRow>> {
    let row = sqlx::query_as::<_, ScimGroupRow>("SELECT * FROM scim_groups WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Get a SCIM group by external_id scoped to an IDP.
pub async fn get_scim_group_by_external_id(
    pool: &PgPool,
    external_id: &str,
    idp_id: Uuid,
) -> anyhow::Result<Option<ScimGroupRow>> {
    let row = sqlx::query_as::<_, ScimGroupRow>(
        "SELECT * FROM scim_groups WHERE external_id = $1 AND idp_id = $2",
    )
    .bind(external_id)
    .bind(idp_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Update a SCIM group (display_name, external_id).
pub async fn update_scim_group(
    pool: &PgPool,
    id: Uuid,
    display_name: &str,
    external_id: Option<&str>,
) -> anyhow::Result<Option<ScimGroupRow>> {
    let row = sqlx::query_as::<_, ScimGroupRow>(
        r#"UPDATE scim_groups SET
            display_name = $2,
            external_id = $3
            WHERE id = $1
            RETURNING *"#,
    )
    .bind(id)
    .bind(display_name)
    .bind(external_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete a SCIM group. CASCADE removes members from scim_group_members.
pub async fn delete_scim_group(pool: &PgPool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM scim_groups WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// List SCIM groups for an IDP with optional filter and pagination.
/// Returns (groups, total_count).
pub async fn list_scim_groups_for_idp(
    pool: &PgPool,
    idp_id: Uuid,
    filter: Option<&ScimFilter>,
    offset: i64,
    limit: i64,
) -> anyhow::Result<(Vec<ScimGroupRow>, i64)> {
    let (filter_clause, filter_values) = build_filter_clause(filter, 2);

    let base_where = format!("idp_id = $1{filter_clause}");

    // Count query
    let count_sql = format!("SELECT COUNT(*) FROM scim_groups WHERE {base_where}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql).bind(idp_id);
    for v in &filter_values {
        count_query = count_query.bind(v.as_str());
    }
    let total: i64 = count_query.fetch_one(pool).await?;

    // Data query
    let next_param = 2 + filter_values.len();
    let data_sql = format!(
        "SELECT * FROM scim_groups WHERE {base_where} ORDER BY display_name LIMIT ${next_param} OFFSET ${}",
        next_param + 1
    );
    let mut data_query = sqlx::query_as::<_, ScimGroupRow>(&data_sql).bind(idp_id);
    for v in &filter_values {
        data_query = data_query.bind(v.as_str());
    }
    data_query = data_query.bind(limit).bind(offset);

    let groups = data_query.fetch_all(pool).await?;
    Ok((groups, total))
}

/// Build a SQL WHERE clause fragment (with leading AND) and bind values from a ScimFilter.
fn build_filter_clause(filter: Option<&ScimFilter>, start_param: usize) -> (String, Vec<String>) {
    match filter {
        None => (String::new(), vec![]),
        Some(f) => {
            let mut values: Vec<String> = Vec::new();
            let clause = build_filter_expr(f, start_param, &mut values);
            (format!(" AND ({clause})"), values)
        }
    }
}

fn build_filter_expr(filter: &ScimFilter, start_param: usize, values: &mut Vec<String>) -> String {
    match filter {
        ScimFilter::Eq(attr, val) => {
            let idx = start_param + values.len();
            match attr.as_str() {
                "displayName" => {
                    values.push(val.clone());
                    format!("LOWER(display_name) = LOWER(${idx})")
                }
                "externalId" => {
                    values.push(val.clone());
                    format!("external_id = ${idx}")
                }
                _ => "false".to_string(),
            }
        }
        ScimFilter::Contains(attr, val) => {
            let idx = start_param + values.len();
            match attr.as_str() {
                "displayName" => {
                    values.push(val.clone());
                    format!("LOWER(display_name) LIKE '%' || LOWER(${idx}) || '%'")
                }
                _ => "false".to_string(),
            }
        }
        ScimFilter::StartsWith(attr, val) => {
            let idx = start_param + values.len();
            match attr.as_str() {
                "displayName" => {
                    values.push(val.clone());
                    format!("LOWER(display_name) LIKE LOWER(${idx}) || '%'")
                }
                _ => "false".to_string(),
            }
        }
        ScimFilter::And(left, right) => {
            let left_expr = build_filter_expr(left, start_param, values);
            let right_expr = build_filter_expr(right, start_param, values);
            format!("({left_expr}) AND ({right_expr})")
        }
    }
}

/// Get all members of a SCIM group.
pub async fn get_scim_group_members(pool: &PgPool, group_id: Uuid) -> anyhow::Result<Vec<User>> {
    let users = sqlx::query_as::<_, User>(
        r#"SELECT u.* FROM users u
           JOIN scim_group_members m ON u.id = m.user_id
           WHERE m.group_id = $1
           ORDER BY u.email"#,
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(users)
}

/// Set the full member list for a SCIM group (atomic replace).
pub async fn set_scim_group_members(
    pool: &PgPool,
    group_id: Uuid,
    user_ids: &[Uuid],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM scim_group_members WHERE group_id = $1")
        .bind(group_id)
        .execute(&mut *tx)
        .await?;

    for user_id in user_ids {
        sqlx::query(
            "INSERT INTO scim_group_members (group_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(group_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Add a single member to a SCIM group.
/// Returns true if the row was inserted (false if already present).
pub async fn add_scim_group_member(
    pool: &PgPool,
    group_id: Uuid,
    user_id: Uuid,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "INSERT INTO scim_group_members (group_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(group_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Remove a single member from a SCIM group.
/// Returns true if the row was deleted.
pub async fn remove_scim_group_member(
    pool: &PgPool,
    group_id: Uuid,
    user_id: Uuid,
) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM scim_group_members WHERE group_id = $1 AND user_id = $2")
        .bind(group_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Evaluate what role a user should have based on their SCIM group memberships.
///
/// If the user belongs to any group whose display_name is listed in the IDP's
/// `scim_admin_groups` JSONB array, return `"admin"`. Otherwise return the IDP's
/// `default_role`.
pub async fn evaluate_user_role(
    pool: &PgPool,
    user_id: Uuid,
    idp_id: Uuid,
) -> anyhow::Result<String> {
    // Fetch the IDP's scim_admin_groups and default_role.
    let row: Option<(serde_json::Value, String)> = sqlx::query_as(
        "SELECT scim_admin_groups, default_role FROM identity_providers WHERE id = $1",
    )
    .bind(idp_id)
    .fetch_optional(pool)
    .await?;

    let (admin_groups_json, default_role) = match row {
        Some(r) => r,
        None => return Ok("member".to_string()),
    };

    // Parse the JSONB array into a Vec<String>.
    let admin_groups: Vec<String> = match admin_groups_json {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
            .collect(),
        _ => vec![],
    };

    if admin_groups.is_empty() {
        return Ok(default_role);
    }

    // Get all scim_groups this user belongs to for this IDP.
    let group_names: Vec<String> = sqlx::query_scalar(
        r#"SELECT g.display_name FROM scim_groups g
           JOIN scim_group_members m ON g.id = m.group_id
           WHERE g.idp_id = $1 AND m.user_id = $2"#,
    )
    .bind(idp_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    for name in &group_names {
        if admin_groups.contains(&name.to_lowercase()) {
            return Ok("admin".to_string());
        }
    }

    Ok(default_role)
}

/// Re-evaluate and update a user's role in the DB.
pub async fn sync_user_role(pool: &PgPool, user_id: Uuid, idp_id: Uuid) -> anyhow::Result<()> {
    let role = evaluate_user_role(pool, user_id, idp_id).await?;
    sqlx::query("UPDATE users SET role = $1 WHERE id = $2")
        .bind(&role)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Get SCIM groups a user belongs to (for populating ScimUser.groups field).
pub async fn get_user_scim_groups(
    pool: &PgPool,
    user_id: Uuid,
) -> anyhow::Result<Vec<ScimGroupRow>> {
    let rows = sqlx::query_as::<_, ScimGroupRow>(
        r#"SELECT g.* FROM scim_groups g
           JOIN scim_group_members m ON g.id = m.group_id
           WHERE m.user_id = $1"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
