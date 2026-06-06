/// Integration tests for `db::beta_overrides` — CRUD against a real Postgres DB.
///
/// The Builder must create:
///   - `migrations/011_beta_overrides.sql` — the `beta_overrides` table
///   - `src/db/beta_overrides.rs` — `BetaOverride`, `list_all`, `list_for_endpoint`, `upsert`, `delete`
///   - `src/db/mod.rs` — `pub mod beta_overrides;`
///
/// Test isolation: each test calls `helpers::setup_test_db()`, which creates a
/// fresh `test_{uuid}` database, runs all migrations (including 011), and returns
/// a pool. No shared state between tests.
use chrono::Utc;
use uuid::Uuid;

use ccag::db;
use ccag::db::beta_overrides::BetaOverride;

use crate::helpers;

// ---------------------------------------------------------------------------
// Helper: create a fixture endpoint in the DB
// ---------------------------------------------------------------------------

async fn create_fixture_endpoint(pool: &sqlx::PgPool, name: &str) -> Uuid {
    let ep = db::endpoints::create_endpoint(pool, name, None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap_or_else(|e| panic!("create_fixture_endpoint({name}): {e}"));
    ep.id
}

// ---------------------------------------------------------------------------
// Helper: build a BetaOverride value for a given endpoint
// ---------------------------------------------------------------------------

fn make_override(endpoint_id: Uuid, profile_id: &str, beta_name: &str, supported: bool) -> BetaOverride {
    BetaOverride {
        endpoint_id,
        profile_id: profile_id.to_string(),
        beta_name: beta_name.to_string(),
        supported,
        set_at: Utc::now(),
        set_by: "test-admin".to_string(),
        reason: Some("test reason".to_string()),
    }
}

// ===========================================================================
// Test 1 — migration_creates_table_with_expected_columns
// ===========================================================================

#[tokio::test]
async fn migration_creates_table_with_expected_columns() {
    let pool = helpers::setup_test_db().await;

    // Query information_schema.columns for the beta_overrides table
    let columns: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT column_name, data_type
           FROM information_schema.columns
           WHERE table_name = 'beta_overrides'
           ORDER BY ordinal_position"#,
    )
    .fetch_all(&pool)
    .await
    .expect("failed to query information_schema.columns");

    // Collect just names for easy assertion
    let col_names: Vec<&str> = columns.iter().map(|(n, _)| n.as_str()).collect();

    assert!(
        col_names.contains(&"endpoint_id"),
        "expected column 'endpoint_id', found: {col_names:?}"
    );
    assert!(
        col_names.contains(&"profile_id"),
        "expected column 'profile_id', found: {col_names:?}"
    );
    assert!(
        col_names.contains(&"beta_name"),
        "expected column 'beta_name', found: {col_names:?}"
    );
    assert!(
        col_names.contains(&"supported"),
        "expected column 'supported', found: {col_names:?}"
    );
    assert!(
        col_names.contains(&"set_at"),
        "expected column 'set_at', found: {col_names:?}"
    );
    assert!(
        col_names.contains(&"set_by"),
        "expected column 'set_by', found: {col_names:?}"
    );
    assert!(
        col_names.contains(&"reason"),
        "expected column 'reason', found: {col_names:?}"
    );

    // Verify specific types
    let type_map: std::collections::HashMap<&str, &str> = columns
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();

    assert_eq!(
        type_map.get("supported"),
        Some(&"boolean"),
        "supported must be boolean"
    );
    // set_at: Postgres reports TIMESTAMPTZ as "timestamp with time zone"
    assert_eq!(
        type_map.get("set_at"),
        Some(&"timestamp with time zone"),
        "set_at must be timestamp with time zone"
    );
    // endpoint_id: Postgres reports UUID as "uuid"
    assert_eq!(
        type_map.get("endpoint_id"),
        Some(&"uuid"),
        "endpoint_id must be uuid"
    );
}

// ===========================================================================
// Test 2 — upsert_inserts_new_row
// ===========================================================================

#[tokio::test]
async fn upsert_inserts_new_row() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_fixture_endpoint(&pool, "ep-upsert-new").await;

    let ovr = make_override(ep_id, "us.anthropic.claude-opus-4-7", "context-1m-2025-08-07", true);
    db::beta_overrides::upsert(&pool, &ovr)
        .await
        .expect("upsert should succeed");

    let rows = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list_for_endpoint should succeed");

    assert_eq!(rows.len(), 1, "expected exactly one row after upsert");
    assert_eq!(rows[0].endpoint_id, ep_id);
    assert_eq!(rows[0].profile_id, "us.anthropic.claude-opus-4-7");
    assert_eq!(rows[0].beta_name, "context-1m-2025-08-07");
    assert!(rows[0].supported, "supported flag should be true");
    assert_eq!(rows[0].set_by, "test-admin");
    assert_eq!(rows[0].reason.as_deref(), Some("test reason"));
}

// ===========================================================================
// Test 3 — upsert_overwrites_existing
// ===========================================================================

#[tokio::test]
async fn upsert_overwrites_existing() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_fixture_endpoint(&pool, "ep-upsert-overwrite").await;

    // First upsert: supported = true
    let ovr1 = make_override(ep_id, "us.anthropic.claude-opus-4-7", "context-1m-2025-08-07", true);
    db::beta_overrides::upsert(&pool, &ovr1)
        .await
        .expect("first upsert should succeed");

    // Second upsert: same PK, supported = false
    let ovr2 = BetaOverride {
        supported: false,
        set_by: "admin-2".to_string(),
        reason: None,
        ..make_override(ep_id, "us.anthropic.claude-opus-4-7", "context-1m-2025-08-07", false)
    };
    db::beta_overrides::upsert(&pool, &ovr2)
        .await
        .expect("second upsert should succeed");

    let rows = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list_for_endpoint should succeed");

    assert_eq!(rows.len(), 1, "upsert should produce exactly one row, not two");
    assert!(!rows[0].supported, "second upsert (supported=false) must win");
    assert_eq!(rows[0].set_by, "admin-2", "set_by should be from the second upsert");
    assert!(rows[0].reason.is_none(), "reason should be None from second upsert");
}

// ===========================================================================
// Test 4 — list_all_aggregates_across_endpoints
// ===========================================================================

#[tokio::test]
async fn list_all_aggregates_across_endpoints() {
    let pool = helpers::setup_test_db().await;
    let ep1 = create_fixture_endpoint(&pool, "ep-list-all-1").await;
    let ep2 = create_fixture_endpoint(&pool, "ep-list-all-2").await;

    db::beta_overrides::upsert(
        &pool,
        &make_override(ep1, "us.anthropic.claude-opus-4-7", "context-1m-2025-08-07", true),
    )
    .await
    .expect("upsert ep1 override");

    db::beta_overrides::upsert(
        &pool,
        &make_override(ep2, "eu.anthropic.claude-sonnet-4-7", "interleaved-thinking-2025-05-14", false),
    )
    .await
    .expect("upsert ep2 override");

    let all = db::beta_overrides::list_all(&pool)
        .await
        .expect("list_all should succeed");

    assert_eq!(all.len(), 2, "list_all should return overrides for both endpoints");

    let ep1_row = all.iter().find(|r| r.endpoint_id == ep1);
    let ep2_row = all.iter().find(|r| r.endpoint_id == ep2);

    assert!(ep1_row.is_some(), "list_all should include ep1's override");
    assert!(ep2_row.is_some(), "list_all should include ep2's override");
    assert!(ep1_row.unwrap().supported);
    assert!(!ep2_row.unwrap().supported);
}

// ===========================================================================
// Test 5 — delete_removes_specific_row_only
// ===========================================================================

#[tokio::test]
async fn delete_removes_specific_row_only() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_fixture_endpoint(&pool, "ep-delete-specific").await;

    // Insert three overrides on the same endpoint, different (profile, beta) tuples
    let pairs: &[(&str, &str)] = &[
        ("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07"),
        ("us.anthropic.claude-sonnet-4-7", "context-1m-2025-08-07"),
        ("us.anthropic.claude-haiku-4-5", "context-1m-2025-08-07"),
    ];
    for (profile, beta) in pairs {
        db::beta_overrides::upsert(&pool, &make_override(ep_id, profile, beta, true))
            .await
            .expect("upsert should succeed");
    }

    // Delete the middle one
    let deleted = db::beta_overrides::delete(
        &pool,
        ep_id,
        "us.anthropic.claude-sonnet-4-7",
        "context-1m-2025-08-07",
    )
    .await
    .expect("delete should succeed");

    assert_eq!(deleted, 1, "delete should report 1 row affected");

    let remaining = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list_for_endpoint should succeed");

    assert_eq!(remaining.len(), 2, "two overrides should remain after deleting one");
    let remaining_profiles: Vec<&str> = remaining.iter().map(|r| r.profile_id.as_str()).collect();
    assert!(
        remaining_profiles.contains(&"us.anthropic.claude-opus-4-7"),
        "opus override should remain"
    );
    assert!(
        remaining_profiles.contains(&"us.anthropic.claude-haiku-4-5"),
        "haiku override should remain"
    );
    assert!(
        !remaining_profiles.contains(&"us.anthropic.claude-sonnet-4-7"),
        "sonnet override should be deleted"
    );
}

// ===========================================================================
// Test 6 — delete_returns_zero_for_missing_row
// ===========================================================================

#[tokio::test]
async fn delete_returns_zero_for_missing_row() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_fixture_endpoint(&pool, "ep-delete-missing").await;

    // Attempt to delete a row that was never inserted
    let deleted = db::beta_overrides::delete(
        &pool,
        ep_id,
        "us.anthropic.nonexistent-profile",
        "context-1m-2025-08-07",
    )
    .await
    .expect("delete of missing row should not error");

    assert_eq!(
        deleted, 0,
        "delete of a nonexistent override must return 0 rows affected"
    );
}

// ===========================================================================
// Test 7 — cascade_on_endpoint_delete
// ===========================================================================

#[tokio::test]
async fn cascade_on_endpoint_delete() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_fixture_endpoint(&pool, "ep-cascade-delete").await;

    // Insert an override for the endpoint
    db::beta_overrides::upsert(
        &pool,
        &make_override(ep_id, "us.anthropic.claude-opus-4-7", "context-1m-2025-08-07", true),
    )
    .await
    .expect("upsert should succeed before endpoint deletion");

    // Verify it exists
    let before = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list before delete");
    assert_eq!(before.len(), 1, "sanity: override exists before endpoint deletion");

    // Delete the endpoint — should cascade to beta_overrides
    db::endpoints::delete_endpoint(&pool, ep_id)
        .await
        .expect("delete endpoint should succeed");

    // The override row must also be gone (FK ON DELETE CASCADE)
    let after = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list after endpoint delete");
    assert_eq!(
        after.len(),
        0,
        "FK CASCADE must delete the override when the endpoint is deleted"
    );
}

// ===========================================================================
// Test 8 — set_at_defaults_to_now
// ===========================================================================

#[tokio::test]
async fn set_at_defaults_to_now() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_fixture_endpoint(&pool, "ep-set-at-default").await;

    let before_insert = Utc::now();

    // The `upsert` implementation may use DEFAULT now() for set_at when the
    // caller's set_at is close to the current time. We simply pass Utc::now()
    // and verify the stored value is within ±5 seconds of now.
    let ovr = make_override(ep_id, "us.anthropic.claude-opus-4-7", "context-1m-2025-08-07", true);
    db::beta_overrides::upsert(&pool, &ovr)
        .await
        .expect("upsert should succeed");

    let after_insert = Utc::now();

    let rows = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list_for_endpoint should succeed");

    assert_eq!(rows.len(), 1);
    let stored_set_at = rows[0].set_at;

    assert!(
        stored_set_at >= before_insert - chrono::Duration::seconds(5),
        "set_at ({stored_set_at}) should be at or after test start ({before_insert})"
    );
    assert!(
        stored_set_at <= after_insert + chrono::Duration::seconds(5),
        "set_at ({stored_set_at}) should be at or before test end ({after_insert})"
    );
}
