use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// A single per-endpoint AIP override row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AipOverride {
    pub endpoint_id: Uuid,
    pub model_id: String,
    pub aip_arn: String,
    pub set_at: DateTime<Utc>,
    pub set_by: String,
    pub reason: Option<String>,
}

/// Insert a new AIP override row.
///
/// Returns an error on PK violation (duplicate `(endpoint_id, model_id)`).
/// The raw `sqlx::Error::Database` with Postgres code `23505` is propagated so
/// admin handlers can downcast it and return HTTP 409.
pub async fn insert(
    pool: &PgPool,
    endpoint_id: Uuid,
    model_id: &str,
    aip_arn: &str,
    set_by: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO endpoint_aip_overrides (endpoint_id, model_id, aip_arn, set_by, reason)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(endpoint_id)
    .bind(model_id)
    .bind(aip_arn)
    .bind(set_by)
    .bind(reason)
    .execute(pool)
    .await?;

    Ok(())
}

/// List all AIP override rows for the given endpoint.
///
/// Returns an empty vec when no rows exist (not an error).
pub async fn list_by_endpoint(
    pool: &PgPool,
    endpoint_id: Uuid,
) -> anyhow::Result<Vec<AipOverride>> {
    let rows = sqlx::query_as::<_, AipOverride>(
        r#"SELECT endpoint_id, model_id, aip_arn, set_at, set_by, reason
           FROM endpoint_aip_overrides
           WHERE endpoint_id = $1
           ORDER BY model_id"#,
    )
    .bind(endpoint_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Delete the override row for a specific `(endpoint_id, model_id)`.
///
/// Idempotent: if no row exists the operation succeeds silently (0 rows
/// affected is not treated as an error at the DB layer).  Admin handlers
/// inspect the rows-affected count and may return 404 if appropriate.
pub async fn delete_by_model(
    pool: &PgPool,
    endpoint_id: Uuid,
    model_id: &str,
) -> anyhow::Result<u64> {
    let result =
        sqlx::query("DELETE FROM endpoint_aip_overrides WHERE endpoint_id = $1 AND model_id = $2")
            .bind(endpoint_id)
            .bind(model_id)
            .execute(pool)
            .await?;

    Ok(result.rows_affected())
}
