use sqlx::{FromRow, PgPool, Row};
use uuid::Uuid;

/// A single admin-managed capability override row.
#[derive(Debug, Clone)]
pub struct BetaOverride {
    pub endpoint_id: Uuid,
    pub profile_id: String,
    pub beta_name: String,
    pub supported: bool,
    pub set_at: chrono::DateTime<chrono::Utc>,
    pub set_by: String,
    pub reason: Option<String>,
}

impl<'r> FromRow<'r, sqlx::postgres::PgRow> for BetaOverride {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(BetaOverride {
            endpoint_id: row.try_get("endpoint_id")?,
            profile_id: row.try_get("profile_id")?,
            beta_name: row.try_get("beta_name")?,
            supported: row.try_get("supported")?,
            set_at: row.try_get("set_at")?,
            set_by: row.try_get("set_by")?,
            reason: row.try_get("reason")?,
        })
    }
}

/// List all beta overrides across all endpoints.
pub async fn list_all(pool: &PgPool) -> Result<Vec<BetaOverride>, sqlx::Error> {
    sqlx::query_as::<_, BetaOverride>(
        "SELECT endpoint_id, profile_id, beta_name, supported, set_at, set_by, reason \
         FROM beta_overrides \
         ORDER BY endpoint_id, profile_id, beta_name",
    )
    .fetch_all(pool)
    .await
}

/// List all beta overrides for a specific endpoint.
pub async fn list_for_endpoint(pool: &PgPool, endpoint_id: Uuid) -> Result<Vec<BetaOverride>, sqlx::Error> {
    sqlx::query_as::<_, BetaOverride>(
        "SELECT endpoint_id, profile_id, beta_name, supported, set_at, set_by, reason \
         FROM beta_overrides \
         WHERE endpoint_id = $1 \
         ORDER BY profile_id, beta_name",
    )
    .bind(endpoint_id)
    .fetch_all(pool)
    .await
}

/// Insert or update a beta override row (upsert on primary key).
pub async fn upsert(pool: &PgPool, ovr: &BetaOverride) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO beta_overrides (endpoint_id, profile_id, beta_name, supported, set_at, set_by, reason) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (endpoint_id, profile_id, beta_name) \
         DO UPDATE SET supported = EXCLUDED.supported, \
                       set_at = EXCLUDED.set_at, \
                       set_by = EXCLUDED.set_by, \
                       reason = EXCLUDED.reason",
    )
    .bind(ovr.endpoint_id)
    .bind(&ovr.profile_id)
    .bind(&ovr.beta_name)
    .bind(ovr.supported)
    .bind(ovr.set_at)
    .bind(&ovr.set_by)
    .bind(&ovr.reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a beta override row. Returns the number of rows deleted (0 or 1).
pub async fn delete(
    pool: &PgPool,
    endpoint_id: Uuid,
    profile_id: &str,
    beta_name: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM beta_overrides WHERE endpoint_id = $1 AND profile_id = $2 AND beta_name = $3",
    )
    .bind(endpoint_id)
    .bind(profile_id)
    .bind(beta_name)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
