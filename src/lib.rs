pub mod api;
pub mod auth;
pub mod budget;
pub mod config;
pub mod db;
pub mod detection;
pub mod endpoint;
pub mod pricing;
pub mod proxy;
pub mod quota;
pub mod ratelimit;
pub mod scim;
pub mod spend;
pub mod telemetry;
pub mod translate;
pub mod websearch;

/// Replay all admin beta overrides from `beta_overrides` table into the in-memory
/// endpoint pool.
///
/// Called at boot (before the gateway starts serving traffic) and on every
/// `cache_version` bump (so overrides written on one replica propagate to all).
///
/// Returns the number of overrides successfully applied (i.e. where
/// `endpoint_pool.get_client(ovr.endpoint_id)` returned `Some`).
/// Rows whose endpoint is not in the pool are silently skipped with a debug log.
pub async fn apply_overrides_to_pool(
    pool: &sqlx::PgPool,
    endpoint_pool: &crate::endpoint::EndpointPool,
) -> Result<usize, sqlx::Error> {
    let overrides = crate::db::beta_overrides::list_all(pool).await?;
    let mut applied = 0usize;
    for ovr in &overrides {
        if let Some(client) = endpoint_pool.get_client(ovr.endpoint_id).await {
            if ovr.supported {
                client
                    .mark_supported(
                        &ovr.profile_id,
                        &ovr.beta_name,
                        crate::endpoint::ProbeSource::AdminOverride,
                    )
                    .await;
            } else {
                client
                    .mark_unsupported(
                        &ovr.profile_id,
                        &ovr.beta_name,
                        crate::endpoint::ProbeSource::AdminOverride,
                    )
                    .await;
            }
            applied += 1;
        } else {
            tracing::debug!(
                endpoint_id = %ovr.endpoint_id,
                profile = %ovr.profile_id,
                beta = %ovr.beta_name,
                "Skipping override replay: endpoint not in pool"
            );
        }
    }
    tracing::info!(total = overrides.len(), applied, "Replayed beta overrides");
    Ok(applied)
}
