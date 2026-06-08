/// Integration tests for the Task 2 self-healing auto-migration runner.
///
/// # What is tested
///
/// These tests exercise `migrate_legacy_aip_endpoints` (the startup-time
/// function the Builder Agent will add, likely in `src/migrations/aip_legacy.rs`)
/// against a real Postgres instance.  All five contract points are covered:
///
/// 1. **Skip case A** — endpoint with no `inference_profile_arn` is untouched.
/// 2. **Skip case B** — endpoint with a legacy ARN AND ≥1 pre-existing rows in
///    `endpoint_aip_overrides` is skipped; the pre-inserted row survives unchanged.
/// 3. **Happy path** — endpoint with legacy ARN and zero override rows → exactly
///    one row inserted with the correct `model_id`, `aip_arn`, `set_by`, and
///    `reason`; the legacy column is NOT cleared.
/// 4. **Failure path** — `get_foundation_model` mock returns `Err` → no row
///    inserted, migration as a whole still returns `Ok(())` (no panic, no abort).
/// 5. **Idempotency** — running the migration twice produces exactly one row;
///    the second run is a complete no-op.
///
/// # BUILDER CONTRACT
///
/// Expose:
/// ```rust
/// pub async fn migrate_legacy_aip_endpoints<F, Fut>(
///     candidates: &[EndpointMigrationCandidate],
///     pool: &sqlx::PgPool,
///     get_foundation_model: F,
/// ) -> anyhow::Result<()>
/// where
///     F: Fn(&str) -> Fut + Send + Sync,
///     Fut: std::future::Future<Output = Result<String, String>> + Send,
/// ```
///
/// where:
/// ```rust
/// pub struct EndpointMigrationCandidate {
///     pub endpoint_id: uuid::Uuid,
///     pub legacy_arn:  String,
/// }
/// ```
///
/// in `src/migrations/aip_legacy.rs` (or wherever the builder places it),
/// re-exported from the crate root as `ccag::migrations::aip_legacy::*`.
///
/// The `get_foundation_model` closure receives the raw legacy ARN and must
/// return the logical Anthropic model id (e.g. `"claude-sonnet-4-5-20250929"`).
/// In production it calls `GetInferenceProfile`, parses the ARN tail, and maps
/// via `bedrock_to_anthropic`. In tests we stub it out directly.
///
/// Wiring: call `migrate_legacy_aip_endpoints` in `src/main.rs` after
/// `load_endpoints_with_db` and before the health loop starts.
///
/// Run: `make test-integration`
use ccag::db;
use ccag::db::endpoint_aip_overrides;
use ccag::migrations::aip_legacy::{EndpointMigrationCandidate, migrate_legacy_aip_endpoints};
use uuid::Uuid;

use crate::helpers;

// ── helper ────────────────────────────────────────────────────────────────────

/// Create a bare test endpoint with no `inference_profile_arn`.
async fn create_cri_endpoint(pool: &sqlx::PgPool, name: &str) -> Uuid {
    db::endpoints::create_endpoint(pool, name, None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap_or_else(|e| panic!("create_cri_endpoint({name}) failed: {e}"))
        .id
}

/// Create a test endpoint with `inference_profile_arn` set (legacy AIP column).
async fn create_aip_endpoint(pool: &sqlx::PgPool, name: &str, legacy_arn: &str) -> Uuid {
    // `create_endpoint` signature: (pool, name, role_arn, external_id,
    //   inference_profile_arn, region, routing_prefix, priority)
    db::endpoints::create_endpoint(
        pool,
        name,
        None,
        None,
        Some(legacy_arn),
        "us-east-1",
        "us",
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("create_aip_endpoint({name}) failed: {e}"))
    .id
}

/// A mock `get_foundation_model` that always returns `Ok(model_id.to_string())`.
fn mock_get_model_ok(
    model_id: &'static str,
) -> impl Fn(&str) -> std::future::Ready<Result<String, String>> {
    move |_arn: &str| std::future::ready(Ok(model_id.to_string()))
}

/// A mock `get_foundation_model` that always returns `Err`.
fn mock_get_model_err() -> impl Fn(&str) -> std::future::Ready<Result<String, String>> {
    |_arn: &str| std::future::ready(Err("simulated GetInferenceProfile failure".to_string()))
}

// ── Contract test 1: Skip case A (CRI endpoint, no legacy ARN) ───────────────

/// An endpoint with `inference_profile_arn = NULL` (CRI endpoint) must be
/// completely ignored by the migration runner; no rows are inserted in
/// `endpoint_aip_overrides` for that endpoint.
#[tokio::test]
async fn migration_skip_cri_endpoint_without_legacy_arn() {
    let pool = helpers::setup_test_db().await;
    let cri_id = create_cri_endpoint(&pool, "cri-skip-a").await;

    // Build a candidate list containing only this CRI endpoint.
    // (In production the runner filters from DB; we inject the list here.)
    let candidates: Vec<EndpointMigrationCandidate> = vec![]; // empty — no legacy ARN

    migrate_legacy_aip_endpoints(
        &candidates,
        &pool,
        mock_get_model_ok("claude-sonnet-4-5-20250929"),
    )
    .await
    .expect("migration runner must return Ok even with zero candidates");

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, cri_id)
        .await
        .expect("list_by_endpoint must not fail");

    assert!(
        rows.is_empty(),
        "CRI endpoint (no legacy ARN) must have zero override rows after migration; got {rows:?}"
    );
}

// ── Contract test 2: Skip case B (legacy ARN + pre-existing rows) ─────────────

/// An endpoint that already has ≥1 rows in `endpoint_aip_overrides` must NOT
/// be processed by the migration runner — even though it has a legacy ARN.
/// The pre-inserted row must survive unchanged (same count, same fields).
#[tokio::test]
async fn migration_skip_endpoint_with_existing_rows() {
    let pool = helpers::setup_test_db().await;
    let legacy_arn =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/existing-aip";
    let ep_id = create_aip_endpoint(&pool, "aip-skip-b", legacy_arn).await;

    // Pre-insert a manual override row (simulates a previously migrated or
    // admin-created row).
    endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-5",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/pre-existing",
        "admin",
        Some("manually set"),
    )
    .await
    .expect("pre-insert must succeed");

    // Run migration with this endpoint in the candidate list.
    let candidates = vec![EndpointMigrationCandidate {
        endpoint_id: ep_id,
        legacy_arn: legacy_arn.to_string(),
    }];

    migrate_legacy_aip_endpoints(
        &candidates,
        &pool,
        mock_get_model_ok("claude-opus-4-7"), // would insert a different model if the runner were wrong
    )
    .await
    .expect("migration runner must return Ok");

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .expect("list must succeed");

    assert_eq!(
        rows.len(),
        1,
        "endpoint with a pre-existing row must still have exactly 1 row after migration; the runner must not add a second row; got {rows:?}"
    );
    // The surviving row must be the original, not one written by the migration runner.
    assert_eq!(
        rows[0].model_id, "claude-sonnet-4-5",
        "the surviving row must be the pre-existing admin-inserted row, not a runner-inserted row"
    );
    assert_eq!(
        rows[0].set_by, "admin",
        "set_by must still be 'admin' (the pre-existing row), not 'auto-migration'"
    );
}

// ── Contract test 3: Happy path ───────────────────────────────────────────────

/// Endpoint with a legacy ARN and zero override rows → runner inserts exactly
/// one row with:
/// - `model_id`  = the value returned by `get_foundation_model`
/// - `aip_arn`   = the legacy ARN itself
/// - `set_by`    = "auto-migration"
/// - `reason`    = "migrated from inference_profile_arn column"
///
/// The legacy column must remain set after the migration (not cleared).
#[tokio::test]
async fn migration_happy_path_inserts_one_row() {
    let pool = helpers::setup_test_db().await;
    let legacy_arn =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged";
    let ep_id = create_aip_endpoint(&pool, "aip-happy", legacy_arn).await;

    let expected_model_id = "claude-sonnet-4-5-20250929";

    let candidates = vec![EndpointMigrationCandidate {
        endpoint_id: ep_id,
        legacy_arn: legacy_arn.to_string(),
    }];

    migrate_legacy_aip_endpoints(&candidates, &pool, mock_get_model_ok(expected_model_id))
        .await
        .expect("migration runner must return Ok on happy path");

    // Exactly one row must be inserted.
    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .expect("list must succeed");

    assert_eq!(
        rows.len(),
        1,
        "happy path: exactly one override row must be inserted; got {rows:?}"
    );

    let row = &rows[0];

    assert_eq!(
        row.model_id, expected_model_id,
        "inserted model_id must equal the value returned by get_foundation_model"
    );
    assert_eq!(
        row.aip_arn, legacy_arn,
        "inserted aip_arn must equal the legacy ARN"
    );
    assert_eq!(
        row.set_by, "auto-migration",
        "set_by must be exactly 'auto-migration'"
    );
    assert_eq!(
        row.reason.as_deref(),
        Some("migrated from inference_profile_arn column"),
        "reason must be exactly 'migrated from inference_profile_arn column'"
    );

    // Verify the legacy column was NOT cleared: re-fetch the endpoint from DB.
    let ep_row =
        sqlx::query_as::<_, ccag::db::schema::Endpoint>("SELECT * FROM endpoints WHERE id = $1")
            .bind(ep_id)
            .fetch_one(&pool)
            .await
            .expect("re-fetch endpoint must succeed");

    assert_eq!(
        ep_row.inference_profile_arn.as_deref(),
        Some(legacy_arn),
        "inference_profile_arn column must NOT be cleared after auto-migration (intentional during transition)"
    );
}

// ── Contract test 4: Failure path (GetInferenceProfile error) ────────────────

/// When `get_foundation_model` returns `Err`, the migration runner must:
/// - Insert zero rows for that endpoint.
/// - Continue without panicking.
/// - Return `Ok(())` (startup is not blocked).
#[tokio::test]
async fn migration_failure_path_no_row_inserted_startup_continues() {
    let pool = helpers::setup_test_db().await;
    let legacy_arn =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/broken-aip";
    let ep_id = create_aip_endpoint(&pool, "aip-failure", legacy_arn).await;

    let candidates = vec![EndpointMigrationCandidate {
        endpoint_id: ep_id,
        legacy_arn: legacy_arn.to_string(),
    }];

    // The mock returns an error simulating a failed GetInferenceProfile call.
    let result = migrate_legacy_aip_endpoints(&candidates, &pool, mock_get_model_err()).await;

    assert!(
        result.is_ok(),
        "migration runner must return Ok even when get_foundation_model fails; startup must not be blocked; got {result:?}"
    );

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .expect("list must succeed");

    assert!(
        rows.is_empty(),
        "when get_foundation_model fails, zero override rows must be inserted; got {rows:?}"
    );
}

/// Multi-endpoint mix: CRI endpoint untouched, healthy AIP endpoint migrated,
/// broken AIP endpoint logged-and-skipped — all in one runner call.
#[tokio::test]
async fn migration_mixed_endpoints_correct_per_endpoint_behaviour() {
    let pool = helpers::setup_test_db().await;

    // (a) CRI endpoint — NOT in the candidate list (already filtered upstream)
    let cri_id = create_cri_endpoint(&pool, "cri-mixed").await;

    // (b) Healthy AIP endpoint
    let healthy_arn =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/healthy";
    let healthy_id = create_aip_endpoint(&pool, "aip-healthy-mixed", healthy_arn).await;

    // (c) Broken AIP endpoint
    let broken_arn = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/broken";
    let broken_id = create_aip_endpoint(&pool, "aip-broken-mixed", broken_arn).await;

    // Candidate list contains only the AIP endpoints (CRI has no legacy ARN).
    let candidates = vec![
        EndpointMigrationCandidate {
            endpoint_id: healthy_id,
            legacy_arn: healthy_arn.to_string(),
        },
        EndpointMigrationCandidate {
            endpoint_id: broken_id,
            legacy_arn: broken_arn.to_string(),
        },
    ];

    // Mock: healthy endpoint returns Ok, broken endpoint returns Err.
    let healthy_id_copy = healthy_id;
    let get_model = move |arn: &str| {
        // Only "healthy" ARN succeeds; all others fail.
        if arn.contains("healthy") {
            std::future::ready(Ok("claude-sonnet-4-5-20250929".to_string()))
        } else {
            std::future::ready(Err("simulated failure".to_string()))
        }
    };

    let result = migrate_legacy_aip_endpoints(&candidates, &pool, get_model).await;
    assert!(
        result.is_ok(),
        "runner must return Ok on mixed-endpoint run"
    );

    // CRI endpoint: still zero rows
    let cri_rows = endpoint_aip_overrides::list_by_endpoint(&pool, cri_id)
        .await
        .unwrap();
    assert!(cri_rows.is_empty(), "CRI endpoint must have zero rows");

    // Healthy AIP endpoint: exactly one row
    let healthy_rows = endpoint_aip_overrides::list_by_endpoint(&pool, healthy_id_copy)
        .await
        .unwrap();
    assert_eq!(
        healthy_rows.len(),
        1,
        "healthy AIP endpoint must have exactly one row after migration"
    );
    assert_eq!(healthy_rows[0].set_by, "auto-migration");

    // Broken AIP endpoint: zero rows
    let broken_rows = endpoint_aip_overrides::list_by_endpoint(&pool, broken_id)
        .await
        .unwrap();
    assert!(
        broken_rows.is_empty(),
        "broken AIP endpoint must have zero rows after migration"
    );
}

// ── Contract test 5: Idempotency ─────────────────────────────────────────────

/// Running the migration twice on the same DB state must be a no-op on the
/// second run.  After both runs, the endpoint still has exactly one override
/// row, and its `set_at` / other fields are unchanged from the first run.
#[tokio::test]
async fn migration_idempotent_second_run_is_noop() {
    let pool = helpers::setup_test_db().await;
    let legacy_arn =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/idem-aip";
    let ep_id = create_aip_endpoint(&pool, "aip-idempotent", legacy_arn).await;

    let candidates = vec![EndpointMigrationCandidate {
        endpoint_id: ep_id,
        legacy_arn: legacy_arn.to_string(),
    }];

    let model_id = "claude-sonnet-4-5-20250929";

    // First run: inserts one row.
    migrate_legacy_aip_endpoints(&candidates, &pool, mock_get_model_ok(model_id))
        .await
        .expect("first run must succeed");

    let rows_after_first = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert_eq!(
        rows_after_first.len(),
        1,
        "should have one row after first run"
    );
    let set_at_first = rows_after_first[0].set_at;

    // Second run: must be a complete no-op — the predicate "zero rows for this
    // endpoint" is now false, so the runner skips the endpoint.
    migrate_legacy_aip_endpoints(&candidates, &pool, mock_get_model_ok(model_id))
        .await
        .expect("second run must also succeed");

    let rows_after_second = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();

    assert_eq!(
        rows_after_second.len(),
        1,
        "second run must not add a duplicate row; still exactly one row expected"
    );
    assert_eq!(
        rows_after_second[0].set_at, set_at_first,
        "set_at must be unchanged on the second run (the row was not touched)"
    );
    assert_eq!(
        rows_after_second[0].set_by, "auto-migration",
        "set_by must still be 'auto-migration' after second run"
    );
}

// ── Race / concurrent safety (sequential idempotency stand-in) ────────────────

/// Calling `migrate_legacy_aip_endpoints` three times in a row (in the same
/// process, serially) must not produce PK violations and must leave exactly one
/// row per AIP endpoint.
///
/// This covers the "double-startup" scenario described in the task spec.
#[tokio::test]
async fn migration_serial_triple_run_no_pk_violation() {
    let pool = helpers::setup_test_db().await;
    let legacy_arn =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/triple-run-aip";
    let ep_id = create_aip_endpoint(&pool, "aip-triple", legacy_arn).await;

    let candidates = vec![EndpointMigrationCandidate {
        endpoint_id: ep_id,
        legacy_arn: legacy_arn.to_string(),
    }];

    let model_id = "claude-haiku-4-5-20251001";

    for run in 1..=3 {
        let result =
            migrate_legacy_aip_endpoints(&candidates, &pool, mock_get_model_ok(model_id)).await;
        assert!(
            result.is_ok(),
            "run {run}: migration runner must not return Err (no PK violation propagated)"
        );
    }

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        1,
        "after 3 serial runs, must still have exactly one row (no duplicates)"
    );
}
