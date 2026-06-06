/// Integration tests for the `endpoint_aip_overrides` CRUD layer.
///
/// Contract points tested:
/// - insert + list_by_endpoint happy path
/// - delete_by_model removes exactly one row
/// - FK CASCADE: deleting the parent endpoint removes all override rows
/// - PK violation on duplicate (endpoint_id, model_id) → sqlx database error
///   with code 23505 (the pattern the admin handler will downcast to return 409)
///
/// These tests require a real Postgres instance. Run with `make test-integration`.
use ccag::db;
use ccag::db::endpoint_aip_overrides::{self, AipOverride};
use uuid::Uuid;

use crate::helpers;

// ── helper ────────────────────────────────────────────────────────────────────

async fn create_test_endpoint(pool: &sqlx::PgPool, name: &str) -> Uuid {
    db::endpoints::create_endpoint(pool, name, None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap_or_else(|e| panic!("create_test_endpoint({name}) failed: {e}"))
        .id
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// `insert` followed by `list_by_endpoint` returns the inserted row with all
/// fields populated correctly.
#[tokio::test]
async fn aip_override_insert_and_list() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-aip-1").await;

    endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-5",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged",
        "test-user",
        Some("test insertion"),
    )
    .await
    .expect("insert should succeed");

    let rows: Vec<AipOverride> = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .expect("list_by_endpoint should succeed");

    assert_eq!(rows.len(), 1, "should have exactly one override row");
    let row = &rows[0];
    assert_eq!(row.endpoint_id, ep_id);
    assert_eq!(row.model_id, "claude-sonnet-4-5");
    assert_eq!(
        row.aip_arn,
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged"
    );
    assert_eq!(row.set_by, "test-user");
    assert_eq!(row.reason.as_deref(), Some("test insertion"));
}

/// `insert` with `reason = None` is accepted; the reason column is nullable.
#[tokio::test]
async fn aip_override_insert_null_reason() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-aip-null-reason").await;

    endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-opus-4-7",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-tagged",
        "admin",
        None,
    )
    .await
    .expect("insert with null reason should succeed");

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .expect("list should succeed");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].reason.is_none(), "reason should be None");
}

/// `list_by_endpoint` returns only the rows for the requested endpoint;
/// rows for other endpoints are not included.
#[tokio::test]
async fn aip_override_list_scoped_to_endpoint() {
    let pool = helpers::setup_test_db().await;
    let ep_a = create_test_endpoint(&pool, "ep-a").await;
    let ep_b = create_test_endpoint(&pool, "ep-b").await;

    endpoint_aip_overrides::insert(
        &pool,
        ep_a,
        "claude-sonnet-4-5",
        "arn:aws:bedrock:us-east-1:111111111111:application-inference-profile/sonnet-a",
        "admin",
        None,
    )
    .await
    .unwrap();

    endpoint_aip_overrides::insert(
        &pool,
        ep_b,
        "claude-sonnet-4-5",
        "arn:aws:bedrock:us-east-1:222222222222:application-inference-profile/sonnet-b",
        "admin",
        None,
    )
    .await
    .unwrap();

    let rows_a = endpoint_aip_overrides::list_by_endpoint(&pool, ep_a)
        .await
        .unwrap();
    let rows_b = endpoint_aip_overrides::list_by_endpoint(&pool, ep_b)
        .await
        .unwrap();

    assert_eq!(rows_a.len(), 1, "ep_a should have exactly 1 row");
    assert_eq!(rows_a[0].endpoint_id, ep_a);
    assert_eq!(rows_b.len(), 1, "ep_b should have exactly 1 row");
    assert_eq!(rows_b[0].endpoint_id, ep_b);
}

/// `list_by_endpoint` returns all rows when multiple overrides exist on one endpoint.
#[tokio::test]
async fn aip_override_list_multiple_rows() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-multi").await;

    for (model, arn_suffix) in [
        ("claude-sonnet-4-5", "sonnet"),
        ("claude-opus-4-7", "opus"),
        ("claude-haiku-4-5", "haiku"),
    ] {
        endpoint_aip_overrides::insert(
            &pool,
            ep_id,
            model,
            &format!(
                "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/{arn_suffix}"
            ),
            "admin",
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("insert {model} failed: {e}"));
    }

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "should have 3 override rows");

    let model_ids: Vec<&str> = rows.iter().map(|r| r.model_id.as_str()).collect();
    assert!(model_ids.contains(&"claude-sonnet-4-5"));
    assert!(model_ids.contains(&"claude-opus-4-7"));
    assert!(model_ids.contains(&"claude-haiku-4-5"));
}

/// `delete_by_model` removes exactly the targeted row; sibling rows on the same
/// endpoint survive.
#[tokio::test]
async fn aip_override_delete_by_model_removes_one_row() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-del").await;

    for (model, suffix) in [("claude-sonnet-4-5", "sonnet"), ("claude-opus-4-7", "opus")] {
        endpoint_aip_overrides::insert(
            &pool,
            ep_id,
            model,
            &format!(
                "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/{suffix}"
            ),
            "admin",
            None,
        )
        .await
        .unwrap();
    }

    // Delete only the sonnet override
    endpoint_aip_overrides::delete_by_model(&pool, ep_id, "claude-sonnet-4-5")
        .await
        .expect("delete_by_model should succeed");

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "only one row should remain after deletion");
    assert_eq!(
        rows[0].model_id, "claude-opus-4-7",
        "the surviving row must be opus, not sonnet"
    );
}

/// `delete_by_model` on a non-existent `(endpoint_id, model_id)` completes
/// without error (idempotent delete; 0 rows affected is not an error at the DB
/// layer — the admin handler will return 404, but the CRUD function itself is Ok).
#[tokio::test]
async fn aip_override_delete_nonexistent_is_ok() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-del-missing").await;

    // No overrides have been inserted; delete of a non-existent row must not error.
    let result = endpoint_aip_overrides::delete_by_model(&pool, ep_id, "claude-sonnet-4-5").await;

    assert!(
        result.is_ok(),
        "delete_by_model on a missing row should return Ok, not an error"
    );
}

/// FK CASCADE: deleting an endpoint via `delete_endpoint` removes all of its
/// `endpoint_aip_overrides` rows automatically (ON DELETE CASCADE).
#[tokio::test]
async fn aip_override_fk_cascade_on_endpoint_delete() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-cascade").await;

    // Insert two override rows on the endpoint
    for (model, suffix) in [("claude-sonnet-4-5", "sonnet"), ("claude-opus-4-7", "opus")] {
        endpoint_aip_overrides::insert(
            &pool,
            ep_id,
            model,
            &format!(
                "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/{suffix}"
            ),
            "admin",
            None,
        )
        .await
        .unwrap();
    }

    // Confirm rows exist before deletion
    let before = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert_eq!(
        before.len(),
        2,
        "should have 2 rows before endpoint deletion"
    );

    // Delete the endpoint itself
    db::endpoints::delete_endpoint(&pool, ep_id)
        .await
        .expect("delete_endpoint should succeed");

    // Verify the override rows were cascade-deleted
    let after = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert!(
        after.is_empty(),
        "all endpoint_aip_overrides rows must be removed by FK CASCADE when the endpoint is deleted"
    );
}

/// PK violation: inserting two rows with the same `(endpoint_id, model_id)` must
/// return a sqlx database error with Postgres error code 23505.
///
/// This is the signal the admin handler will downcast to return HTTP 409 Conflict.
#[tokio::test]
async fn aip_override_pk_violation_on_duplicate() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-pk-dup").await;

    endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-5",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/first",
        "admin",
        None,
    )
    .await
    .expect("first insert should succeed");

    // Second insert with the same (endpoint_id, model_id) must fail
    let err = endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-5",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/second",
        "admin",
        None,
    )
    .await
    .expect_err("second insert with same (endpoint_id, model_id) must error");

    // The error must be a Postgres unique-constraint violation (code 23505).
    // This is the code the admin handler checks before returning 409 Conflict.
    let db_err = err
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| {
            if let sqlx::Error::Database(dbe) = e {
                Some(dbe)
            } else {
                None
            }
        })
        .expect("error must be a sqlx::Error::Database wrapping a PG error");

    assert_eq!(
        db_err.code().as_deref(),
        Some("23505"),
        "PK violation must produce Postgres error code 23505 (unique_violation)"
    );
}

/// `list_by_endpoint` on an endpoint with no overrides returns an empty vec
/// (not an error).
#[tokio::test]
async fn aip_override_list_empty_for_endpoint_with_no_overrides() {
    let pool = helpers::setup_test_db().await;
    let ep_id = create_test_endpoint(&pool, "ep-empty").await;

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .expect("list_by_endpoint on endpoint with no rows should succeed");

    assert!(
        rows.is_empty(),
        "list_by_endpoint must return empty vec for an endpoint with no overrides"
    );
}
