use sqlx::PgPool;
use uuid::Uuid;

/// Upsert user endpoint affinity. Creates or updates the mapping.
pub async fn upsert(pool: &PgPool, user_identity: &str, endpoint_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO user_endpoint_affinity (user_identity, endpoint_id, last_used_at)
           VALUES ($1, $2, now())
           ON CONFLICT (user_identity)
           DO UPDATE SET endpoint_id = $2, last_used_at = now()"#,
    )
    .bind(user_identity)
    .bind(endpoint_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get the affinity endpoint for a user, if it exists and is not expired (30 min TTL).
pub async fn get(pool: &PgPool, user_identity: &str) -> anyhow::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT endpoint_id FROM user_endpoint_affinity \
         WHERE user_identity = $1 AND last_used_at > now() - interval '1800 seconds'",
    )
    .bind(user_identity)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Delete stale affinity entries (older than 30 minutes). Returns count of deleted rows.
pub async fn cleanup_stale(pool: &PgPool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM user_endpoint_affinity WHERE last_used_at < now() - interval '1800 seconds'",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
