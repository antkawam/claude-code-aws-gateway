use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::schema::VirtualKey;
use super::settings::bump_cache_version;

pub fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn generate_key() -> String {
    use rand::Rng;
    let random_bytes: [u8; 32] = rand::rng().random();
    format!("sk-proxy-{}", hex::encode(random_bytes))
}

pub fn key_prefix(key: &str) -> String {
    if key.len() > 16 {
        format!("{}...", &key[..16])
    } else {
        key.to_string()
    }
}

pub async fn create_key(
    pool: &PgPool,
    name: Option<&str>,
    user_id: Option<Uuid>,
    team_id: Option<Uuid>,
    rate_limit_rpm: Option<i32>,
) -> anyhow::Result<(String, VirtualKey)> {
    let raw_key = generate_key();
    let hash = hash_key(&raw_key);
    let prefix = key_prefix(&raw_key);

    let key = sqlx::query_as::<_, VirtualKey>(
        r#"INSERT INTO virtual_keys (key_hash, key_prefix, name, user_id, team_id, rate_limit_rpm)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING *"#,
    )
    .bind(&hash)
    .bind(&prefix)
    .bind(name)
    .bind(user_id)
    .bind(team_id)
    .bind(rate_limit_rpm)
    .fetch_one(pool)
    .await?;

    bump_cache_version(pool).await?;
    Ok((raw_key, key))
}

pub async fn list_keys(pool: &PgPool) -> anyhow::Result<Vec<VirtualKey>> {
    let keys =
        sqlx::query_as::<_, VirtualKey>("SELECT * FROM virtual_keys ORDER BY created_at DESC")
            .fetch_all(pool)
            .await?;
    Ok(keys)
}

pub async fn get_active_keys(pool: &PgPool) -> anyhow::Result<Vec<VirtualKey>> {
    let keys = sqlx::query_as::<_, VirtualKey>(
        "SELECT v.* FROM virtual_keys v \
         LEFT JOIN users u ON v.user_id = u.id \
         WHERE v.is_active = true \
         AND (v.expires_at IS NULL OR v.expires_at > now()) \
         AND (v.user_id IS NULL OR u.active = true)",
    )
    .fetch_all(pool)
    .await?;
    Ok(keys)
}

pub async fn revoke_key(pool: &PgPool, key_id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE virtual_keys SET is_active = false WHERE id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

pub async fn delete_key(pool: &PgPool, key_id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM virtual_keys WHERE id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}
