use sqlx::PgPool;

fn token_key(token_id: &str) -> String {
    format!("setup_token:{}", token_id)
}

/// Create a setup token in the DB (proxy_settings KV store).
/// The token maps to a raw virtual key and expires after 5 minutes (enforced on consume).
pub async fn create(pool: &PgPool, token_id: &str, raw_key: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO proxy_settings (key, value, updated_at)
           VALUES ($1, $2, now())
           ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()"#,
    )
    .bind(token_key(token_id))
    .bind(raw_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Consume a setup token (single-use). Returns the raw virtual key if found and not expired.
/// The token is deleted atomically on consumption.
pub async fn consume(pool: &PgPool, token_id: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        r#"DELETE FROM proxy_settings
           WHERE key = $1 AND updated_at > now() - interval '300 seconds'
           RETURNING value"#,
    )
    .bind(token_key(token_id))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(v,)| v))
}

/// Delete expired setup tokens from the DB. Returns the number of tokens cleaned up.
pub async fn cleanup_expired(pool: &PgPool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM proxy_settings WHERE key LIKE 'setup_token:%' AND updated_at < now() - interval '300 seconds'",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
