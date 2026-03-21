use sqlx::PgPool;
use uuid::Uuid;

use super::schema::User;

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
