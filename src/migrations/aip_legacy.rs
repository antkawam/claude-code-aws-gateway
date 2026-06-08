use uuid::Uuid;

/// A candidate endpoint for the startup auto-migration.
///
/// Carries only the minimal data the migration runner needs: the endpoint's
/// primary key and the legacy `inference_profile_arn` column value.
#[derive(Debug, Clone)]
pub struct EndpointMigrationCandidate {
    pub endpoint_id: Uuid,
    pub legacy_arn: String,
}

/// Self-healing startup migration for legacy single-AIP endpoints.
///
/// For each `candidate` whose `endpoint_id` has **zero** rows in
/// `endpoint_aip_overrides`, calls `get_foundation_model(legacy_arn)` to
/// determine the Anthropic logical model ID and inserts one override row with:
/// - `model_id`  = the value returned by `get_foundation_model`
/// - `aip_arn`   = `legacy_arn`
/// - `set_by`    = `"auto-migration"`
/// - `reason`    = `"migrated from inference_profile_arn column"`
///
/// The legacy `inference_profile_arn` column is **not** cleared.
///
/// # Error handling
///
/// Per-endpoint errors (from `get_foundation_model` or from the DB insert) are
/// logged as warnings and the runner continues to the next endpoint.  The function
/// returns `Ok(())` even when individual endpoints fail, so that startup is never
/// blocked by a single bad endpoint.
pub async fn migrate_legacy_aip_endpoints<F, Fut>(
    candidates: &[EndpointMigrationCandidate],
    pool: &sqlx::PgPool,
    get_foundation_model: F,
) -> anyhow::Result<()>
where
    F: Fn(&str) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<String, String>> + Send,
{
    for candidate in candidates {
        let endpoint_id = candidate.endpoint_id;
        let legacy_arn = &candidate.legacy_arn;

        // Skip endpoints that already have at least one override row.
        match crate::db::endpoint_aip_overrides::list_by_endpoint(pool, endpoint_id).await {
            Ok(rows) if !rows.is_empty() => {
                tracing::debug!(
                    %endpoint_id,
                    existing_rows = rows.len(),
                    "Skipping endpoint: already has AIP override rows"
                );
                continue;
            }
            Ok(_) => { /* zero rows — proceed */ }
            Err(e) => {
                tracing::warn!(
                    %endpoint_id,
                    %e,
                    "Failed to check existing AIP override rows for endpoint — skipping"
                );
                continue;
            }
        }

        // Resolve the foundation model via the injected closure.
        let model_id = match get_foundation_model(legacy_arn).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    %endpoint_id,
                    legacy_arn = %legacy_arn,
                    error = %e,
                    "get_foundation_model failed for endpoint — skipping migration for this endpoint"
                );
                continue;
            }
        };

        // Insert the override row.
        if let Err(e) = crate::db::endpoint_aip_overrides::insert(
            pool,
            endpoint_id,
            &model_id,
            legacy_arn,
            "auto-migration",
            Some("migrated from inference_profile_arn column"),
        )
        .await
        {
            tracing::warn!(
                %endpoint_id,
                model_id = %model_id,
                %e,
                "Failed to insert AIP override row for endpoint — skipping"
            );
            continue;
        }

        tracing::info!(
            %endpoint_id,
            model_id = %model_id,
            aip_arn = %legacy_arn,
            "Auto-migrated legacy inference_profile_arn to endpoint_aip_overrides"
        );
    }

    Ok(())
}
