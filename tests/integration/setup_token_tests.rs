use crate::helpers;
use ccag::db;

// ============================================================
// Setup Token: DB persistence
// ============================================================

/// Create a token and immediately consume it — the raw key should match.
#[tokio::test]
async fn test_setup_token_create_and_consume() {
    let pool = helpers::setup_test_db().await;

    db::setup_tokens::create(&pool, "tok-1", "raw-secret-abc")
        .await
        .expect("create should succeed");

    let result = db::setup_tokens::consume(&pool, "tok-1")
        .await
        .expect("consume should succeed");

    assert_eq!(
        result.as_deref(),
        Some("raw-secret-abc"),
        "consumed value should match the raw key that was stored"
    );
}

/// A setup token must be single-use: consuming it twice should return None the second time.
#[tokio::test]
async fn test_setup_token_single_use() {
    let pool = helpers::setup_test_db().await;

    db::setup_tokens::create(&pool, "tok-once", "one-time-secret")
        .await
        .expect("create should succeed");

    let first = db::setup_tokens::consume(&pool, "tok-once")
        .await
        .expect("first consume should succeed");
    assert_eq!(first.as_deref(), Some("one-time-secret"));

    let second = db::setup_tokens::consume(&pool, "tok-once")
        .await
        .expect("second consume should not error");
    assert!(
        second.is_none(),
        "second consume of the same token must return None"
    );
}

/// An expired token (updated_at > 300 seconds ago) should not be consumable.
#[tokio::test]
async fn test_setup_token_expired() {
    let pool = helpers::setup_test_db().await;

    db::setup_tokens::create(&pool, "tok-exp", "expired-secret")
        .await
        .expect("create should succeed");

    // Manually backdate updated_at by 6 minutes (360s > 300s TTL)
    sqlx::query(
        "UPDATE proxy_settings SET updated_at = now() - INTERVAL '6 minutes' \
         WHERE key = 'setup_token:tok-exp'",
    )
    .execute(&pool)
    .await
    .expect("backdating updated_at should succeed");

    let result = db::setup_tokens::consume(&pool, "tok-exp")
        .await
        .expect("consume should not error");
    assert!(
        result.is_none(),
        "expired token (>300s old) should return None on consume"
    );
}

/// cleanup_expired should delete only tokens older than 300 seconds, leaving fresh ones intact.
#[tokio::test]
async fn test_setup_token_cleanup() {
    let pool = helpers::setup_test_db().await;

    // Create two tokens
    db::setup_tokens::create(&pool, "tok-fresh", "fresh-secret")
        .await
        .expect("create fresh token");
    db::setup_tokens::create(&pool, "tok-stale", "stale-secret")
        .await
        .expect("create stale token");

    // Expire only the stale one
    sqlx::query(
        "UPDATE proxy_settings SET updated_at = now() - INTERVAL '6 minutes' \
         WHERE key = 'setup_token:tok-stale'",
    )
    .execute(&pool)
    .await
    .expect("backdating stale token should succeed");

    // Run cleanup
    let deleted = db::setup_tokens::cleanup_expired(&pool)
        .await
        .expect("cleanup_expired should succeed");
    assert_eq!(deleted, 1, "exactly one expired token should be deleted");

    // The stale token should be gone
    let stale = db::setup_tokens::consume(&pool, "tok-stale")
        .await
        .expect("consume stale should not error");
    assert!(stale.is_none(), "stale token should have been cleaned up");

    // The fresh token should still be consumable
    let fresh = db::setup_tokens::consume(&pool, "tok-fresh")
        .await
        .expect("consume fresh should succeed");
    assert_eq!(
        fresh.as_deref(),
        Some("fresh-secret"),
        "fresh token should survive cleanup"
    );
}
