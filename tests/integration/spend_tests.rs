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

// =============================================================================
// Task 3 (Slice 2 close-out): end-to-end anti-wedge proof against real Postgres.
//
// These tests exercise the full pipeline (Slice 1 sanitization in record() +
// Slice 2 per-record fallback in flush_with_db) against a real Postgres
// instance. The unit tests in src/spend/mod.rs already prove the per-record
// drop logic with mocked DB outcomes; the value of these tests is proving the
// realistic flow + that the loop survives.
// =============================================================================

use ccag::db::spend::{PoolSpendDb, RequestLogEntry, SpendDb};

// -----------------------------------------------------------------------------
// 1. Primary anti-wedge proof: NUL-bearing tool_errors flows through record()
//    (sanitization), reaches Postgres, persists. Subsequent records still
//    flush. The end-to-end Slice 1 contract.
// -----------------------------------------------------------------------------

#[tokio::test]
async fn spend_flush_with_poison_tool_errors_persists_sanitized() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = SpendTracker::new(db_pool, Arc::new(metrics));

    // Build a poison entry with NUL bytes inside a JSONB payload — the exact
    // shape that produces Postgres SQLSTATE 22P05 (untranslatable character)
    // in production when written un-sanitized to JSONB columns.
    let poison_request_id = "req-poison-tool-errors-1";
    let mut poison =
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("poison-user@test.com"));
    poison.request_id = poison_request_id.to_string();
    poison.tool_errors = Some(serde_json::json!({
        "err": "bad\u{0000}value",
        "nested": {"key\u{0000}": "v"}
    }));

    // record() must sanitize the entry at the buffer boundary so the NULs
    // never reach Postgres.
    tracker.record(poison).await;

    // Flush must succeed against real Postgres — sanitization stripped the NULs.
    let flushed = tracker
        .flush()
        .await
        .expect("flush of sanitized poison entry must succeed against real Postgres");
    assert_eq!(flushed, 1, "exactly one record was buffered + flushed");

    // The sanitized record made it to spend_log.
    let row_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row_count.0, 1,
        "sanitized poison record must be persisted to spend_log"
    );

    // Sanitization actually applied: tool_errors stored in the DB contains no
    // NUL byte. We fetch the raw JSONB back as serde_json::Value and serialize
    // it to text, then assert the absence of \0.
    let stored: (Option<serde_json::Value>,) =
        sqlx::query_as("SELECT tool_errors FROM spend_log WHERE request_id = $1")
            .bind(poison_request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let stored_json = stored
        .0
        .expect("tool_errors must be Some after persistence");
    let stored_text = serde_json::to_string(&stored_json).unwrap();
    assert!(
        !stored_text.contains('\0'),
        "persisted tool_errors must contain no NUL byte; got: {stored_text:?}"
    );

    // Anti-wedge: the tracker keeps accepting + flushing NEW records after a
    // poisoned entry. Record + flush a clean entry; row count must increase.
    let mut clean = helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("post-poison-user@test.com"),
    );
    clean.request_id = "req-post-poison-clean-1".to_string();
    tracker.record(clean).await;

    let flushed_after = tracker
        .flush()
        .await
        .expect("subsequent clean flush must succeed (loop did not wedge)");
    assert_eq!(flushed_after, 1);

    let row_count_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row_count_after.0, 2,
        "subsequent clean record must persist — anti-wedge invariant"
    );
}

// -----------------------------------------------------------------------------
// 2. End-to-end Slice 2 proof: the per-record fallback drops a poison record
//    that fails its own insert, the un-poisoned records reach spend_log, and
//    the tracker keeps accepting + flushing afterward.
//
// Because record() sanitizes (Slice 1), real poison cannot survive into the
// buffer, so we cannot get a real Postgres 22P05 from real data — the only
// way to exercise the per-record-drop path end-to-end is to inject the
// data-rejection error via the SpendDb seam. We use a forwarding adapter that
// wraps the real PoolSpendDb but forces a single configured failing
// request_id to fail with SQLSTATE 22P05 on both insert_batch and insert_one.
// -----------------------------------------------------------------------------

/// Test-only `sqlx::error::DatabaseError` carrying an arbitrary SQLSTATE.
/// Mirrors the unit-test pattern in `src/spend/mod.rs` because that type is
/// `#[cfg(test)]`-only and not visible from the integration test crate.
#[derive(Debug)]
struct InjectedDbError {
    code: String,
    message: String,
}

impl InjectedDbError {
    fn new(code: &str) -> Self {
        Self {
            code: code.to_string(),
            message: format!("injected db error sqlstate {code}"),
        }
    }
}

impl std::fmt::Display for InjectedDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InjectedDbError {}

impl sqlx::error::DatabaseError for InjectedDbError {
    fn message(&self) -> &str {
        &self.message
    }
    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(std::borrow::Cow::Borrowed(&self.code))
    }
    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }
    fn kind(&self) -> sqlx::error::ErrorKind {
        sqlx::error::ErrorKind::Other
    }
}

fn injected_data_error(code: &str) -> sqlx::Error {
    sqlx::Error::Database(Box::new(InjectedDbError::new(code)))
}

/// SpendDb adapter that forwards to a real `PoolSpendDb`, but for a single
/// configured `poison_request_id`:
///   - `insert_batch` fails with SQLSTATE 22P05 if the poison request_id is in
///     the batch (mirrors how Postgres rejects the whole batch when one row is
///     bad).
///   - `insert_one` fails with SQLSTATE 22P05 only when called for the poison
///     request_id; clean records pass through to the real pool.
struct PoisonInjectingSpendDb<'a> {
    inner: PoolSpendDb<'a>,
    poison_request_id: String,
}

#[async_trait::async_trait]
impl<'a> SpendDb for PoisonInjectingSpendDb<'a> {
    async fn insert_batch(&self, entries: &[RequestLogEntry]) -> Result<(), sqlx::Error> {
        if entries
            .iter()
            .any(|e| e.request_id == self.poison_request_id)
        {
            return Err(injected_data_error("22P05"));
        }
        self.inner.insert_batch(entries).await
    }

    async fn insert_one(&self, entry: &RequestLogEntry) -> Result<(), sqlx::Error> {
        if entry.request_id == self.poison_request_id {
            return Err(injected_data_error("22P05"));
        }
        self.inner.insert_one(entry).await
    }
}

#[tokio::test]
async fn spend_flush_quarantines_data_error_via_direct_inject() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = SpendTracker::new(db_pool, Arc::new(metrics));

    // Three entries: req-clean-a, req-poison-injected, req-clean-b.
    let mut e1 =
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("inject-user@test.com"));
    e1.request_id = "req-clean-a".to_string();
    let mut e2 =
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("inject-user@test.com"));
    e2.request_id = "req-poison-injected".to_string();
    let mut e3 =
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("inject-user@test.com"));
    e3.request_id = "req-clean-b".to_string();

    tracker.record(e1).await;
    tracker.record(e2).await;
    tracker.record(e3).await;

    // Flush via the SpendDb seam with the poison-injecting adapter wrapping
    // the real pool. The adapter makes the batch insert fail, then makes the
    // poison record fail its individual insert too, while letting the clean
    // records hit the real Postgres.
    let inject_db = PoisonInjectingSpendDb {
        inner: PoolSpendDb { pool: &pool },
        poison_request_id: "req-poison-injected".to_string(),
    };

    let result = tracker.flush_with_db(&inject_db).await;
    // Per the spec, per_record_fallback returns Ok(succeeded). Two records
    // succeeded individually.
    assert!(
        result.is_ok(),
        "flush_with_db must return Ok after a per-record fallback that drops one record; got {result:?}"
    );
    assert_eq!(
        result.unwrap(),
        2,
        "two clean records must have inserted individually"
    );

    // The two clean records reached spend_log. The poisoned one did NOT.
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT request_id FROM spend_log ORDER BY request_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let request_ids: Vec<String> = rows.into_iter().map(|r| r.0).collect();
    assert_eq!(
        request_ids,
        vec!["req-clean-a".to_string(), "req-clean-b".to_string()],
        "only the clean records may persist; poison must be quarantined"
    );

    // Anti-wedge invariant: buffer is empty (poison dropped, no transient
    // records to re-buffer).
    let buf_len: i64 = {
        // SpendTracker has no public buffer_len() outside cfg(test); we infer
        // emptiness by flushing again on an empty buffer.
        let again = tracker
            .flush()
            .await
            .expect("flush on empty buffer must succeed");
        again as i64
    };
    assert_eq!(
        buf_len, 0,
        "buffer must be empty after data-error flush — anti-wedge invariant"
    );

    // Subsequent record() + flush() succeeds — the loop did not wedge.
    let mut e4 = helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("post-inject-user@test.com"),
    );
    e4.request_id = "req-after-inject".to_string();
    tracker.record(e4).await;
    let flushed_after = tracker
        .flush()
        .await
        .expect("subsequent flush must succeed (anti-wedge)");
    assert_eq!(flushed_after, 1);

    let final_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        final_count.0, 3,
        "two clean records from the quarantine flush + one post-quarantine record must be persisted"
    );
}

// -----------------------------------------------------------------------------
// 3. Transient-then-recovery: the loop re-buffers on a transient failure and
//    persists everything when the next flush succeeds.
//
// We model the transient by injecting a one-shot transient SpendDb adapter,
// then calling the real pool-backed flush() to recover. This is cleaner than
// swapping pool URLs at runtime.
// -----------------------------------------------------------------------------

struct OneShotTransientSpendDb {
    fired: tokio::sync::Mutex<bool>,
}

#[async_trait::async_trait]
impl SpendDb for OneShotTransientSpendDb {
    async fn insert_batch(&self, _entries: &[RequestLogEntry]) -> Result<(), sqlx::Error> {
        let mut fired = self.fired.lock().await;
        if !*fired {
            *fired = true;
            return Err(sqlx::Error::PoolTimedOut);
        }
        Ok(())
    }

    async fn insert_one(&self, _entry: &RequestLogEntry) -> Result<(), sqlx::Error> {
        // Should never be called because the transient batch error
        // re-buffers without entering the per-record path.
        Err(sqlx::Error::PoolTimedOut)
    }
}

#[tokio::test]
async fn spend_flush_loop_survives_transient_then_recovers() {
    let pool = helpers::setup_test_db().await;
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let (metrics, _provider) = Metrics::new(None).unwrap();
    let tracker = SpendTracker::new(db_pool, Arc::new(metrics));

    for i in 0..3 {
        let mut e =
            helpers::make_spend_entry("claude-sonnet-4-20250514", Some("transient-user@test.com"));
        e.request_id = format!("req-transient-{i}");
        tracker.record(e).await;
    }

    // First flush: transient injector fires PoolTimedOut. All entries must be
    // re-buffered, no quarantine.
    let transient_db = OneShotTransientSpendDb {
        fired: tokio::sync::Mutex::new(false),
    };
    let result = tracker.flush_with_db(&transient_db).await;
    assert!(
        result.is_err(),
        "transient batch failure must surface as Err to the loop"
    );

    // No rows should have hit Postgres yet.
    let row_count_after_transient: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row_count_after_transient.0, 0,
        "no rows must be persisted while DB is transiently unavailable"
    );

    // Second flush: real pool, all 3 re-buffered records persist.
    let flushed = tracker
        .flush()
        .await
        .expect("recovery flush must succeed against real Postgres");
    assert_eq!(
        flushed, 3,
        "all 3 re-buffered records must be flushed on recovery"
    );

    let row_count_after_recovery: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row_count_after_recovery.0, 3,
        "recovery flush must persist all re-buffered records"
    );
}
