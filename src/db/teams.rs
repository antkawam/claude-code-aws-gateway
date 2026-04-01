use sqlx::PgPool;
use uuid::Uuid;

use super::schema::Team;
use crate::scim::filter::ScimFilter;

pub async fn create_team(pool: &PgPool, name: &str) -> anyhow::Result<Team> {
    let team = sqlx::query_as::<_, Team>("INSERT INTO teams (name) VALUES ($1) RETURNING *")
        .bind(name)
        .fetch_one(pool)
        .await?;
    Ok(team)
}

pub async fn list_teams(pool: &PgPool) -> anyhow::Result<Vec<Team>> {
    let teams = sqlx::query_as::<_, Team>("SELECT * FROM teams ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(teams)
}

pub async fn get_team(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<Team>> {
    let team = sqlx::query_as::<_, Team>("SELECT * FROM teams WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(team)
}

pub async fn delete_team(pool: &PgPool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM teams WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Update team routing strategy.
pub async fn update_team_routing_strategy(
    pool: &PgPool,
    id: Uuid,
    routing_strategy: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE teams SET routing_strategy = $1 WHERE id = $2")
        .bind(routing_strategy)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Get a team by external_id scoped to an IDP.
pub async fn get_team_by_external_id(
    pool: &PgPool,
    external_id: &str,
    idp_id: Uuid,
) -> anyhow::Result<Option<Team>> {
    let team =
        sqlx::query_as::<_, Team>("SELECT * FROM teams WHERE external_id = $1 AND idp_id = $2")
            .bind(external_id)
            .bind(idp_id)
            .fetch_optional(pool)
            .await?;
    Ok(team)
}

/// Create a SCIM-provisioned team.
pub async fn create_scim_team(
    pool: &PgPool,
    name: &str,
    external_id: Option<&str>,
    display_name: Option<&str>,
    idp_id: Uuid,
) -> anyhow::Result<Team> {
    let team = sqlx::query_as::<_, Team>(
        r#"INSERT INTO teams (name, external_id, display_name, scim_managed, idp_id)
            VALUES ($1, $2, COALESCE($3, $1), true, $4)
            RETURNING *"#,
    )
    .bind(name)
    .bind(external_id)
    .bind(display_name)
    .bind(idp_id)
    .fetch_one(pool)
    .await?;
    Ok(team)
}

/// Update a SCIM team (name, external_id, display_name).
pub async fn update_scim_team(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    external_id: Option<&str>,
    display_name: Option<&str>,
) -> anyhow::Result<Option<Team>> {
    let team = sqlx::query_as::<_, Team>(
        r#"UPDATE teams SET
            name = $2,
            external_id = $3,
            display_name = $4
            WHERE id = $1
            RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(external_id)
    .bind(display_name)
    .fetch_optional(pool)
    .await?;
    Ok(team)
}

/// Get all users in a team (for member list).
pub async fn get_team_members(
    pool: &PgPool,
    team_id: Uuid,
) -> anyhow::Result<Vec<crate::db::schema::User>> {
    let users = sqlx::query_as::<_, crate::db::schema::User>(
        "SELECT * FROM users WHERE team_id = $1 ORDER BY email",
    )
    .bind(team_id)
    .fetch_all(pool)
    .await?;
    Ok(users)
}

/// Set the full member list for a team (atomic replace).
/// Clears existing members first, then assigns new ones.
pub async fn set_team_members(
    pool: &PgPool,
    team_id: Uuid,
    user_ids: &[Uuid],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    // Clear current members
    sqlx::query("UPDATE users SET team_id = NULL WHERE team_id = $1")
        .bind(team_id)
        .execute(&mut *tx)
        .await?;
    // Assign new members (if any)
    if !user_ids.is_empty() {
        sqlx::query("UPDATE users SET team_id = $1 WHERE id = ANY($2)")
            .bind(team_id)
            .bind(user_ids)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Add a single member to a team.
/// Returns true if the user was found and updated.
pub async fn add_team_member(pool: &PgPool, team_id: Uuid, user_id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE users SET team_id = $1 WHERE id = $2")
        .bind(team_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Remove a single member from a team.
/// Returns true if the user was a member of this team and was removed.
pub async fn remove_team_member(
    pool: &PgPool,
    team_id: Uuid,
    user_id: Uuid,
) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE users SET team_id = NULL WHERE id = $1 AND team_id = $2")
        .bind(user_id)
        .bind(team_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// List teams scoped to an IDP, with optional filter and pagination.
/// Returns (teams, total_count).
pub async fn list_teams_for_idp(
    pool: &PgPool,
    idp_id: Uuid,
    filter: Option<&ScimFilter>,
    offset: i64,
    limit: i64,
) -> anyhow::Result<(Vec<Team>, i64)> {
    let (filter_clause, filter_values) = build_filter_clause(filter, 2);

    let base_where = format!("idp_id = $1{filter_clause}");

    // Count query
    let count_sql = format!("SELECT COUNT(*) FROM teams WHERE {base_where}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql).bind(idp_id);
    for v in &filter_values {
        count_query = count_query.bind(v.as_str());
    }
    let total: i64 = count_query.fetch_one(pool).await?;

    // Data query
    let next_param = 2 + filter_values.len();
    let data_sql = format!(
        "SELECT * FROM teams WHERE {base_where} ORDER BY name LIMIT ${next_param} OFFSET ${}",
        next_param + 1
    );
    let mut data_query = sqlx::query_as::<_, Team>(&data_sql).bind(idp_id);
    for v in &filter_values {
        data_query = data_query.bind(v.as_str());
    }
    data_query = data_query.bind(limit).bind(offset);

    let teams = data_query.fetch_all(pool).await?;
    Ok((teams, total))
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
                    format!("LOWER(name) = LOWER(${idx})")
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
                    format!("LOWER(name) LIKE '%' || LOWER(${idx}) || '%'")
                }
                _ => "false".to_string(),
            }
        }
        ScimFilter::StartsWith(attr, val) => {
            let idx = start_param + values.len();
            match attr.as_str() {
                "displayName" => {
                    values.push(val.clone());
                    format!("LOWER(name) LIKE LOWER(${idx}) || '%'")
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

/// Update team budget settings.
pub async fn update_team_budget(
    pool: &PgPool,
    id: Uuid,
    budget_amount_usd: Option<f64>,
    budget_period: &str,
    budget_policy: Option<serde_json::Value>,
    default_user_budget_usd: Option<f64>,
    notify_recipients: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"UPDATE teams SET
            budget_amount_usd = $1,
            budget_period = $2,
            budget_policy = $3,
            default_user_budget_usd = $4,
            notify_recipients = $5
        WHERE id = $6"#,
    )
    .bind(budget_amount_usd)
    .bind(budget_period)
    .bind(budget_policy)
    .bind(default_user_budget_usd)
    .bind(notify_recipients)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}
