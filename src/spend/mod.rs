use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::spend::{RequestLogEntry, insert_batch};
use crate::telemetry::Metrics;

/// Max buffered entries before dropping oldest. Prevents unbounded memory growth
/// if the database is unreachable.
const MAX_BUFFER_SIZE: usize = 10_000;

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
    pub async fn record(&self, entry: RequestLogEntry) {
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
    use crate::telemetry::Metrics;

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
}
