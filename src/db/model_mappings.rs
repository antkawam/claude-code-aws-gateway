use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelMappingRow {
    pub anthropic_prefix: String,
    pub bedrock_suffix: String,
    pub anthropic_display: Option<String>,
    #[serde(default = "default_created_via")]
    pub created_via: String,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
}

fn default_created_via() -> String {
    "unknown".to_string()
}

/// Load all model mappings (order is irrelevant; uses HashMap exact-match lookup).
pub async fn get_all_mappings(pool: &PgPool) -> Result<Vec<ModelMappingRow>, sqlx::Error> {
    sqlx::query_as!(
        ModelMappingRow,
        r#"SELECT anthropic_prefix, bedrock_suffix, anthropic_display, created_via, last_used_at
           FROM model_mappings"#
    )
    .fetch_all(pool)
    .await
}

/// Insert or update a model mapping. Bumps cache_version for cross-instance sync.
pub async fn upsert_mapping(
    pool: &PgPool,
    anthropic_prefix: &str,
    bedrock_suffix: &str,
    anthropic_display: Option<&str>,
    created_via: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source, created_via)
           VALUES ($1, $2, $3, 'discovered', $4)
           ON CONFLICT (anthropic_prefix) DO UPDATE
           SET bedrock_suffix = EXCLUDED.bedrock_suffix,
               anthropic_display = EXCLUDED.anthropic_display,
               source = 'discovered',
               created_via = EXCLUDED.created_via"#,
        anthropic_prefix,
        bedrock_suffix,
        anthropic_display,
        created_via,
    )
    .execute(pool)
    .await?;

    super::settings::bump_cache_version(pool)
        .await
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    Ok(())
}

/// Insert missing seed rows with `ON CONFLICT DO NOTHING`. Never overwrites
/// existing rows (any source). Returns the count of rows actually inserted.
/// Bumps `cache_version` if any rows were inserted (so other gateway instances reload).
pub async fn seed_missing(pool: &PgPool, rows: Vec<ModelMappingRow>) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut total_inserted: u64 = 0;

    for row in rows {
        let result = sqlx::query!(
            r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source, created_via)
               VALUES ($1, $2, $3, 'seed', 'unknown')
               ON CONFLICT (anthropic_prefix) DO NOTHING"#,
            row.anthropic_prefix,
            row.bedrock_suffix,
            row.anthropic_display,
        )
        .execute(&mut *tx)
        .await?;

        total_inserted += result.rows_affected();
    }

    tx.commit().await?;

    let inserted_count = total_inserted as usize;
    if inserted_count > 0 {
        super::settings::bump_cache_version(pool)
            .await
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    }

    Ok(inserted_count)
}

/// Update `last_used_at` to the current time for the given `anthropic_prefix`.
/// Returns `Ok(())` whether zero or one rows were updated (no-op on missing prefix).
pub async fn touch_last_used(pool: &PgPool, anthropic_prefix: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"UPDATE model_mappings SET last_used_at = now() WHERE anthropic_prefix = $1"#,
        anthropic_prefix,
    )
    .execute(pool)
    .await?;
    Ok(())
}
