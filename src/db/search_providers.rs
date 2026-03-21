use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserSearchProvider {
    pub id: Uuid,
    pub user_id: Uuid,
    pub provider_type: String,
    pub api_key: Option<String>,
    pub api_url: Option<String>,
    pub max_results: i32,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Get all search provider configs for a user.
pub async fn get_all_by_user_id(
    pool: &PgPool,
    user_id: Uuid,
) -> anyhow::Result<Vec<UserSearchProvider>> {
    let rows = sqlx::query_as::<_, UserSearchProvider>(
        "SELECT * FROM user_search_providers WHERE user_id = $1 ORDER BY provider_type",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Get the active (enabled) search provider for a user.
pub async fn get_active_by_user_id(
    pool: &PgPool,
    user_id: Uuid,
) -> anyhow::Result<Option<UserSearchProvider>> {
    let row = sqlx::query_as::<_, UserSearchProvider>(
        "SELECT * FROM user_search_providers WHERE user_id = $1 AND enabled = true LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Upsert a provider config keyed by (user_id, provider_type).
pub async fn upsert(
    pool: &PgPool,
    user_id: Uuid,
    provider_type: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    max_results: i32,
    enabled: bool,
) -> anyhow::Result<UserSearchProvider> {
    let row = sqlx::query_as::<_, UserSearchProvider>(
        r#"INSERT INTO user_search_providers (user_id, provider_type, api_key, api_url, max_results, enabled)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (user_id, provider_type) DO UPDATE SET
             api_key = EXCLUDED.api_key,
             api_url = EXCLUDED.api_url,
             max_results = EXCLUDED.max_results,
             enabled = EXCLUDED.enabled,
             updated_at = now()
           RETURNING *"#,
    )
    .bind(user_id)
    .bind(provider_type)
    .bind(api_key)
    .bind(api_url)
    .bind(max_results)
    .bind(enabled)
    .fetch_one(pool)
    .await?;

    super::settings::bump_cache_version(pool).await?;
    Ok(row)
}

/// Activate one provider and disable all others for this user (in a transaction).
pub async fn activate(
    pool: &PgPool,
    user_id: Uuid,
    provider_type: &str,
) -> anyhow::Result<Option<UserSearchProvider>> {
    let mut tx = pool.begin().await?;

    // Disable all providers for this user
    sqlx::query(
        "UPDATE user_search_providers SET enabled = false, updated_at = now() WHERE user_id = $1",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    // Enable the requested provider (must exist)
    let row = sqlx::query_as::<_, UserSearchProvider>(
        "UPDATE user_search_providers SET enabled = true, updated_at = now() WHERE user_id = $1 AND provider_type = $2 RETURNING *",
    )
    .bind(user_id)
    .bind(provider_type)
    .fetch_optional(&mut *tx)
    .await?;

    tx.commit().await?;

    if row.is_some() {
        super::settings::bump_cache_version(pool).await?;
    }
    Ok(row)
}

/// Delete a specific provider config by type.
pub async fn delete_by_user_and_type(
    pool: &PgPool,
    user_id: Uuid,
    provider_type: &str,
) -> anyhow::Result<bool> {
    let result =
        sqlx::query("DELETE FROM user_search_providers WHERE user_id = $1 AND provider_type = $2")
            .bind(user_id)
            .bind(provider_type)
            .execute(pool)
            .await?;
    if result.rows_affected() > 0 {
        super::settings::bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Delete all provider configs for a user.
pub async fn delete_all_by_user_id(pool: &PgPool, user_id: Uuid) -> anyhow::Result<u64> {
    let result = sqlx::query("DELETE FROM user_search_providers WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        super::settings::bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected())
}
