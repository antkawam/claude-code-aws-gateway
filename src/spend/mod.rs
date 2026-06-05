use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::spend::{PoolSpendDb, RequestLogEntry, SpendDb, is_transient_db_error};
use crate::telemetry::Metrics;

pub(crate) mod sanitize;
use sanitize::{sanitize_json, sanitize_string};

/// Max buffered entries before dropping oldest. Prevents unbounded memory growth
/// if the database is unreachable.
const MAX_BUFFER_SIZE: usize = 10_000;

/// Sanitize a `String` in place. No-op (no allocation) when the string is clean.
fn sanitize_in_place(s: &mut String) {
    if let std::borrow::Cow::Owned(clean) = sanitize_string(s) {
        *s = clean;
    }
}

/// Sanitize an `Option<String>` in place. No-op when `None` or the value is clean.
fn sanitize_in_place_opt(opt: &mut Option<String>) {
    if let Some(s) = opt.as_mut() {
        sanitize_in_place(s);
    }
}

/// Extract the SQLSTATE code from a `sqlx::Error` for structured logging.
/// Returns `None` for non-database error variants or when no code is set.
fn sqlstate(err: &sqlx::Error) -> Option<String> {
    match err {
        sqlx::Error::Database(db_err) => db_err.code().map(|c| c.into_owned()),
        _ => None,
    }
}

/// Async batch spend tracker. Buffers request log entries in memory and
/// flushes to Postgres on interval.
pub struct SpendTracker {
    buffer: Arc<Mutex<Vec<RequestLogEntry>>>,
    db_pool: Arc<tokio::sync::RwLock<sqlx::PgPool>>,
    metrics: Arc<Metrics>,
}

impl SpendTracker {
    pub fn new(db_pool: Arc<tokio::sync::RwLock<sqlx::PgPool>>, metrics: Arc<Metrics>) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
            db_pool,
            metrics,
        }
    }

    /// Record a request log entry (buffered, not written immediately).
    ///
    /// Sanitizes all client-controlled string fields before buffering so that
    /// NUL characters (\0) and related Postgres-poisoning values are never written
    /// to the spend buffer.
    pub async fn record(&self, mut entry: RequestLogEntry) {
        // Sanitize all client/model-controlled string fields before buffering.
        // Only assigns back when sanitize_string returns Owned (i.e. something changed).
        sanitize_in_place_opt(&mut entry.user_identity);
        sanitize_in_place(&mut entry.model);
        sanitize_in_place_opt(&mut entry.stop_reason);
        sanitize_in_place_opt(&mut entry.session_id);
        sanitize_in_place_opt(&mut entry.project_key);

        for s in &mut entry.tool_names {
            sanitize_in_place(s);
        }
        for s in &mut entry.content_block_types {
            sanitize_in_place(s);
        }

        if let Some(ref mut v) = entry.tool_errors {
            sanitize_json(v);
        }
        if let Some(ref mut v) = entry.detection_flags {
            sanitize_json(v);
        }

        let mut buf = self.buffer.lock().await;
        if buf.len() >= MAX_BUFFER_SIZE {
            let to_drop = buf.len() - MAX_BUFFER_SIZE + 1;
            tracing::warn!(
                dropped = to_drop,
                "Spend buffer at capacity, dropping oldest entries"
            );
            buf.drain(..to_drop);
        }
        buf.push(entry);
    }

    /// Flush buffered entries to the database within a transaction.
    ///
    /// Thin wrapper that constructs a pool-backed `SpendDb` adapter and
    /// delegates to `flush_with_db` (the testable seam).
    pub async fn flush(&self) -> anyhow::Result<usize> {
        let pool = self.db_pool.read().await.clone();
        let db = PoolSpendDb { pool: &pool };
        self.flush_with_db(&db).await
    }

    /// Flush buffered entries through an injected `SpendDb` implementation.
    ///
    /// Behaviour:
    /// 1. Drain the buffer and call `db.insert_batch`.
    /// 2. On batch success → done.
    /// 3. On batch failure, classify with `is_transient_db_error`:
    ///    - Transient → re-buffer ALL records (capacity-respecting prepend),
    ///      record `spend_flush_errors`, return `Err`.
    ///    - Data rejection → enter per-record fallback.
    /// 4. Per-record fallback: call `db.insert_one` for each record in order.
    ///    - Success → record flushed (gone from buffer).
    ///    - Failure with data-rejection error → drop the record, increment the
    ///      quarantine counter, log at `warn` (request_id + SQLSTATE only —
    ///      NEVER the full payload).
    ///    - Failure with transient error → stop the pass; re-buffer the
    ///      failing record + all un-attempted remaining records.
    pub async fn flush_with_db(&self, db: &dyn SpendDb) -> anyhow::Result<usize> {
        let entries = {
            let mut buf = self.buffer.lock().await;
            std::mem::take(&mut *buf)
        };

        let count = entries.len();
        if count == 0 {
            return Ok(0);
        }

        match db.insert_batch(&entries).await {
            Ok(()) => {
                tracing::debug!(count, "Flushed request log entries to database");
                Ok(count)
            }
            Err(batch_err) => {
                if is_transient_db_error(&batch_err) {
                    // Transient — re-buffer everything for retry.
                    tracing::warn!(
                        error = %batch_err,
                        count,
                        "Spend flush failed (transient), re-buffering entries"
                    );
                    self.metrics.record_spend_flush_error();
                    self.rebuffer_capacity_respecting(entries).await;
                    Err(anyhow::Error::from(batch_err))
                } else {
                    // Data rejection — at least one record is poison. Run a
                    // per-record pass to isolate it.
                    tracing::warn!(
                        error = %batch_err,
                        count,
                        sqlstate = ?sqlstate(&batch_err),
                        "Spend batch insert failed with data-rejection error; entering per-record fallback"
                    );
                    self.metrics.record_spend_flush_error();
                    self.per_record_fallback(db, entries).await
                }
            }
        }
    }

    /// Per-record fallback after a batch failure with a data-rejection error.
    ///
    /// Inserts each entry individually:
    /// - Success → consumed.
    /// - Data-rejection failure → dropped + quarantine counter += 1.
    /// - Transient failure → stop the pass; re-buffer the failing record AND
    ///   all un-attempted remaining records.
    ///
    /// Returns the number of records successfully inserted in this pass.
    async fn per_record_fallback(
        &self,
        db: &dyn SpendDb,
        entries: Vec<RequestLogEntry>,
    ) -> anyhow::Result<usize> {
        let mut iter = entries.into_iter();
        let mut succeeded: usize = 0;
        let mut quarantined: u64 = 0;
        let mut to_rebuffer: Vec<RequestLogEntry> = Vec::new();

        while let Some(entry) = iter.next() {
            match db.insert_one(&entry).await {
                Ok(()) => {
                    succeeded += 1;
                }
                Err(e) if is_transient_db_error(&e) => {
                    // Transient — DB is going down. Re-buffer the failing
                    // record and all un-attempted remaining records and stop
                    // hammering the DB record-by-record.
                    tracing::warn!(
                        error = %e,
                        request_id = %entry.request_id,
                        "Per-record fallback hit transient error; aborting pass and re-buffering remaining records"
                    );
                    to_rebuffer.push(entry);
                    to_rebuffer.extend(iter);
                    break;
                }
                Err(e) => {
                    // Data rejection on this individual record — drop it.
                    // NEVER log the full payload (could re-emit poison/PII):
                    // log only request_id + SQLSTATE.
                    tracing::warn!(
                        request_id = %entry.request_id,
                        sqlstate = ?sqlstate(&e),
                        "Quarantining spend record after data-rejection on individual insert"
                    );
                    quarantined += 1;
                }
            }
        }

        if quarantined > 0 {
            self.metrics.record_spend_records_quarantined(quarantined);
        }

        if !to_rebuffer.is_empty() {
            self.rebuffer_capacity_respecting(to_rebuffer).await;
        }

        Ok(succeeded)
    }

    /// Re-buffer entries on the front of the buffer, respecting MAX_BUFFER_SIZE.
    /// Records that don't fit are dropped (oldest first when over capacity).
    async fn rebuffer_capacity_respecting(&self, entries: Vec<RequestLogEntry>) {
        let mut buf = self.buffer.lock().await;
        let available = MAX_BUFFER_SIZE.saturating_sub(buf.len());
        let to_restore = entries.into_iter().take(available);
        let existing = std::mem::take(&mut *buf);
        *buf = to_restore.chain(existing).collect();
    }

    /// Start the background flush loop. Returns a JoinHandle.
    pub fn start_flush_loop(self: &Arc<Self>, interval_secs: u64) -> tokio::task::JoinHandle<()> {
        let tracker = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                if let Err(e) = tracker.flush().await {
                    tracing::warn!(%e, "Spend flush loop iteration failed");
                }
            }
        })
    }

    /// Get current buffer length (for testing).
    #[cfg(test)]
    async fn buffer_len(&self) -> usize {
        self.buffer.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::spend::RequestLogEntry;
    use crate::spend::sanitize::{sanitize_json, sanitize_string};
    use crate::telemetry::Metrics;
    use std::borrow::Cow;

    fn make_entry(request_id: &str) -> RequestLogEntry {
        RequestLogEntry {
            key_id: None,
            user_identity: Some("test-user".to_string()),
            request_id: request_id.to_string(),
            model: "claude-sonnet-4-6".to_string(),
            streaming: true,
            duration_ms: 100,
            input_tokens: 500,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            stop_reason: Some("end_turn".to_string()),
            tool_count: 0,
            tool_names: vec![],
            turn_count: 1,
            thinking_enabled: false,
            has_system_prompt: true,
            session_id: None,
            project_key: None,
            tool_errors: None,
            has_correction: false,
            content_block_types: vec![],
            system_prompt_hash: None,
            detection_flags: None,
            endpoint_id: None,
        }
    }

    fn make_tracker() -> SpendTracker {
        // Create a pool that won't be used (record() doesn't touch the pool).
        // PgPool::connect_lazy won't actually try to connect until a query runs.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://unused:unused@localhost:5432/unused")
            .unwrap();

        let (metrics, _provider) = Metrics::new(None).unwrap();
        SpendTracker::new(Arc::new(tokio::sync::RwLock::new(pool)), Arc::new(metrics))
    }

    #[tokio::test]
    async fn record_buffers_entry() {
        let tracker = make_tracker();
        assert_eq!(tracker.buffer_len().await, 0);

        tracker.record(make_entry("req-1")).await;
        assert_eq!(tracker.buffer_len().await, 1);

        tracker.record(make_entry("req-2")).await;
        assert_eq!(tracker.buffer_len().await, 2);
    }

    #[tokio::test]
    async fn record_enforces_buffer_capacity() {
        let tracker = make_tracker();

        // Fill buffer to MAX_BUFFER_SIZE
        for i in 0..MAX_BUFFER_SIZE {
            tracker.record(make_entry(&format!("req-{i}"))).await;
        }
        assert_eq!(tracker.buffer_len().await, MAX_BUFFER_SIZE);

        // Adding one more should drop the oldest to make room
        tracker.record(make_entry("overflow")).await;
        assert_eq!(tracker.buffer_len().await, MAX_BUFFER_SIZE);

        // The newest entry should be last
        let buf = tracker.buffer.lock().await;
        assert_eq!(buf.last().unwrap().request_id, "overflow");
        // The first entry ("req-0") should have been dropped
        assert_eq!(buf.first().unwrap().request_id, "req-1");
    }

    #[tokio::test]
    async fn flush_drains_buffer() {
        let tracker = make_tracker();
        tracker.record(make_entry("req-1")).await;
        tracker.record(make_entry("req-2")).await;

        // flush() will fail because the pool is not connected,
        // but we can verify the buffer is drained before the DB call
        // and re-buffered on failure.
        let result = tracker.flush().await;
        assert!(result.is_err(), "Should fail with no real DB");

        // On flush failure, entries should be re-buffered
        assert_eq!(tracker.buffer_len().await, 2);
    }

    #[tokio::test]
    async fn flush_empty_buffer_succeeds() {
        let tracker = make_tracker();

        // Flushing empty buffer should succeed with count 0
        let result = tracker.flush().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn flush_failure_rebuffers_respecting_capacity() {
        let tracker = make_tracker();

        // Add some entries
        for i in 0..5 {
            tracker.record(make_entry(&format!("req-{i}"))).await;
        }

        // Flush will fail (no DB), entries get re-buffered
        let _ = tracker.flush().await;
        assert_eq!(tracker.buffer_len().await, 5);

        // Add more entries while failed entries are in buffer
        tracker.record(make_entry("new-1")).await;
        assert_eq!(tracker.buffer_len().await, 6);
    }

    // -------------------------------------------------------------------------
    // Task 1 contract tests: sanitize_string
    // #[cfg(test)] module continues below
    // -------------------------------------------------------------------------

    #[test]
    fn sanitize_string_strips_nul() {
        let result = sanitize_string("a\0b");
        assert_eq!(
            result.as_ref(),
            "ab",
            "NUL char must be removed from string"
        );
    }

    #[test]
    fn sanitize_string_strips_multiple_nuls() {
        let result = sanitize_string("\0hello\0world\0");
        assert_eq!(result.as_ref(), "helloworld");
    }

    #[test]
    fn sanitize_string_clean_input_returns_borrowed() {
        // Zero-alloc fast path: clean strings must NOT allocate.
        let result = sanitize_string("hello world");
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "Clean input must return Cow::Borrowed (no allocation), got Cow::Owned"
        );
    }

    #[test]
    fn sanitize_string_poison_input_returns_owned() {
        // Only poison input should allocate.
        let result = sanitize_string("has\0nul");
        assert!(
            matches!(result, Cow::Owned(_)),
            "Poison input must return Cow::Owned (new allocation)"
        );
    }

    #[test]
    fn sanitize_string_empty_is_borrowed() {
        let result = sanitize_string("");
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "Empty string must return Cow::Borrowed"
        );
    }

    #[test]
    fn sanitize_string_unicode_emoji_unchanged() {
        // Emoji: multi-byte UTF-8, must pass through unchanged and without allocation.
        let input = "hello \u{1F389} world";
        let result = sanitize_string(input);
        assert_eq!(result.as_ref(), input);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "Emoji input must return Cow::Borrowed (no allocation)"
        );
    }

    #[test]
    fn sanitize_string_unicode_cjk_unchanged() {
        // CJK ideographs: must pass through unchanged.
        let input = "\u{65E5}\u{672C}\u{8A9E}\u{30C6}\u{30B9}\u{30C8}"; // 日本語テスト
        let result = sanitize_string(input);
        assert_eq!(result.as_ref(), input);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "CJK input must return Cow::Borrowed"
        );
    }

    #[test]
    fn sanitize_string_unicode_accents_unchanged() {
        // Precomposed accented characters (e.g. é U+00E9).
        let input = "caf\u{00E9} r\u{00E9}sum\u{00E9} na\u{00EF}ve"; // café résumé naïve
        let result = sanitize_string(input);
        assert_eq!(result.as_ref(), input);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "Accented characters must return Cow::Borrowed"
        );
    }

    #[test]
    fn sanitize_string_unicode_combining_marks_unchanged() {
        // Combining acute accent (U+0301) after 'e' — decomposed form.
        // This is valid UTF-8 and must pass through unchanged.
        let input = "e\u{0301} cafe\u{0301}"; // e + combining acute + space + cafe + combining acute
        let result = sanitize_string(input);
        assert_eq!(result.as_ref(), input);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "Combining marks must return Cow::Borrowed"
        );
    }

    // -------------------------------------------------------------------------
    // Task 1 contract tests: sanitize_json
    // -------------------------------------------------------------------------

    #[test]
    fn sanitize_json_strips_nul_from_top_level_string() {
        let mut v = serde_json::Value::String("hel\0lo".to_string());
        sanitize_json(&mut v);
        assert_eq!(v, serde_json::Value::String("hello".to_string()));
    }

    #[test]
    fn sanitize_json_strips_nul_from_object_values() {
        let mut v = serde_json::json!({"key": "val\0ue", "clean": "ok"});
        sanitize_json(&mut v);
        assert_eq!(v["key"], serde_json::Value::String("value".to_string()));
        assert_eq!(v["clean"], serde_json::Value::String("ok".to_string()));
    }

    #[test]
    fn sanitize_json_strips_nul_from_object_keys() {
        // A key containing NUL must have the NUL removed from the key name.
        let mut v = serde_json::json!({"k\0ey": "v"});
        sanitize_json(&mut v);

        // The poisoned key must be gone.
        assert!(
            v.get("k\0ey").is_none(),
            "Original poisoned key must not exist after sanitization"
        );
        // The sanitized key must be present.
        assert_eq!(
            v.get("key"),
            Some(&serde_json::Value::String("v".to_string())),
            "Sanitized key 'key' must be present with correct value"
        );
    }

    #[test]
    fn sanitize_json_strips_nul_from_array_elements() {
        let mut v = serde_json::json!(["clean", "poi\0son", "also\0bad"]);
        sanitize_json(&mut v);
        assert_eq!(v, serde_json::json!(["clean", "poison", "alsobad"]));
    }

    #[test]
    fn sanitize_json_recurses_into_nested_object_in_array_in_object() {
        // Deeply nested: object -> array -> object -> string with NUL.
        let mut v = serde_json::json!({
            "outer": [
                {"inner_key": "inner\0val", "i\0nner_k": "x"}
            ]
        });
        sanitize_json(&mut v);

        let outer_arr = v["outer"].as_array().expect("outer must be array");
        assert_eq!(outer_arr.len(), 1);
        let inner_obj = &outer_arr[0];

        // The clean key's value must be sanitized.
        assert_eq!(
            inner_obj.get("inner_key"),
            Some(&serde_json::Value::String("innerval".to_string()))
        );
        // The poisoned key must be gone; the sanitized key present.
        assert!(
            inner_obj.get("i\0nner_k").is_none(),
            "Poisoned nested key must be gone"
        );
        assert_eq!(
            inner_obj.get("inner_k"),
            Some(&serde_json::Value::String("x".to_string()))
        );
    }

    #[test]
    fn sanitize_json_leaves_non_string_types_alone() {
        // Numbers, booleans, null must be untouched.
        let mut v = serde_json::json!({
            "num": 42,
            "flag": true,
            "nil": null,
            "arr": [1, 2, 3]
        });
        let expected = v.clone();
        sanitize_json(&mut v);
        assert_eq!(v, expected);
    }

    // -------------------------------------------------------------------------
    // Task 1 contract test: serde_json lone-surrogate empirical probe
    // -------------------------------------------------------------------------

    #[test]
    fn sanitize_json_lone_surrogate_empirical_probe() {
        // Empirically determine what serde_json does with a JSON string containing
        // a lone UTF-16 high surrogate (\uD800, not followed by a low surrogate).
        //
        // serde_json (as of 1.x) REJECTS lone surrogates at parse time by default
        // (returning a parse error) unless the "arbitrary_precision" feature is
        // enabled. With the standard feature set this will be Err(_).
        //
        // If serde_json ever changes this behaviour and returns Ok, we assert the
        // post-condition our pipeline guarantees: after running through sanitize_json
        // the resulting string contains no \0 byte and no lone surrogate code unit
        // (represented as U+FFFD or simply absent).
        //
        // This test documents observed behaviour so future maintainers understand
        // why we don't need special lone-surrogate handling in Rust Strings (which
        // are already guaranteed valid UTF-8 and cannot hold unpaired surrogates).
        let json_literal = "\"\\uD800\"";
        match serde_json::from_str::<serde_json::Value>(json_literal) {
            Err(_parse_err) => {
                // Expected path with standard serde_json: lone surrogates are
                // rejected at parse. Nothing enters the pipeline; no sanitization
                // needed. This is the currently observed behaviour.
            }
            Ok(mut parsed) => {
                // If a future serde_json version accepts lone surrogates (e.g. as
                // U+FFFD replacement), we still sanitize and assert our invariants.
                sanitize_json(&mut parsed);
                let serialized = serde_json::to_string(&parsed).unwrap_or_default();
                // Post-condition 1: no raw NUL byte in the stored value.
                assert!(
                    !serialized.contains('\0'),
                    "After sanitize_json, stored value must contain no NUL byte; got: {serialized:?}"
                );
                // Post-condition 2: no lone surrogate escape in the serialized form.
                // serde_json serializes U+FFFD as the literal character, not \uD800.
                // We just confirm \uD800 is not present verbatim.
                let lower = serialized.to_ascii_lowercase();
                assert!(
                    !lower.contains("\\ud800"),
                    "After sanitize_json, stored value must not contain a lone \\uD800 escape; got: {serialized:?}"
                );
            }
        }
    }

    // -------------------------------------------------------------------------
    // Task 1 contract test: SpendTracker::record() sanitizes poisoned entries
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn record_sanitizes_nul_in_model_and_tool_fields() {
        let tracker = make_tracker();

        // Build a poisoned entry: NUL chars in multiple client-controlled fields.
        let entry = RequestLogEntry {
            model: "claude-3\0-sonnet".to_string(),
            tool_names: vec!["bash\0tool".to_string(), "clean_tool".to_string()],
            tool_errors: Some(serde_json::json!({
                "err": "bad\0input",
                "nested": {"k\0ey": "v\0al"}
            })),
            content_block_types: vec!["text\0block".to_string(), "tool_use".to_string()],
            // Set clean optional fields so we can verify they are not corrupted.
            user_identity: Some("clean-user@example.com".to_string()),
            stop_reason: Some("end_turn".to_string()),
            session_id: Some("sess-abc".to_string()),
            project_key: Some("proj-xyz".to_string()),
            ..make_entry("poison-req-1")
        };

        tracker.record(entry).await;

        // Inspect the buffered entry directly.
        let buf = tracker.buffer.lock().await;
        assert_eq!(buf.len(), 1, "Entry must be buffered");
        let stored = &buf[0];

        // --- model ---
        assert!(
            !stored.model.contains('\0'),
            "model must not contain NUL after record(); got: {:?}",
            stored.model
        );
        assert_eq!(stored.model, "claude-3-sonnet");

        // --- tool_names ---
        for name in &stored.tool_names {
            assert!(
                !name.contains('\0'),
                "tool_names element must not contain NUL; got: {name:?}"
            );
        }
        assert_eq!(stored.tool_names[0], "bashtool");
        assert_eq!(stored.tool_names[1], "clean_tool");

        // --- content_block_types ---
        for cbt in &stored.content_block_types {
            assert!(
                !cbt.contains('\0'),
                "content_block_types element must not contain NUL; got: {cbt:?}"
            );
        }
        assert_eq!(stored.content_block_types[0], "textblock");
        assert_eq!(stored.content_block_types[1], "tool_use");

        // --- tool_errors JSON: recursively check no \0 anywhere ---
        if let Some(tool_errors) = &stored.tool_errors {
            let serialized =
                serde_json::to_string(tool_errors).expect("tool_errors must serialize");
            assert!(
                !serialized.contains('\0'),
                "tool_errors must contain no NUL byte anywhere after record(); serialized: {serialized:?}"
            );
            // The poisoned key "k\0ey" must have been sanitized to "key".
            assert!(
                !serialized.contains("k\0ey"),
                "tool_errors must not contain the original poisoned key k\\0ey"
            );
        } else {
            panic!("tool_errors must be Some after record()");
        }

        // --- Clean fields must be untouched ---
        assert_eq!(
            stored.user_identity.as_deref(),
            Some("clean-user@example.com"),
            "user_identity must not be corrupted"
        );
        assert_eq!(
            stored.stop_reason.as_deref(),
            Some("end_turn"),
            "stop_reason must not be corrupted"
        );
        assert_eq!(
            stored.session_id.as_deref(),
            Some("sess-abc"),
            "session_id must not be corrupted"
        );
        assert_eq!(
            stored.project_key.as_deref(),
            Some("proj-xyz"),
            "project_key must not be corrupted"
        );
    }

    #[tokio::test]
    async fn record_sanitizes_nul_in_optional_string_fields() {
        // Verify user_identity, stop_reason, session_id, project_key are each
        // sanitized independently when they carry NUL.
        let tracker = make_tracker();

        let entry = RequestLogEntry {
            user_identity: Some("user\0name".to_string()),
            stop_reason: Some("end\0turn".to_string()),
            session_id: Some("sess\0id".to_string()),
            project_key: Some("proj\0key".to_string()),
            ..make_entry("poison-req-2")
        };

        tracker.record(entry).await;

        let buf = tracker.buffer.lock().await;
        let stored = &buf[0];

        assert_eq!(stored.user_identity.as_deref(), Some("username"));
        assert_eq!(stored.stop_reason.as_deref(), Some("endturn"));
        assert_eq!(stored.session_id.as_deref(), Some("sessid"));
        assert_eq!(stored.project_key.as_deref(), Some("projkey"));
    }

    #[tokio::test]
    async fn record_clean_entry_passes_through_unchanged() {
        // Verify that a clean entry is not mutated by sanitization.
        let tracker = make_tracker();

        let entry = RequestLogEntry {
            model: "claude-sonnet-4-6".to_string(),
            tool_names: vec!["bash".to_string(), "computer".to_string()],
            content_block_types: vec!["text".to_string(), "tool_use".to_string()],
            tool_errors: Some(serde_json::json!({"err": "timeout", "code": 1})),
            user_identity: Some("alice@example.com".to_string()),
            stop_reason: Some("end_turn".to_string()),
            session_id: Some("sess-clean".to_string()),
            project_key: Some("proj-clean".to_string()),
            ..make_entry("clean-req-1")
        };

        tracker.record(entry).await;

        let buf = tracker.buffer.lock().await;
        let stored = &buf[0];

        assert_eq!(stored.model, "claude-sonnet-4-6");
        assert_eq!(stored.tool_names, vec!["bash", "computer"]);
        assert_eq!(stored.content_block_types, vec!["text", "tool_use"]);
        assert_eq!(stored.user_identity.as_deref(), Some("alice@example.com"));
        assert_eq!(stored.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(stored.session_id.as_deref(), Some("sess-clean"));
        assert_eq!(stored.project_key.as_deref(), Some("proj-clean"));
    }

    // =========================================================================
    // Task 2 contract tests: per-record fallback + error classification +
    // quarantine metric.
    //
    // TEST SEAM (builder must implement):
    //
    // 1. `pub fn is_transient_db_error(err: &sqlx::Error) -> bool` in
    //    `src/db/spend.rs` (or `src/spend/`). Returns true for connection /
    //    pool / IO failures; false for SQLSTATE 22xxx data-rejection errors.
    //    Default for unknown SQLSTATE codes: TRANSIENT (true) — better to
    //    retry than to silently drop spend data on an error we don't
    //    recognise. Documented + asserted below.
    //
    // 2. `pub async fn insert_one(pool: &PgPool, entry: &RequestLogEntry)
    //     -> Result<(), sqlx::Error>` in `src/db/spend.rs`. Inserts a single
    //    record; surfaces the raw `sqlx::Error` so the caller can classify.
    //    (Returning `sqlx::Error` rather than `anyhow::Error` is required
    //    for the classifier to inspect SQLSTATE without downcast gymnastics.)
    //
    // 3. `Metrics::record_spend_records_quarantined(&self, n: u64)` in
    //    `src/telemetry/mod.rs`, backed by counter
    //    `ccag.spend_records_quarantined.total`. Mirrors the
    //    `spend_flush_errors` declaration at lines 20, 98, 191.
    //
    // 4. SpendDb trait + flush_with_db on SpendTracker — the test seam that
    //    lets these tests inject scripted batch / per-record outcomes
    //    without a real Postgres:
    //
    //        #[async_trait::async_trait]
    //        pub trait SpendDb: Send + Sync {
    //            async fn insert_batch(&self, entries: &[RequestLogEntry])
    //                -> Result<(), sqlx::Error>;
    //            async fn insert_one(&self, entry: &RequestLogEntry)
    //                -> Result<(), sqlx::Error>;
    //        }
    //
    //        impl SpendTracker {
    //            pub async fn flush_with_db(&self, db: &dyn SpendDb)
    //                -> anyhow::Result<usize>;
    //        }
    //
    //    The existing `flush()` becomes a thin wrapper that constructs a
    //    pool-backed `SpendDb` adapter and calls `flush_with_db`.
    //
    //    Using a trait (not function pointers) keeps async signatures clean
    //    and matches existing codebase patterns. If the builder prefers a
    //    different shape (e.g. two `Fn` closures), the tests will need a
    //    matching minor edit, but the contract assertions stay identical.
    // =========================================================================

    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- Mock SpendDb -------------------------------------------------------

    /// Outcome script for one call.
    #[derive(Clone)]
    enum Outcome {
        Ok,
        /// SQLSTATE 22P05 — untranslatable character. Data rejection.
        DataError22P05,
        /// SQLSTATE 22021 — invalid byte sequence for encoding. Data rejection.
        DataError22021,
        /// PoolTimedOut — transient.
        Transient,
    }

    /// Mock database error implementing `sqlx::error::DatabaseError`.
    /// Constructible with an arbitrary SQLSTATE for classifier tests and for
    /// the per-record fallback tests.
    #[derive(Debug)]
    struct MockDbError {
        code: String,
        message: String,
    }

    impl MockDbError {
        fn new(code: &str) -> Self {
            Self {
                code: code.to_string(),
                message: format!("mock db error sqlstate {code}"),
            }
        }
    }

    impl std::fmt::Display for MockDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl std::error::Error for MockDbError {}

    impl sqlx::error::DatabaseError for MockDbError {
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

    fn make_data_error(code: &str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(MockDbError::new(code)))
    }

    fn outcome_to_err(o: &Outcome) -> Result<(), sqlx::Error> {
        match o {
            Outcome::Ok => Ok(()),
            Outcome::DataError22P05 => Err(make_data_error("22P05")),
            Outcome::DataError22021 => Err(make_data_error("22021")),
            Outcome::Transient => Err(sqlx::Error::PoolTimedOut),
        }
    }

    /// Mock SpendDb that scripts batch + per-record outcomes.
    /// - `batch_outcome` decides the result of the first `insert_batch` call.
    /// - `per_record_outcomes` is consumed in order on `insert_one` calls;
    ///   if exhausted, defaults to `Ok`.
    struct MockSpendDb {
        batch_outcome: Outcome,
        per_record_outcomes: tokio::sync::Mutex<Vec<Outcome>>,
        batch_calls: AtomicUsize,
        per_record_calls: AtomicUsize,
        succeeded_request_ids: tokio::sync::Mutex<Vec<String>>,
        failed_request_ids: tokio::sync::Mutex<Vec<String>>,
    }

    impl MockSpendDb {
        fn new(batch: Outcome, per_record: Vec<Outcome>) -> Self {
            Self {
                batch_outcome: batch,
                per_record_outcomes: tokio::sync::Mutex::new(per_record),
                batch_calls: AtomicUsize::new(0),
                per_record_calls: AtomicUsize::new(0),
                succeeded_request_ids: tokio::sync::Mutex::new(Vec::new()),
                failed_request_ids: tokio::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::db::spend::SpendDb for MockSpendDb {
        async fn insert_batch(&self, _entries: &[RequestLogEntry]) -> Result<(), sqlx::Error> {
            self.batch_calls.fetch_add(1, Ordering::SeqCst);
            outcome_to_err(&self.batch_outcome)
        }

        async fn insert_one(&self, entry: &RequestLogEntry) -> Result<(), sqlx::Error> {
            self.per_record_calls.fetch_add(1, Ordering::SeqCst);
            let outcome = {
                let mut q = self.per_record_outcomes.lock().await;
                if q.is_empty() {
                    Outcome::Ok
                } else {
                    q.remove(0)
                }
            };
            match outcome_to_err(&outcome) {
                Ok(()) => {
                    self.succeeded_request_ids
                        .lock()
                        .await
                        .push(entry.request_id.clone());
                    Ok(())
                }
                Err(e) => {
                    self.failed_request_ids
                        .lock()
                        .await
                        .push(entry.request_id.clone());
                    Err(e)
                }
            }
        }
    }

    // ---- Helpers ------------------------------------------------------------

    /// Return the current value of the `ccag_spend_records_quarantined_total`
    /// counter from the Prometheus text exposition. Returns 0 if the metric
    /// has not yet been recorded (Prometheus omits unrecorded counters).
    fn quarantined_total(metrics: &Metrics) -> u64 {
        let text = metrics.prometheus_text();
        // Look for a line like:
        //   ccag_spend_records_quarantined_total 3
        //   ccag_spend_records_quarantined_total{...} 3
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            if line.starts_with("ccag_spend_records_quarantined_total")
                && let Some(v) = line.split_whitespace().last()
                && let Ok(n) = v.parse::<f64>()
            {
                return n as u64;
            }
        }
        0
    }

    // ---- Error classification ----------------------------------------------

    #[test]
    fn classify_22p05_is_data_rejection() {
        let err = make_data_error("22P05");
        assert!(
            !crate::db::spend::is_transient_db_error(&err),
            "SQLSTATE 22P05 (untranslatable character) must be classified as data rejection (drop), not transient"
        );
    }

    #[test]
    fn classify_22021_is_data_rejection() {
        let err = make_data_error("22021");
        assert!(
            !crate::db::spend::is_transient_db_error(&err),
            "SQLSTATE 22021 (invalid byte sequence) must be classified as data rejection (drop), not transient"
        );
    }

    #[test]
    fn classify_pool_timed_out_is_transient() {
        let err = sqlx::Error::PoolTimedOut;
        assert!(
            crate::db::spend::is_transient_db_error(&err),
            "PoolTimedOut must be classified as transient (re-buffer for retry)"
        );
    }

    #[test]
    fn classify_08006_connection_failure_is_transient() {
        // SQLSTATE class 08 = "Connection Exception". 08006 = "connection failure".
        let err = make_data_error("08006");
        assert!(
            crate::db::spend::is_transient_db_error(&err),
            "SQLSTATE 08006 (connection failure) must be classified as transient"
        );
    }

    #[test]
    fn classify_unknown_sqlstate_defaults_to_transient() {
        // Documented default: unknown codes are treated as TRANSIENT so
        // unfamiliar errors don't silently drop spend data. The class 99 and
        // codes outside the recognised data-exception (22xxx) and connection
        // (08xxx) classes should re-buffer rather than quarantine.
        let err = make_data_error("99999");
        assert!(
            crate::db::spend::is_transient_db_error(&err),
            "Unknown SQLSTATE codes must default to transient (re-buffer) so unrecognised errors do not silently drop data"
        );
    }

    #[test]
    fn classify_io_error_is_transient() {
        // sqlx::Error::Io wraps a std::io::Error. Connection-level IO
        // failures must re-buffer.
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err = sqlx::Error::Io(io_err);
        assert!(
            crate::db::spend::is_transient_db_error(&err),
            "Io-class errors must be classified as transient"
        );
    }

    // ---- flush behavior tests (using SpendDb seam) --------------------------

    #[tokio::test]
    async fn flush_succeeds_no_fallback() {
        let tracker = make_tracker();
        tracker.record(make_entry("req-1")).await;
        tracker.record(make_entry("req-2")).await;

        let db = MockSpendDb::new(Outcome::Ok, vec![]);
        let result = tracker.flush_with_db(&db).await;
        assert!(result.is_ok(), "Clean batch insert must succeed");
        assert_eq!(
            result.unwrap(),
            2,
            "Returned count must equal entries flushed"
        );

        // Buffer drained.
        assert_eq!(
            tracker.buffer_len().await,
            0,
            "Buffer must be drained on success"
        );

        // Per-record fallback NOT taken.
        assert_eq!(
            db.batch_calls.load(Ordering::SeqCst),
            1,
            "Batch insert must be called exactly once on the happy path"
        );
        assert_eq!(
            db.per_record_calls.load(Ordering::SeqCst),
            0,
            "Per-record fallback must NOT be invoked when batch succeeds"
        );

        // Quarantine counter unchanged.
        assert_eq!(
            quarantined_total(&tracker.metrics),
            0,
            "Quarantine counter must remain 0 on a clean flush"
        );
    }

    #[tokio::test]
    async fn flush_data_error_drops_only_poisoned_record() {
        let tracker = make_tracker();
        // Three records: req-1 clean, req-2 poison (will fail per-record), req-3 clean.
        tracker.record(make_entry("req-1")).await;
        tracker.record(make_entry("req-2")).await;
        tracker.record(make_entry("req-3")).await;

        // Batch fails with data error → triggers per-record fallback.
        // Per-record outcomes: ok, data-error, ok (positional, in buffer order).
        let db = MockSpendDb::new(
            Outcome::DataError22P05,
            vec![Outcome::Ok, Outcome::DataError22P05, Outcome::Ok],
        );

        let _ = tracker.flush_with_db(&db).await;

        // The two clean records were inserted individually.
        let succeeded = db.succeeded_request_ids.lock().await.clone();
        assert_eq!(
            succeeded.len(),
            2,
            "Two clean records must have been inserted individually"
        );
        assert!(succeeded.contains(&"req-1".to_string()));
        assert!(succeeded.contains(&"req-3".to_string()));

        // The poisoned record failed individually and was NOT re-buffered.
        let failed = db.failed_request_ids.lock().await.clone();
        assert_eq!(failed, vec!["req-2".to_string()]);

        // Anti-wedge invariant: poisoned record gone from buffer.
        assert_eq!(
            tracker.buffer_len().await,
            0,
            "Buffer must be empty after data-error flush — poisoned record must NOT be re-buffered"
        );

        // Quarantine counter incremented exactly once.
        assert_eq!(
            quarantined_total(&tracker.metrics),
            1,
            "Quarantine counter must be incremented by exactly the number of dropped records"
        );

        // Per-record fallback was actually entered.
        assert!(
            db.per_record_calls.load(Ordering::SeqCst) >= 1,
            "Per-record fallback must be invoked when batch fails with data error"
        );
    }

    #[tokio::test]
    async fn flush_transient_error_rebuffers_all() {
        let tracker = make_tracker();
        tracker.record(make_entry("req-1")).await;
        tracker.record(make_entry("req-2")).await;
        tracker.record(make_entry("req-3")).await;

        // Batch fails with a transient (pool-timed-out) error.
        let db = MockSpendDb::new(Outcome::Transient, vec![]);

        let result = tracker.flush_with_db(&db).await;
        assert!(
            result.is_err(),
            "Transient batch failure must surface as Err (loop logs + retries on next tick)"
        );

        // ALL records re-buffered.
        assert_eq!(
            tracker.buffer_len().await,
            3,
            "Transient batch failure must re-buffer ALL records (no data dropped on DB-down)"
        );

        // No per-record pass should run on transient batch failure — no point
        // hammering a down DB record-by-record.
        assert_eq!(
            db.per_record_calls.load(Ordering::SeqCst),
            0,
            "Per-record fallback must NOT run on transient batch failure"
        );

        // Quarantine counter unchanged.
        assert_eq!(
            quarantined_total(&tracker.metrics),
            0,
            "Quarantine counter must remain 0 on transient failure (no data dropped)"
        );
    }

    #[tokio::test]
    async fn flush_data_error_anti_wedge_invariant() {
        // Pure data-error case: batch fails, every record fails individually
        // with a data error. The buffer must be EMPTY afterwards — that's the
        // anti-wedge invariant. (Status quo would re-buffer them and wedge.)
        let tracker = make_tracker();
        tracker.record(make_entry("poison-1")).await;
        tracker.record(make_entry("poison-2")).await;

        let db = MockSpendDb::new(
            Outcome::DataError22P05,
            vec![Outcome::DataError22P05, Outcome::DataError22021],
        );

        let _ = tracker.flush_with_db(&db).await;

        assert_eq!(
            tracker.buffer_len().await,
            0,
            "After a data-error flush, the buffer must reflect only legitimately re-buffered transient records — 0 in pure data-error case (anti-wedge invariant)"
        );

        assert_eq!(
            quarantined_total(&tracker.metrics),
            2,
            "Both poisoned records must be quarantined"
        );
    }

    #[tokio::test]
    async fn flush_per_record_transient_rebuffers_remaining() {
        // During the per-record pass, if a record fails with a transient
        // error the pass should stop and remaining un-attempted records
        // should be re-buffered for next tick (per spec "Decision rule for
        // ambiguous/unknown errors during the per-record pass").
        let tracker = make_tracker();
        tracker.record(make_entry("req-1")).await;
        tracker.record(make_entry("req-2")).await;
        tracker.record(make_entry("req-3")).await;

        // Batch fails with data error → per-record fallback.
        // Per-record: ok, transient (DB just went down), then req-3 is
        // never attempted and must be re-buffered.
        let db = MockSpendDb::new(
            Outcome::DataError22P05,
            vec![Outcome::Ok, Outcome::Transient, Outcome::Ok],
        );

        let _ = tracker.flush_with_db(&db).await;

        // req-1 was inserted, req-2 hit transient → must be re-buffered,
        // req-3 was never attempted → must be re-buffered.
        // Buffer length == 2 (req-2 and req-3). Quarantine counter == 0
        // (no data-rejection drops occurred).
        assert_eq!(
            tracker.buffer_len().await,
            2,
            "Per-record transient failure must re-buffer the failing + un-attempted records"
        );
        assert_eq!(
            quarantined_total(&tracker.metrics),
            0,
            "Per-record transient failure must NOT increment the quarantine counter"
        );
    }

    #[tokio::test]
    async fn flush_rebuffer_respects_max_buffer_size() {
        // Re-buffering on transient failure must still cap at MAX_BUFFER_SIZE.
        // Preserves the existing capacity contract for the new fallback path.
        let tracker = make_tracker();

        // Fill buffer to capacity, then add a few more (oldest dropped) so
        // we know the buffer is exactly MAX_BUFFER_SIZE.
        for i in 0..MAX_BUFFER_SIZE {
            tracker.record(make_entry(&format!("req-{i}"))).await;
        }
        assert_eq!(tracker.buffer_len().await, MAX_BUFFER_SIZE);

        // Batch fails with transient → all entries re-buffer; none should
        // be dropped because total <= MAX_BUFFER_SIZE.
        let db = MockSpendDb::new(Outcome::Transient, vec![]);
        let _ = tracker.flush_with_db(&db).await;

        assert_eq!(
            tracker.buffer_len().await,
            MAX_BUFFER_SIZE,
            "Re-buffer on transient must cap at MAX_BUFFER_SIZE"
        );

        // Now record more entries — capacity must still hold (oldest dropped).
        tracker.record(make_entry("overflow-1")).await;
        assert_eq!(
            tracker.buffer_len().await,
            MAX_BUFFER_SIZE,
            "Buffer must remain capped at MAX_BUFFER_SIZE after transient re-buffer + new records"
        );
    }

    // ---- Metric method test (lives here because src/telemetry/mod.rs
    // already has its own #[cfg(test)] mod, and the same assertion form
    // works equally well from inside spend tests). -----------------------

    #[test]
    fn record_spend_records_quarantined_increments_counter() {
        let (metrics, _provider) = Metrics::new(None).unwrap();
        assert_eq!(quarantined_total(&metrics), 0);

        metrics.record_spend_records_quarantined(1);
        assert_eq!(quarantined_total(&metrics), 1);

        metrics.record_spend_records_quarantined(4);
        assert_eq!(
            quarantined_total(&metrics),
            5,
            "record_spend_records_quarantined(n) must add n (not always 1)"
        );

        // Confirm the metric name is exposed in Prometheus output.
        let text = metrics.prometheus_text();
        assert!(
            text.contains("ccag_spend_records_quarantined_total"),
            "Prometheus output must contain ccag_spend_records_quarantined_total counter"
        );
    }
}
