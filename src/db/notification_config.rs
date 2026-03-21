use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NotificationConfig {
    pub id: Uuid,
    pub slot: String,
    pub destination_type: String,
    pub destination_value: String,
    pub event_categories: serde_json::Value,
    pub last_tested_at: Option<DateTime<Utc>>,
    pub last_test_success: Option<bool>,
    pub last_test_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct DeliveryLogEntry {
    pub id: i64,
    pub event_id: Option<i64>,
    pub destination_type: String,
    pub destination_value: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub error_message: Option<String>,
    pub duration_ms: i32,
    pub created_at: DateTime<Utc>,
}

/// Get the active notification config (slot = 'active').
pub async fn get_active(pool: &PgPool) -> anyhow::Result<Option<NotificationConfig>> {
    let row = sqlx::query_as::<_, NotificationConfig>(
        "SELECT * FROM notification_config WHERE slot = 'active'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Get the draft notification config (slot = 'draft').
pub async fn get_draft(pool: &PgPool) -> anyhow::Result<Option<NotificationConfig>> {
    let row = sqlx::query_as::<_, NotificationConfig>(
        "SELECT * FROM notification_config WHERE slot = 'draft'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Get both active and draft configs.
pub async fn get_both(
    pool: &PgPool,
) -> anyhow::Result<(Option<NotificationConfig>, Option<NotificationConfig>)> {
    let rows =
        sqlx::query_as::<_, NotificationConfig>("SELECT * FROM notification_config ORDER BY slot")
            .fetch_all(pool)
            .await?;

    let mut active = None;
    let mut draft = None;
    for row in rows {
        match row.slot.as_str() {
            "active" => active = Some(row),
            "draft" => draft = Some(row),
            _ => {}
        }
    }
    Ok((active, draft))
}

/// Save or update draft config. Uses upsert on the unique slot constraint.
pub async fn upsert_draft(
    pool: &PgPool,
    destination_type: &str,
    destination_value: &str,
    event_categories: &serde_json::Value,
) -> anyhow::Result<NotificationConfig> {
    let row = sqlx::query_as::<_, NotificationConfig>(
        r#"INSERT INTO notification_config (slot, destination_type, destination_value, event_categories)
           VALUES ('draft', $1, $2, $3)
           ON CONFLICT (slot) WHERE slot = 'draft' DO UPDATE SET
             destination_type = EXCLUDED.destination_type,
             destination_value = EXCLUDED.destination_value,
             event_categories = EXCLUDED.event_categories,
             last_tested_at = NULL,
             last_test_success = NULL,
             last_test_error = NULL,
             updated_at = now()
           RETURNING *"#,
    )
    .bind(destination_type)
    .bind(destination_value)
    .bind(event_categories)
    .fetch_one(pool)
    .await?;

    super::settings::bump_cache_version(pool).await?;
    Ok(row)
}

/// Promote draft → active. Transaction: delete current active, update draft slot to active.
pub async fn activate_draft(pool: &PgPool) -> anyhow::Result<Option<NotificationConfig>> {
    let mut tx = pool.begin().await?;

    // Delete current active
    sqlx::query("DELETE FROM notification_config WHERE slot = 'active'")
        .execute(&mut *tx)
        .await?;

    // Promote draft to active
    let row = sqlx::query_as::<_, NotificationConfig>(
        "UPDATE notification_config SET slot = 'active', updated_at = now() WHERE slot = 'draft' RETURNING *",
    )
    .fetch_optional(&mut *tx)
    .await?;

    tx.commit().await?;

    if row.is_some() {
        super::settings::bump_cache_version(pool).await?;
    }
    Ok(row)
}

/// Delete draft config.
pub async fn delete_draft(pool: &PgPool) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM notification_config WHERE slot = 'draft'")
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        super::settings::bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Delete active config (fall back to env var).
pub async fn delete_active(pool: &PgPool) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM notification_config WHERE slot = 'active'")
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        super::settings::bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Update test result on a config slot.
pub async fn update_test_result(
    pool: &PgPool,
    slot: &str,
    success: bool,
    error: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE notification_config
           SET last_tested_at = now(), last_test_success = $1, last_test_error = $2, updated_at = now()
           WHERE slot = $3"#,
    )
    .bind(success)
    .bind(error)
    .bind(slot)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update event categories on the active config.
pub async fn update_event_categories(
    pool: &PgPool,
    categories: &serde_json::Value,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE notification_config SET event_categories = $1, updated_at = now() WHERE slot = 'active'",
    )
    .bind(categories)
    .execute(pool)
    .await?;
    if result.rows_affected() > 0 {
        super::settings::bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Log a delivery attempt.
#[allow(clippy::too_many_arguments)]
pub async fn log_delivery(
    pool: &PgPool,
    event_id: Option<i64>,
    destination_type: &str,
    destination_value: &str,
    event_type: &str,
    payload: &serde_json::Value,
    status: &str,
    error: Option<&str>,
    duration_ms: i32,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO notification_delivery_log
           (event_id, destination_type, destination_value, event_type, payload, status, error_message, duration_ms)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
    )
    .bind(event_id)
    .bind(destination_type)
    .bind(destination_value)
    .bind(event_type)
    .bind(payload)
    .bind(status)
    .bind(error)
    .bind(duration_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get recent delivery log entries.
pub async fn get_recent_deliveries(
    pool: &PgPool,
    limit: i64,
) -> anyhow::Result<Vec<DeliveryLogEntry>> {
    let rows = sqlx::query_as::<_, DeliveryLogEntry>(
        "SELECT * FROM notification_delivery_log ORDER BY created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Prune delivery log to keep only the most recent entries.
pub async fn prune_delivery_log(pool: &PgPool, keep_count: i64) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"DELETE FROM notification_delivery_log
           WHERE id NOT IN (
             SELECT id FROM notification_delivery_log
             ORDER BY created_at DESC LIMIT $1
           )"#,
    )
    .bind(keep_count)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
