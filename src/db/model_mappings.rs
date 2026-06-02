use sqlx::PgPool;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelMappingRow {
    pub anthropic_prefix: String,
    pub bedrock_suffix: String,
    pub anthropic_display: Option<String>,
}

/// Load all model mappings (order is irrelevant; uses HashMap exact-match lookup).
pub async fn get_all_mappings(pool: &PgPool) -> Result<Vec<ModelMappingRow>, sqlx::Error> {
    sqlx::query_as!(
        ModelMappingRow,
        r#"SELECT anthropic_prefix, bedrock_suffix, anthropic_display
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
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
           VALUES ($1, $2, $3, 'discovered')
           ON CONFLICT (anthropic_prefix) DO UPDATE
           SET bedrock_suffix = EXCLUDED.bedrock_suffix,
               anthropic_display = EXCLUDED.anthropic_display,
               source = 'discovered'"#,
        anthropic_prefix,
        bedrock_suffix,
        anthropic_display,
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
            r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
               VALUES ($1, $2, $3, 'seed')
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
