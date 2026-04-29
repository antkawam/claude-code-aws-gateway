use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ModelPricing {
    pub model_prefix: String,
    pub input_rate: f64,
    pub output_rate: f64,
    pub cache_read_rate: f64,
    pub cache_write_rate: f64,
    pub source: String,
    pub aws_sku: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// List all pricing rows, ordered by model_prefix ASC.
pub async fn list_all(pool: &PgPool) -> anyhow::Result<Vec<ModelPricing>> {
    let rows = sqlx::query_as::<_, ModelPricing>(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         ORDER BY model_prefix ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fetch one row by prefix (exact match, not LIKE).
pub async fn get(pool: &PgPool, prefix: &str) -> anyhow::Result<Option<ModelPricing>> {
    let row = sqlx::query_as::<_, ModelPricing>(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind(prefix)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Upsert from AWS Price List refresh. Refuses to overwrite rows where source='admin_manual'.
/// Returns true if inserted/updated, false if skipped because of admin_manual.
/// Pass the intended source in `row.source` (typically 'price_list_api').
pub async fn upsert_from_api(pool: &PgPool, row: &ModelPricing) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source, aws_sku, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, now())
        ON CONFLICT (model_prefix) DO UPDATE SET
            input_rate       = EXCLUDED.input_rate,
            output_rate      = EXCLUDED.output_rate,
            cache_read_rate  = EXCLUDED.cache_read_rate,
            cache_write_rate = EXCLUDED.cache_write_rate,
            source           = EXCLUDED.source,
            aws_sku          = EXCLUDED.aws_sku,
            updated_at       = now()
            WHERE model_pricing.source <> 'admin_manual'"#,
    )
    .bind(&row.model_prefix)
    .bind(row.input_rate)
    .bind(row.output_rate)
    .bind(row.cache_read_rate)
    .bind(row.cache_write_rate)
    .bind(&row.source)
    .bind(&row.aws_sku)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Upsert from admin override. Always writes, always forces source='admin_manual'
/// regardless of what `row.source` contains.
pub async fn upsert_manual(pool: &PgPool, row: &ModelPricing) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source, aws_sku, updated_at)
        VALUES ($1, $2, $3, $4, $5, 'admin_manual', $6, now())
        ON CONFLICT (model_prefix) DO UPDATE SET
            input_rate       = EXCLUDED.input_rate,
            output_rate      = EXCLUDED.output_rate,
            cache_read_rate  = EXCLUDED.cache_read_rate,
            cache_write_rate = EXCLUDED.cache_write_rate,
            source           = 'admin_manual',
            aws_sku          = EXCLUDED.aws_sku,
            updated_at       = now()"#,
    )
    .bind(&row.model_prefix)
    .bind(row.input_rate)
    .bind(row.output_rate)
    .bind(row.cache_read_rate)
    .bind(row.cache_write_rate)
    .bind(&row.aws_sku)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a row. Returns true if existed & deleted, false if it didn't exist.
pub async fn delete(pool: &PgPool, prefix: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(prefix)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}
