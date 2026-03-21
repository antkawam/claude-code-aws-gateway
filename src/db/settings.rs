use sqlx::PgPool;

pub async fn get_setting(pool: &PgPool, key: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM proxy_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

pub async fn set_setting(pool: &PgPool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO proxy_settings (key, value, updated_at)
           VALUES ($1, $2, now())
           ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()"#,
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;

    bump_cache_version(pool).await?;
    Ok(())
}

pub async fn bump_cache_version(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query("UPDATE cache_version SET version = version + 1, updated_at = now() WHERE id = 1")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_cache_version(pool: &PgPool) -> anyhow::Result<i64> {
    let row: (i64,) = sqlx::query_as("SELECT version FROM cache_version WHERE id = 1")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}
