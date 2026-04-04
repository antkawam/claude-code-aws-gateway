use sqlx::PgPool;

/// Record a single login attempt (timestamp defaults to now()).
pub async fn record_attempt(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO login_attempts DEFAULT VALUES")
        .execute(pool)
        .await?;
    Ok(())
}

/// Count login attempts within the last `window_secs` seconds.
pub async fn count_recent(pool: &PgPool, window_secs: f64) -> anyhow::Result<i64> {
    let (count,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM login_attempts WHERE attempted_at > now() - make_interval(secs => $1)",
    )
    .bind(window_secs)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Delete login attempts older than `window_secs` seconds. Returns rows deleted.
pub async fn cleanup(pool: &PgPool, window_secs: f64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM login_attempts WHERE attempted_at < now() - make_interval(secs => $1)",
    )
    .bind(window_secs)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Check whether a login attempt is allowed, and if so, record it.
///
/// Returns `Ok(true)` if the attempt is allowed (under the limit),
/// or `Ok(false)` if the rate limit has been exceeded.
pub async fn check_and_record(
    pool: &PgPool,
    max_attempts: i64,
    window_secs: f64,
) -> anyhow::Result<bool> {
    cleanup(pool, window_secs).await?;
    let count = count_recent(pool, window_secs).await?;
    if count >= max_attempts {
        return Ok(false);
    }
    record_attempt(pool).await?;
    Ok(true)
}
