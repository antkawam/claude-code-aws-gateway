use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use super::schema::User;
use crate::scim::filter::ScimFilter;

pub async fn create_user(
    pool: &PgPool,
    email: &str,
    team_id: Option<Uuid>,
    role: &str,
) -> anyhow::Result<User> {
    let user = sqlx::query_as::<_, User>(
        "INSERT INTO users (email, team_id, role) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(email)
    .bind(team_id)
    .bind(role)
    .fetch_one(pool)
    .await?;
    Ok(user)
}

pub async fn list_users(pool: &PgPool) -> anyhow::Result<Vec<User>> {
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY email")
        .fetch_all(pool)
        .await?;
    Ok(users)
}

/// Fetch a map of user id -> email for the given set of ids.
pub async fn get_emails_by_ids(
    pool: &PgPool,
    ids: &[Uuid],
) -> anyhow::Result<HashMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, email FROM users WHERE id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn get_user_by_email(pool: &PgPool, email: &str) -> anyhow::Result<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

pub async fn update_user_role(pool: &PgPool, id: Uuid, role: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE users SET role = $1 WHERE id = $2")
        .bind(role)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn update_user_team(
    pool: &PgPool,
    id: Uuid,
    team_id: Option<Uuid>,
) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE users SET team_id = $1 WHERE id = $2")
        .bind(team_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn count_users_without_team(pool: &PgPool) -> anyhow::Result<i64> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE team_id IS NULL")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

pub async fn delete_user(pool: &PgPool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Get a user by their external_id, scoped to a specific IDP.
pub async fn get_user_by_external_id(
    pool: &PgPool,
    external_id: &str,
    idp_id: Uuid,
) -> anyhow::Result<Option<User>> {
    let user =
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE external_id = $1 AND idp_id = $2")
            .bind(external_id)
            .bind(idp_id)
            .fetch_optional(pool)
            .await?;
    Ok(user)
}

/// Create a SCIM-provisioned user with all SCIM fields.
#[allow(clippy::too_many_arguments)]
pub async fn create_scim_user(
    pool: &PgPool,
    email: &str,
    external_id: Option<&str>,
    display_name: Option<&str>,
    given_name: Option<&str>,
    family_name: Option<&str>,
    role: &str,
    idp_id: Uuid,
) -> anyhow::Result<User> {
    let user = sqlx::query_as::<_, User>(
        r#"INSERT INTO users
            (email, external_id, display_name, given_name, family_name, role, idp_id, scim_managed, active)
            VALUES ($1, $2, $3, $4, $5, $6, $7, true, true)
            RETURNING *"#,
    )
    .bind(email)
    .bind(external_id)
    .bind(display_name)
    .bind(given_name)
    .bind(family_name)
    .bind(role)
    .bind(idp_id)
    .fetch_one(pool)
    .await?;
    Ok(user)
}

/// Full update of a SCIM user (for PUT).
#[allow(clippy::too_many_arguments)]
pub async fn update_scim_user(
    pool: &PgPool,
    id: Uuid,
    email: &str,
    external_id: Option<&str>,
    display_name: Option<&str>,
    given_name: Option<&str>,
    family_name: Option<&str>,
    active: bool,
) -> anyhow::Result<Option<User>> {
    let user = sqlx::query_as::<_, User>(
        r#"UPDATE users SET
            email = $2,
            external_id = $3,
            display_name = $4,
            given_name = $5,
            family_name = $6,
            active = $7
            WHERE id = $1
            RETURNING *"#,
    )
    .bind(id)
    .bind(email)
    .bind(external_id)
    .bind(display_name)
    .bind(given_name)
    .bind(family_name)
    .bind(active)
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// Set only the active flag (for PATCH deactivation and DELETE).
pub async fn set_user_active(pool: &PgPool, id: Uuid, active: bool) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE users SET active = $1 WHERE id = $2")
        .bind(active)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// List users scoped to an IDP, with optional SCIM filter and pagination.
/// Returns (users, total_count).
pub async fn list_users_for_idp(
    pool: &PgPool,
    idp_id: Uuid,
    filter: Option<&ScimFilter>,
    offset: i64,
    limit: i64,
) -> anyhow::Result<(Vec<User>, i64)> {
    // Build the filter clause and collect string values to bind.
    // We use a separate enum to track the type of each bind value.
    let (filter_clause, filter_values) = build_filter_clause(filter, 2);

    let base_where = format!("(idp_id = $1 OR scim_managed = false){}", filter_clause);

    // Count query
    let count_sql = format!("SELECT COUNT(*) FROM users WHERE {base_where}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql).bind(idp_id);
    for v in &filter_values {
        count_query = count_query.bind(v.as_str());
    }
    let total: i64 = count_query.fetch_one(pool).await?;

    // Data query
    let next_param = 2 + filter_values.len();
    let data_sql = format!(
        "SELECT * FROM users WHERE {base_where} ORDER BY email LIMIT ${next_param} OFFSET ${}",
        next_param + 1
    );
    let mut data_query = sqlx::query_as::<_, User>(&data_sql).bind(idp_id);
    for v in &filter_values {
        data_query = data_query.bind(v.as_str());
    }
    data_query = data_query.bind(limit).bind(offset);

    let users = data_query.fetch_all(pool).await?;
    Ok((users, total))
}

/// Build a SQL WHERE clause fragment (with leading AND) and the list of bind values
/// from a `ScimFilter`. Parameters start at `$start_param`.
///
/// Returns `("", vec![])` when filter is None.
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

/// Recursively build the SQL expression for a ScimFilter, binding values into `values`.
fn build_filter_expr(filter: &ScimFilter, start_param: usize, values: &mut Vec<String>) -> String {
    match filter {
        ScimFilter::Eq(attr, val) => {
            let idx = start_param + values.len();
            match attr.as_str() {
                "userName" => {
                    values.push(val.clone());
                    format!("LOWER(email) = LOWER(${idx})")
                }
                "externalId" => {
                    values.push(val.clone());
                    format!("external_id = ${idx}")
                }
                "active" => {
                    // Boolean: do not add to bind values, inline directly
                    let bool_val = if val.eq_ignore_ascii_case("true") {
                        "true"
                    } else {
                        "false"
                    };
                    format!("active = {bool_val}")
                }
                _ => {
                    // Unknown attribute — produce a clause that matches nothing
                    "false".to_string()
                }
            }
        }
        ScimFilter::Contains(attr, val) => {
            let idx = start_param + values.len();
            match attr.as_str() {
                "userName" => {
                    values.push(val.clone());
                    format!("LOWER(email) LIKE '%' || LOWER(${idx}) || '%'")
                }
                _ => "false".to_string(),
            }
        }
        ScimFilter::StartsWith(attr, val) => {
            let idx = start_param + values.len();
            match attr.as_str() {
                "userName" => {
                    values.push(val.clone());
                    format!("LOWER(email) LIKE LOWER(${idx}) || '%'")
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
