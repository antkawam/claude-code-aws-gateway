use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::spend::{RequestLogEntry, insert_batch};
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
    pub async fn flush(&self) -> anyhow::Result<usize> {
        let entries = {
            let mut buf = self.buffer.lock().await;
            std::mem::take(&mut *buf)
        };

        let count = entries.len();
        if count > 0 {
            let pool = self.db_pool.read().await.clone();
            if let Err(e) = insert_batch(&pool, &entries).await {
                // Put entries back on failure so they can be retried next flush
                tracing::warn!(%e, count, "Spend flush failed, re-buffering entries");
                self.metrics.record_spend_flush_error();
                let mut buf = self.buffer.lock().await;
                // Prepend failed entries (they're older), respecting buffer cap
                let available = MAX_BUFFER_SIZE.saturating_sub(buf.len());
                let to_restore = entries.into_iter().take(available);
                let existing = std::mem::take(&mut *buf);
                *buf = to_restore.chain(existing).collect();
                return Err(e);
            }
            tracing::debug!(count, "Flushed request log entries to database");
        }
        Ok(count)
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
}
