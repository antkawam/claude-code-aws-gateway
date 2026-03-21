use sqlx::PgPool;
use uuid::Uuid;

use super::schema::Team;

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
