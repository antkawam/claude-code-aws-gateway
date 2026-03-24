use std::sync::Arc;

use ccag::db;
use ccag::spend::SpendTracker;
use ccag::telemetry::Metrics;

use crate::helpers;

// ============================================================
// SpendTracker: flush persists to database
// ============================================================

#[tokio::test]
async fn spend_flush_persists_to_db() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = SpendTracker::new(db_pool, Arc::new(metrics));

    // Record 3 entries
    for i in 0..3 {
        let mut entry =
            helpers::make_spend_entry("claude-sonnet-4-20250514", Some("flush-user@test.com"));
        entry.request_id = format!("req-flush-{i}");
        tracker.record(entry).await;
    }

    // Flush to real database
    let count = tracker.flush().await.unwrap();
    assert_eq!(count, 3);

    // Verify rows are in the database
    let row_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count.0, 3);
}

// ============================================================
// SpendTracker: flush empty buffer is noop
// ============================================================

#[tokio::test]
async fn spend_flush_empty_is_noop() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = SpendTracker::new(db_pool, Arc::new(metrics));

    let count = tracker.flush().await.unwrap();
    assert_eq!(count, 0);

    let row_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count.0, 0);
}

// ============================================================
// SpendTracker: flushed cost is queryable via budget functions
// ============================================================

#[tokio::test]
async fn spend_recorded_cost_is_queryable() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = SpendTracker::new(db_pool, Arc::new(metrics));

    let entry = helpers::make_spend_entry("claude-sonnet-4-20250514", Some("cost-user@test.com"));
    tracker.record(entry).await;
    tracker.flush().await.unwrap();

    // The spend should be queryable via the budget spend function
    let spend = db::spend::get_user_monthly_spend_usd(&pool, "cost-user@test.com")
        .await
        .unwrap();
    assert!(
        spend > 0.0,
        "Flushed spend should be queryable: got {spend}"
    );
}

// ============================================================
// SpendTracker: concurrent record and flush
// ============================================================

#[tokio::test]
async fn spend_concurrent_record_and_flush() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = Arc::new(SpendTracker::new(db_pool, Arc::new(metrics)));

    let total_entries = 50;
    let mut handles = vec![];

    // Spawn tasks that record entries concurrently
    for i in 0..total_entries {
        let t = Arc::clone(&tracker);
        handles.push(tokio::spawn(async move {
            let mut entry = helpers::make_spend_entry(
                "claude-sonnet-4-20250514",
                Some("concurrent-user@test.com"),
            );
            entry.request_id = format!("req-concurrent-{i}");
            t.record(entry).await;
        }));
    }

    // Wait for all records to complete
    for h in handles {
        h.await.unwrap();
    }

    // Flush all at once
    let flushed = tracker.flush().await.unwrap();
    assert_eq!(flushed, total_entries);

    // Verify all entries are in the database
    let row_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count.0, total_entries as i64);
}

// ============================================================
// SpendTracker: large batch insert
// ============================================================

#[tokio::test]
async fn spend_batch_insert_large() {
    let pool = helpers::setup_test_db().await;

    // Insert 500 entries directly via insert_batch
    let entries: Vec<_> = (0..500)
        .map(|i| {
            let mut e =
                helpers::make_spend_entry("claude-sonnet-4-20250514", Some("batch-user@test.com"));
            e.request_id = format!("req-batch-{i}");
            e
        })
        .collect();

    db::spend::insert_batch(&pool, &entries).await.unwrap();

    let row_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count.0, 500);
}

// ============================================================
// SpendTracker: multiple flushes accumulate
// ============================================================

#[tokio::test]
async fn spend_multiple_flushes_accumulate() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = SpendTracker::new(db_pool, Arc::new(metrics));

    // First batch
    for i in 0..3 {
        let mut entry =
            helpers::make_spend_entry("claude-sonnet-4-20250514", Some("multi-user@test.com"));
        entry.request_id = format!("req-multi-a-{i}");
        tracker.record(entry).await;
    }
    tracker.flush().await.unwrap();

    // Second batch
    for i in 0..2 {
        let mut entry =
            helpers::make_spend_entry("claude-haiku-4-5-20251001", Some("multi-user@test.com"));
        entry.request_id = format!("req-multi-b-{i}");
        tracker.record(entry).await;
    }
    tracker.flush().await.unwrap();

    // Total should be 5
    let row_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count.0, 5);
}
