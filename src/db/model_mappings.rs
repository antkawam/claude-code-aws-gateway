use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct ModelMappingRow {
    pub anthropic_prefix: String,
    pub bedrock_suffix: String,
    pub anthropic_display: Option<String>,
}

/// Load all model mappings ordered by prefix length descending (longest first).
pub async fn get_all_mappings(pool: &PgPool) -> Result<Vec<ModelMappingRow>, sqlx::Error> {
    sqlx::query_as!(
        ModelMappingRow,
        r#"SELECT anthropic_prefix, bedrock_suffix, anthropic_display
           FROM model_mappings
           ORDER BY length(anthropic_prefix) DESC"#
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
