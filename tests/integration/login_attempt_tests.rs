use crate::helpers;
use ccag::db;

// ============================================================
// Login Attempts: DB-backed brute-force tracking
// ============================================================

/// Record 3 attempts, then count_recent should return 3.
#[tokio::test]
async fn test_record_and_count() {
    let pool = helpers::setup_test_db().await;

    for _ in 0..3 {
        db::login_attempts::record_attempt(&pool)
            .await
            .expect("record_attempt should succeed");
    }

    let count = db::login_attempts::count_recent(&pool, 60.0)
        .await
        .expect("count_recent should succeed");

    assert_eq!(count, 3, "should count all 3 recent attempts");
}

/// Record 3 attempts, backdate 2 of them beyond the window,
/// count_recent(60.0) should return only 1.
#[tokio::test]
async fn test_count_only_recent() {
    let pool = helpers::setup_test_db().await;

    for _ in 0..3 {
        db::login_attempts::record_attempt(&pool)
            .await
            .expect("record_attempt should succeed");
    }

    // Backdate the 2 oldest attempts to 120 seconds ago
    sqlx::query(
        "UPDATE login_attempts SET attempted_at = now() - INTERVAL '120 seconds' \
         WHERE id IN (SELECT id FROM login_attempts ORDER BY id ASC LIMIT 2)",
    )
    .execute(&pool)
    .await
    .expect("backdating attempts should succeed");

    let count = db::login_attempts::count_recent(&pool, 60.0)
        .await
        .expect("count_recent should succeed");

    assert_eq!(
        count, 1,
        "only 1 attempt should be within the 60-second window"
    );
}

/// Call check_and_record with limit=10 nine times — all should return true.
#[tokio::test]
async fn test_check_and_record_under_limit() {
    let pool = helpers::setup_test_db().await;

    for i in 0..9 {
        let allowed = db::login_attempts::check_and_record(&pool, 10, 60.0)
            .await
            .expect("check_and_record should succeed");

        assert!(allowed, "attempt {i} should be allowed (under limit of 10)");
    }
}

/// Call check_and_record with limit=10 ten times (all allowed),
/// then the 11th should return false (at limit).
#[tokio::test]
async fn test_check_and_record_at_limit() {
    let pool = helpers::setup_test_db().await;

    // First 10 attempts should all be allowed
    for i in 0..10 {
        let allowed = db::login_attempts::check_and_record(&pool, 10, 60.0)
            .await
            .expect("check_and_record should succeed");

        assert!(
            allowed,
            "attempt {i} should be allowed (at or under limit of 10)"
        );
    }

    // 11th attempt should be rejected
    let allowed = db::login_attempts::check_and_record(&pool, 10, 60.0)
        .await
        .expect("check_and_record should succeed");

    assert!(
        !allowed,
        "11th attempt should be rejected (limit of 10 reached)"
    );
}

/// Record 3 attempts, backdate 2 beyond the window, cleanup should
/// remove exactly 2, and count_recent should return 1.
#[tokio::test]
async fn test_cleanup_removes_old() {
    let pool = helpers::setup_test_db().await;

    for _ in 0..3 {
        db::login_attempts::record_attempt(&pool)
            .await
            .expect("record_attempt should succeed");
    }

    // Backdate the 2 oldest attempts to 120 seconds ago
    sqlx::query(
        "UPDATE login_attempts SET attempted_at = now() - INTERVAL '120 seconds' \
         WHERE id IN (SELECT id FROM login_attempts ORDER BY id ASC LIMIT 2)",
    )
    .execute(&pool)
    .await
    .expect("backdating attempts should succeed");

    let deleted = db::login_attempts::cleanup(&pool, 60.0)
        .await
        .expect("cleanup should succeed");

    assert_eq!(deleted, 2, "cleanup should delete exactly 2 old attempts");

    let count = db::login_attempts::count_recent(&pool, 60.0)
        .await
        .expect("count_recent should succeed");

    assert_eq!(count, 1, "only 1 attempt should remain after cleanup");
}
