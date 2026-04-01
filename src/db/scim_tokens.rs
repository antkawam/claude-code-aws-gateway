use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::schema::ScimToken;
use super::settings::bump_cache_version;

/// Hash a SCIM token using SHA-256 (same approach as virtual keys).
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Generate a new SCIM token: `scim-ccag-` prefix + 32 random bytes hex-encoded.
pub fn generate_token() -> String {
    use rand::Rng;
    let random_bytes: [u8; 32] = rand::rng().random();
    format!("scim-ccag-{}", hex::encode(random_bytes))
}

/// Token prefix for display (first 16 chars + "...").
pub fn token_prefix(token: &str) -> String {
    if token.len() > 16 {
        format!("{}...", &token[..16])
    } else {
        token.to_string()
    }
}

/// Create a new SCIM token for an IDP. Returns `(plaintext_token, ScimToken)`.
pub async fn create_scim_token(
    pool: &PgPool,
    idp_id: Uuid,
    name: Option<&str>,
    created_by: &str,
) -> anyhow::Result<(String, ScimToken)> {
    let raw_token = generate_token();
    let hash = hash_token(&raw_token);
    let prefix = token_prefix(&raw_token);

    let record = sqlx::query_as::<_, ScimToken>(
        r#"INSERT INTO scim_tokens (idp_id, token_hash, token_prefix, name, created_by)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING *"#,
    )
    .bind(idp_id)
    .bind(&hash)
    .bind(&prefix)
    .bind(name)
    .bind(created_by)
    .fetch_one(pool)
    .await?;

    bump_cache_version(pool).await?;
    Ok((raw_token, record))
}

/// Validate a SCIM token by its hash. Returns the token record if valid and enabled.
pub async fn validate_scim_token(
    pool: &PgPool,
    token_hash: &str,
) -> anyhow::Result<Option<ScimToken>> {
    let record = sqlx::query_as::<_, ScimToken>(
        "SELECT * FROM scim_tokens WHERE token_hash = $1 AND enabled = true",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    Ok(record)
}

/// List SCIM tokens, optionally filtered by IDP.
pub async fn list_scim_tokens(
    pool: &PgPool,
    idp_id: Option<Uuid>,
) -> anyhow::Result<Vec<ScimToken>> {
    let records = match idp_id {
        Some(id) => {
            sqlx::query_as::<_, ScimToken>(
                "SELECT * FROM scim_tokens WHERE idp_id = $1 ORDER BY created_at DESC",
            )
            .bind(id)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, ScimToken>("SELECT * FROM scim_tokens ORDER BY created_at DESC")
                .fetch_all(pool)
                .await?
        }
    };
    Ok(records)
}

/// Revoke (disable) a SCIM token. Returns `true` if a row was updated.
pub async fn revoke_scim_token(pool: &PgPool, token_id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE scim_tokens SET enabled = false WHERE id = $1")
        .bind(token_id)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Delete a SCIM token. Returns `true` if a row was deleted.
pub async fn delete_scim_token(pool: &PgPool, token_id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM scim_tokens WHERE id = $1")
        .bind(token_id)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Update the `last_used_at` timestamp for a token.
pub async fn update_last_used(pool: &PgPool, token_id: Uuid) -> anyhow::Result<()> {
    sqlx::query("UPDATE scim_tokens SET last_used_at = now() WHERE id = $1")
        .bind(token_id)
        .execute(pool)
        .await?;
    Ok(())
}
