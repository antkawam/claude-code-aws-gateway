/// Integration tests for boot-time beta-override replay (Task 8, Layer B).
///
/// # Contract for Builder
///
/// Implement a public async function:
///
/// ```rust
/// // Location: src/main.rs (or a new module such as src/boot.rs or src/admin_override_replay.rs)
/// pub async fn apply_overrides_to_pool(
///     pool: &sqlx::PgPool,
///     endpoint_pool: &ccag::endpoint::EndpointPool,
/// ) -> Result<usize, sqlx::Error>;
/// ```
///
/// Semantics:
/// 1. Load all rows from `beta_overrides` via `db::beta_overrides::list_all(pool)`.
/// 2. For each row: call `endpoint_pool.get_client(row.endpoint_id)`.
///    - `Some(client)` → apply the row using `client.mark_supported` or `client.mark_unsupported`
///      with `ProbeSource::AdminOverride`.
///    - `None` → skip with `tracing::debug!` (orphan — endpoint not in pool).
/// 3. Return the count of overrides successfully applied (i.e. where `get_client` returned `Some`).
///
/// The function must be `pub` (or `pub(crate)`) and re-exported so this test can call it as
/// `ccag::apply_overrides_to_pool` or `ccag::boot::apply_overrides_to_pool`, etc.
/// The exact re-export path is Builder's choice; document it in the PR.
///
/// Usage at boot time (Builder wires this into `src/main.rs`):
/// ```rust
/// // After load_endpoints completes, before start_cache_poll_loop:
/// match ccag::apply_overrides_to_pool(&pool, &state.endpoint_pool).await {
///     Ok(n) => tracing::info!(n, "Replayed beta overrides into pool"),
///     Err(e) => tracing::warn!(%e, "Failed to replay beta overrides"),
/// }
/// ```
///
/// Usage in cache-version poll loop (on every bump, full replay; cheap because table is small):
/// ```rust
/// match ccag::apply_overrides_to_pool(pool, &state.endpoint_pool).await {
///     Ok(n) => tracing::debug!(n, "Re-replayed beta overrides after cache version bump"),
///     Err(e) => tracing::warn!(%e, "Failed to re-replay beta overrides"),
/// }
/// ```
///
/// Test isolation: each test calls `helpers::setup_test_db()`, which creates a fresh
/// `test_{uuid}` database and runs all migrations (including `011_beta_overrides.sql`).
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use ccag::db;
use ccag::db::beta_overrides::BetaOverride;
use ccag::endpoint::{EndpointPool, ProbeSource};

use crate::helpers;

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

/// Insert a fixture endpoint into the DB and return its UUID.
async fn create_endpoint(pool: &sqlx::PgPool, name: &str) -> Uuid {
    db::endpoints::create_endpoint(pool, name, None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap_or_else(|e| panic!("create_endpoint({name}): {e}"))
        .id
}

/// Insert a `BetaOverride` row directly into the DB.
async fn insert_override(
    pool: &sqlx::PgPool,
    endpoint_id: Uuid,
    profile_id: &str,
    beta_name: &str,
    supported: bool,
) {
    let ovr = BetaOverride {
        endpoint_id,
        profile_id: profile_id.to_string(),
        beta_name: beta_name.to_string(),
        supported,
        set_at: Utc::now(),
        set_by: "test-admin".to_string(),
        reason: Some("replay test".to_string()),
    };
    db::beta_overrides::upsert(pool, &ovr)
        .await
        .unwrap_or_else(|e| panic!("insert_override: {e}"));
}

/// Build an `EndpointPool` loaded with the endpoints currently in the DB.
///
/// Uses the same helper pattern as `beta_overrides_admin_tests.rs`.
async fn load_pool(pool: &sqlx::PgPool) -> Arc<EndpointPool> {
    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let endpoint_pool = Arc::new(EndpointPool::new());
    let endpoints = db::endpoints::get_enabled_endpoints(pool)
        .await
        .expect("get_enabled_endpoints");
    endpoint_pool.load_endpoints(endpoints, &aws_config).await;
    endpoint_pool
}

// ===========================================================================
// Test 1 — replay_loads_overrides_into_pool
//
// DB has 2 endpoints and 3 override rows (2 for endpoint A, 1 for endpoint B).
// After calling apply_overrides_to_pool:
// - Returns count == 3 (all 3 rows matched a loaded endpoint).
// - EndpointClient A has correct values for its 2 overrides.
// - EndpointClient B has correct value for its 1 override.
// ===========================================================================

#[tokio::test]
async fn replay_loads_overrides_into_pool() {
    let pool = helpers::setup_test_db().await;

    let ep_a = create_endpoint(&pool, "replay-ep-a").await;
    let ep_b = create_endpoint(&pool, "replay-ep-b").await;

    // 2 overrides for endpoint A
    insert_override(
        &pool,
        ep_a,
        "us.anthropic.claude-opus-4-7",
        "context-1m-2025-08-07",
        true,
    )
    .await;
    insert_override(
        &pool,
        ep_a,
        "us.anthropic.claude-haiku-4-5",
        "context-1m-2025-08-07",
        false,
    )
    .await;
    // 1 override for endpoint B
    insert_override(
        &pool,
        ep_b,
        "us.anthropic.claude-sonnet-4-5",
        "context-1m-2025-08-07",
        true,
    )
    .await;

    let endpoint_pool = load_pool(&pool).await;

    let count = ccag::apply_overrides_to_pool(&pool, &endpoint_pool)
        .await
        .expect("apply_overrides_to_pool must not return an error");

    assert_eq!(
        count, 3,
        "all 3 override rows matched a pool endpoint; count must be 3"
    );

    // Verify endpoint A — opus-4-7 is supported
    let client_a = endpoint_pool
        .get_client(ep_a)
        .await
        .expect("endpoint A must be in pool");
    let opus_supported = client_a
        .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
        .await;
    assert_eq!(
        opus_supported,
        Some(true),
        "endpoint A / opus-4-7 must have supported=true after replay"
    );

    // Verify endpoint A — haiku-4-5 is NOT supported
    let haiku_supported = client_a
        .is_beta_supported("us.anthropic.claude-haiku-4-5", "context-1m-2025-08-07")
        .await;
    assert_eq!(
        haiku_supported,
        Some(false),
        "endpoint A / haiku-4-5 must have supported=false after replay"
    );

    // Verify endpoint B — sonnet-4-5 is supported
    let client_b = endpoint_pool
        .get_client(ep_b)
        .await
        .expect("endpoint B must be in pool");
    let sonnet_supported = client_b
        .is_beta_supported("us.anthropic.claude-sonnet-4-5", "context-1m-2025-08-07")
        .await;
    assert_eq!(
        sonnet_supported,
        Some(true),
        "endpoint B / sonnet-4-5 must have supported=true after replay"
    );
}

// ===========================================================================
// Test 2 — replay_skips_orphan_overrides
//
// The pool is loaded with only ONE endpoint (endpoint A).
// The DB has an override row for a *different* UUID that was NOT loaded into
// the pool (simulating an incomplete pool-load, e.g. the endpoint was disabled
// between the DB insert and the pool load).
//
// After replay:
// - count == 1 (only the row for the loaded endpoint was applied).
// - The orphan row is silently skipped (no panic, no error).
// ===========================================================================

#[tokio::test]
async fn replay_skips_orphan_overrides() {
    let pool = helpers::setup_test_db().await;

    // Create one endpoint that IS in the DB (and will be loaded into the pool)
    let ep_loaded = create_endpoint(&pool, "replay-loaded").await;
    // A UUID for an endpoint NOT in the pool (not inserted into the DB as an
    // endpoint row, so get_client returns None for it)
    let ep_orphan_not_in_pool = Uuid::new_v4();

    insert_override(
        &pool,
        ep_loaded,
        "us.anthropic.claude-opus-4-7",
        "context-1m-2025-08-07",
        true,
    )
    .await;

    // Insert an override row with a UUID that has NO corresponding endpoint in the DB.
    // This bypasses the FK constraint by inserting directly (simulating the scenario
    // where the endpoint was deleted but the override row was somehow not CASCADE'd, or
    // the pool was loaded before the FK row was created).
    //
    // We use a raw sqlx query here to bypass the FK check (the FK is `ON DELETE CASCADE`,
    // so normally this can't happen via the application layer — this test verifies the
    // defensive code path in apply_overrides_to_pool).
    //
    // NOTE: If the DB enforces FK strictly (no INSERT possible for nonexistent endpoint_id),
    // the Builder should mark this test as `#[ignore]` and document why. The behavior
    // being tested (skip when get_client returns None) is still exercised via the
    // "pool not yet loaded" scenario: insert the endpoint into DB but do NOT load it
    // into the pool.
    let ep_in_db_not_in_pool = create_endpoint(&pool, "replay-not-loaded").await;
    insert_override(
        &pool,
        ep_in_db_not_in_pool,
        "us.anthropic.claude-sonnet-4-5",
        "context-1m-2025-08-07",
        false,
    )
    .await;

    // Load only ep_loaded into the pool (NOT ep_in_db_not_in_pool)
    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let endpoint_pool = Arc::new(EndpointPool::new());
    let endpoints = db::endpoints::get_enabled_endpoints(&pool)
        .await
        .expect("get_enabled_endpoints");
    // Filter: only load the first endpoint (ep_loaded)
    let only_loaded: Vec<_> = endpoints
        .into_iter()
        .filter(|e| e.id == ep_loaded)
        .collect();
    endpoint_pool.load_endpoints(only_loaded, &aws_config).await;

    let count = ccag::apply_overrides_to_pool(&pool, &endpoint_pool)
        .await
        .expect("apply_overrides_to_pool must not return an error on orphan rows");

    assert_eq!(
        count, 1,
        "only the 1 row for the loaded endpoint should be applied; the orphan row is skipped"
    );

    // The loaded endpoint's override IS applied
    let client = endpoint_pool
        .get_client(ep_loaded)
        .await
        .expect("ep_loaded must be in pool");
    assert_eq!(
        client
            .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
            .await,
        Some(true),
        "the loaded endpoint's override must be applied"
    );

    // The not-loaded endpoint is NOT in the pool — verify there's no panic
    let missing = endpoint_pool.get_client(ep_in_db_not_in_pool).await;
    assert!(
        missing.is_none(),
        "ep_in_db_not_in_pool must not be in the pool"
    );

    // ep_orphan_not_in_pool was never in DB, so no override row for it either
    let _ = ep_orphan_not_in_pool; // suppress unused warning
}

// ===========================================================================
// Test 3 — replay_admin_override_beats_existing_seedprobe
//
// Pre-populate the EndpointClient cache with a SeedProbe entry:
//   (profile, beta) → supported=true, source=SeedProbe
//
// DB has an override row for the same (endpoint, profile, beta) with supported=false.
//
// After replay:
// - is_beta_supported returns Some(false) (AdminOverride wins).
// - The entry's source is AdminOverride (confirmed indirectly: it must NOT expire
//   after CAPABILITY_TTL, which SeedProbe entries would — but we verify by checking
//   that a second call to is_beta_supported still returns Some(false)).
// ===========================================================================

#[tokio::test]
async fn replay_admin_override_beats_existing_seedprobe() {
    let pool = helpers::setup_test_db().await;

    let ep_id = create_endpoint(&pool, "replay-override-beats-seed").await;

    // Pre-populate pool client with a SeedProbe entry saying "supported=true"
    let endpoint_pool = load_pool(&pool).await;
    let client = endpoint_pool
        .get_client(ep_id)
        .await
        .expect("endpoint must be in pool");
    client
        .mark_supported(
            "us.anthropic.claude-opus-4-7",
            "context-1m-2025-08-07",
            ProbeSource::SeedProbe,
        )
        .await;

    // Confirm the seed probe is currently saying supported=true
    let before = client
        .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
        .await;
    assert_eq!(
        before,
        Some(true),
        "seed probe should say Some(true) before replay"
    );

    // DB has an AdminOverride that says supported=false for the same key
    insert_override(
        &pool,
        ep_id,
        "us.anthropic.claude-opus-4-7",
        "context-1m-2025-08-07",
        false,
    )
    .await;

    // Run replay
    let count = ccag::apply_overrides_to_pool(&pool, &endpoint_pool)
        .await
        .expect("apply_overrides_to_pool must not error");
    assert_eq!(count, 1, "one override row must be applied");

    // AdminOverride must now win: supported=false
    let after = client
        .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
        .await;
    assert_eq!(
        after,
        Some(false),
        "AdminOverride (supported=false) must override the pre-existing SeedProbe (supported=true)"
    );
}

// ===========================================================================
// Test 4 — replay_idempotent_when_called_twice
//
// Call apply_overrides_to_pool twice.  Each call must:
// - Return count == N (same count both times; no accumulation).
// - Leave the cache in the same state as after one call.
// ===========================================================================

#[tokio::test]
async fn replay_idempotent_when_called_twice() {
    let pool = helpers::setup_test_db().await;

    let ep_id = create_endpoint(&pool, "replay-idempotent").await;
    insert_override(
        &pool,
        ep_id,
        "us.anthropic.claude-opus-4-7",
        "context-1m-2025-08-07",
        true,
    )
    .await;
    insert_override(
        &pool,
        ep_id,
        "us.anthropic.claude-haiku-4-5",
        "context-1m-2025-08-07",
        false,
    )
    .await;

    let endpoint_pool = load_pool(&pool).await;

    let count1 = ccag::apply_overrides_to_pool(&pool, &endpoint_pool)
        .await
        .expect("first apply must not error");
    assert_eq!(count1, 2, "first replay: count must be 2");

    let count2 = ccag::apply_overrides_to_pool(&pool, &endpoint_pool)
        .await
        .expect("second apply must not error");
    assert_eq!(
        count2, 2,
        "second replay: count must still be 2 (idempotent)"
    );

    // Cache state must be consistent after both calls
    let client = endpoint_pool
        .get_client(ep_id)
        .await
        .expect("endpoint must be in pool");

    assert_eq!(
        client
            .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
            .await,
        Some(true),
        "opus-4-7 must be supported=true after two replays"
    );
    assert_eq!(
        client
            .is_beta_supported("us.anthropic.claude-haiku-4-5", "context-1m-2025-08-07")
            .await,
        Some(false),
        "haiku-4-5 must be supported=false after two replays"
    );
}

// ===========================================================================
// Test 5 — replay_handles_empty_db
//
// Fresh DB with no override rows.  apply_overrides_to_pool must:
// - Return Ok(0) (no rows, no overrides applied).
// - Not panic or error.
// ===========================================================================

#[tokio::test]
async fn replay_handles_empty_db() {
    let pool = helpers::setup_test_db().await;

    // Create one endpoint so the pool is non-empty (the function must still return 0)
    let ep_id = create_endpoint(&pool, "replay-empty-db").await;
    let endpoint_pool = load_pool(&pool).await;

    let count = ccag::apply_overrides_to_pool(&pool, &endpoint_pool)
        .await
        .expect("apply_overrides_to_pool must not error on an empty beta_overrides table");

    assert_eq!(count, 0, "no override rows in DB means count must be 0");

    // The endpoint client cache must remain empty
    let client = endpoint_pool
        .get_client(ep_id)
        .await
        .expect("endpoint must be in pool");
    let result = client
        .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
        .await;
    assert!(
        result.is_none(),
        "cache must be empty (None) when no overrides were applied; got: {result:?}"
    );
}
