use sqlx::PgPool;
use uuid::Uuid;

use super::schema::Endpoint;
use super::settings::bump_cache_version;

#[allow(clippy::too_many_arguments)]
pub async fn create_endpoint(
    pool: &PgPool,
    name: &str,
    role_arn: Option<&str>,
    external_id: Option<&str>,
    inference_profile_arn: Option<&str>,
    region: &str,
    routing_prefix: &str,
    priority: i32,
) -> anyhow::Result<Endpoint> {
    let endpoint = sqlx::query_as::<_, Endpoint>(
        r#"INSERT INTO endpoints (name, role_arn, external_id, inference_profile_arn, region, routing_prefix, priority)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING *"#,
    )
    .bind(name)
    .bind(role_arn)
    .bind(external_id)
    .bind(inference_profile_arn)
    .bind(region)
    .bind(routing_prefix)
    .bind(priority)
    .fetch_one(pool)
    .await?;

    bump_cache_version(pool).await?;
    Ok(endpoint)
}

pub async fn list_endpoints(pool: &PgPool) -> anyhow::Result<Vec<Endpoint>> {
    let endpoints =
        sqlx::query_as::<_, Endpoint>("SELECT * FROM endpoints ORDER BY priority, name")
            .fetch_all(pool)
            .await?;
    Ok(endpoints)
}

pub async fn get_endpoint(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<Endpoint>> {
    let endpoint = sqlx::query_as::<_, Endpoint>("SELECT * FROM endpoints WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(endpoint)
}

#[allow(clippy::too_many_arguments)]
pub async fn update_endpoint(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    role_arn: Option<&str>,
    external_id: Option<&str>,
    inference_profile_arn: Option<&str>,
    region: &str,
    routing_prefix: &str,
    priority: i32,
    enabled: bool,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"UPDATE endpoints SET name = $1, role_arn = $2, external_id = $3,
           inference_profile_arn = $4, region = $5,
           routing_prefix = $6, priority = $7, enabled = $8 WHERE id = $9"#,
    )
    .bind(name)
    .bind(role_arn)
    .bind(external_id)
    .bind(inference_profile_arn)
    .bind(region)
    .bind(routing_prefix)
    .bind(priority)
    .bind(enabled)
    .bind(id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

pub async fn delete_endpoint(pool: &PgPool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM endpoints WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

pub async fn get_team_endpoints(pool: &PgPool, team_id: Uuid) -> anyhow::Result<Vec<Endpoint>> {
    let endpoints = sqlx::query_as::<_, Endpoint>(
        r#"SELECT e.* FROM endpoints e
           JOIN team_endpoints te ON te.endpoint_id = e.id
           WHERE te.team_id = $1 AND e.enabled = true
           ORDER BY te.priority, e.priority"#,
    )
    .bind(team_id)
    .fetch_all(pool)
    .await?;
    Ok(endpoints)
}

pub async fn set_team_endpoints(
    pool: &PgPool,
    team_id: Uuid,
    assignments: &[(Uuid, i32)], // (endpoint_id, priority)
) -> anyhow::Result<()> {
    // Delete existing assignments
    sqlx::query("DELETE FROM team_endpoints WHERE team_id = $1")
        .bind(team_id)
        .execute(pool)
        .await?;

    // Insert new assignments
    for (endpoint_id, priority) in assignments {
        sqlx::query(
            "INSERT INTO team_endpoints (team_id, endpoint_id, priority) VALUES ($1, $2, $3)",
        )
        .bind(team_id)
        .bind(endpoint_id)
        .bind(priority)
        .execute(pool)
        .await?;
    }

    bump_cache_version(pool).await?;
    Ok(())
}

/// Get all enabled endpoints (for teams without specific assignments).
pub async fn get_enabled_endpoints(pool: &PgPool) -> anyhow::Result<Vec<Endpoint>> {
    let endpoints = sqlx::query_as::<_, Endpoint>(
        "SELECT * FROM endpoints WHERE enabled = true ORDER BY priority, name",
    )
    .fetch_all(pool)
    .await?;
    Ok(endpoints)
}

/// Set one endpoint as the default, clearing any previous default.
pub async fn set_default_endpoint(pool: &PgPool, id: Uuid) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;

    sqlx::query("UPDATE endpoints SET is_default = false WHERE is_default = true")
        .execute(&mut *tx)
        .await?;

    let result = sqlx::query("UPDATE endpoints SET is_default = true WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Get the designated default endpoint, if any.
pub async fn get_default_endpoint(pool: &PgPool) -> anyhow::Result<Option<Endpoint>> {
    let endpoint = sqlx::query_as::<_, Endpoint>(
        "SELECT * FROM endpoints WHERE is_default = true AND enabled = true",
    )
    .fetch_optional(pool)
    .await?;
    Ok(endpoint)
}
