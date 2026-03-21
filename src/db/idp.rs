use sqlx::PgPool;
use uuid::Uuid;

use super::schema::IdentityProvider;
use super::settings::bump_cache_version;

#[allow(clippy::too_many_arguments)]
pub async fn create_idp(
    pool: &PgPool,
    name: &str,
    issuer_url: &str,
    client_id: Option<&str>,
    audience: Option<&str>,
    jwks_url: Option<&str>,
    flow_type: &str,
    auto_provision: bool,
    default_role: &str,
    allowed_domains: Option<&[String]>,
) -> anyhow::Result<IdentityProvider> {
    let row = sqlx::query_as::<_, IdentityProvider>(
        r#"INSERT INTO identity_providers
            (name, issuer_url, client_id, audience, jwks_url, flow_type, auto_provision, default_role, allowed_domains, source)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'admin')
            RETURNING *"#,
    )
    .bind(name)
    .bind(issuer_url)
    .bind(client_id)
    .bind(audience)
    .bind(jwks_url)
    .bind(flow_type)
    .bind(auto_provision)
    .bind(default_role)
    .bind(allowed_domains)
    .fetch_one(pool)
    .await?;

    bump_cache_version(pool).await?;
    Ok(row)
}

pub async fn list_idps(pool: &PgPool) -> anyhow::Result<Vec<IdentityProvider>> {
    let rows = sqlx::query_as::<_, IdentityProvider>(
        "SELECT * FROM identity_providers ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_enabled_idps(pool: &PgPool) -> anyhow::Result<Vec<IdentityProvider>> {
    let rows = sqlx::query_as::<_, IdentityProvider>(
        "SELECT * FROM identity_providers WHERE enabled = true ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub async fn update_idp(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    issuer_url: &str,
    client_id: Option<&str>,
    audience: Option<&str>,
    jwks_url: Option<&str>,
    flow_type: &str,
    auto_provision: bool,
    default_role: &str,
    allowed_domains: Option<&[String]>,
    enabled: bool,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"UPDATE identity_providers SET
            name = $2, issuer_url = $3, client_id = $4, audience = $5,
            jwks_url = $6, flow_type = $7, auto_provision = $8,
            default_role = $9, allowed_domains = $10, enabled = $11
            WHERE id = $1 AND source = 'admin'"#,
    )
    .bind(id)
    .bind(name)
    .bind(issuer_url)
    .bind(client_id)
    .bind(audience)
    .bind(jwks_url)
    .bind(flow_type)
    .bind(auto_provision)
    .bind(default_role)
    .bind(allowed_domains)
    .bind(enabled)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}

pub async fn delete_idp(pool: &PgPool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM identity_providers WHERE id = $1 AND source = 'admin'")
        .bind(id)
        .execute(pool)
        .await?;

    if result.rows_affected() > 0 {
        bump_cache_version(pool).await?;
    }
    Ok(result.rows_affected() > 0)
}
