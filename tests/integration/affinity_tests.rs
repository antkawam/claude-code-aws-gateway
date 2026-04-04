use crate::helpers;
use ccag::db;
use uuid::Uuid;

// ============================================================
// User Endpoint Affinity: DB persistence
// ============================================================

/// Upsert an affinity record and retrieve it.
#[tokio::test]
async fn test_upsert_and_get() {
    let pool = helpers::setup_test_db().await;
    let ep_id = Uuid::new_v4();

    db::affinity::upsert(&pool, "user@test.com", ep_id)
        .await
        .expect("upsert should succeed");

    let result = db::affinity::get(&pool, "user@test.com")
        .await
        .expect("get should succeed");
    assert_eq!(
        result,
        Some(ep_id),
        "should return the upserted endpoint_id"
    );
}

/// Upserting the same user with a different endpoint overwrites the previous one.
#[tokio::test]
async fn test_update_overwrites() {
    let pool = helpers::setup_test_db().await;
    let ep1 = Uuid::new_v4();
    let ep2 = Uuid::new_v4();

    db::affinity::upsert(&pool, "user@test.com", ep1)
        .await
        .expect("first upsert");
    db::affinity::upsert(&pool, "user@test.com", ep2)
        .await
        .expect("second upsert");

    let result = db::affinity::get(&pool, "user@test.com")
        .await
        .expect("get should succeed");
    assert_eq!(result, Some(ep2), "should return the latest endpoint_id");
}

/// Expired affinity (>1800s old) should not be returned.
#[tokio::test]
async fn test_expired_not_returned() {
    let pool = helpers::setup_test_db().await;
    let ep_id = Uuid::new_v4();

    db::affinity::upsert(&pool, "user@test.com", ep_id)
        .await
        .expect("upsert");

    // Backdate to 31 minutes ago (1860s > 1800s TTL)
    sqlx::query(
        "UPDATE user_endpoint_affinity SET last_used_at = now() - INTERVAL '31 minutes' \
         WHERE user_identity = 'user@test.com'",
    )
    .execute(&pool)
    .await
    .expect("backdating should succeed");

    let result = db::affinity::get(&pool, "user@test.com")
        .await
        .expect("get should succeed");
    assert!(result.is_none(), "expired affinity should not be returned");
}

/// cleanup_stale removes only entries older than 1800s.
#[tokio::test]
async fn test_cleanup_stale() {
    let pool = helpers::setup_test_db().await;
    let ep_id = Uuid::new_v4();

    db::affinity::upsert(&pool, "fresh@test.com", ep_id)
        .await
        .expect("upsert fresh");
    db::affinity::upsert(&pool, "stale@test.com", ep_id)
        .await
        .expect("upsert stale");

    // Expire only the stale one
    sqlx::query(
        "UPDATE user_endpoint_affinity SET last_used_at = now() - INTERVAL '31 minutes' \
         WHERE user_identity = 'stale@test.com'",
    )
    .execute(&pool)
    .await
    .expect("backdating stale should succeed");

    let deleted = db::affinity::cleanup_stale(&pool)
        .await
        .expect("cleanup should succeed");
    assert_eq!(deleted, 1, "exactly one stale entry should be deleted");

    let stale = db::affinity::get(&pool, "stale@test.com")
        .await
        .expect("get stale");
    assert!(stale.is_none(), "stale entry should be gone after cleanup");

    let fresh = db::affinity::get(&pool, "fresh@test.com")
        .await
        .expect("get fresh");
    assert_eq!(fresh, Some(ep_id), "fresh entry should survive cleanup");
}

/// Upserting the same user with the same endpoint is idempotent.
#[tokio::test]
async fn test_upsert_idempotent() {
    let pool = helpers::setup_test_db().await;
    let ep_id = Uuid::new_v4();

    db::affinity::upsert(&pool, "user@test.com", ep_id)
        .await
        .expect("first upsert");
    db::affinity::upsert(&pool, "user@test.com", ep_id)
        .await
        .expect("second upsert should not error");

    let result = db::affinity::get(&pool, "user@test.com")
        .await
        .expect("get");
    assert_eq!(result, Some(ep_id));
}
