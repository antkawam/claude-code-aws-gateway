use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::RwLock;
use uuid::Uuid;

/// Rolling-window counters for a single endpoint.
pub struct EndpointCounters {
    /// (bucket_start, count) with 1-minute resolution
    throttle_buckets: Vec<(Instant, u64)>,
    error_buckets: Vec<(Instant, u64)>,
    request_count: AtomicU64,
}

impl Default for EndpointCounters {
    fn default() -> Self {
        Self {
            throttle_buckets: Vec::new(),
            error_buckets: Vec::new(),
            request_count: AtomicU64::new(0),
        }
    }
}

/// Snapshot of endpoint stats for API responses.
#[derive(Clone, serde::Serialize)]
pub struct EndpointStatSnapshot {
    pub throttle_count_1h: u64,
    pub error_count_1h: u64,
    pub request_count: u64,
}

/// Tracks per-endpoint operational stats with rolling 1-hour windows.
pub struct EndpointStats {
    inner: RwLock<HashMap<Uuid, EndpointCounters>>,
}

impl Default for EndpointStats {
    fn default() -> Self {
        Self::new()
    }
}

const BUCKET_WINDOW_SECS: u64 = 60; // 1-minute buckets
const RETENTION_SECS: u64 = 3600; // Keep 1 hour of data

impl EndpointStats {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub async fn record_throttle(&self, endpoint_id: Uuid) {
        let mut map = self.inner.write().await;
        let counters = map.entry(endpoint_id).or_default();
        let now = Instant::now();
        // Find or create current bucket
        if let Some(last) = counters.throttle_buckets.last_mut()
            && now.duration_since(last.0).as_secs() < BUCKET_WINDOW_SECS
        {
            last.1 += 1;
            return;
        }
        counters.throttle_buckets.push((now, 1));
    }

    pub async fn record_error(&self, endpoint_id: Uuid) {
        let mut map = self.inner.write().await;
        let counters = map.entry(endpoint_id).or_default();
        let now = Instant::now();
        if let Some(last) = counters.error_buckets.last_mut()
            && now.duration_since(last.0).as_secs() < BUCKET_WINDOW_SECS
        {
            last.1 += 1;
            return;
        }
        counters.error_buckets.push((now, 1));
    }

    pub async fn record_request(&self, endpoint_id: Uuid) {
        let map = self.inner.read().await;
        if let Some(counters) = map.get(&endpoint_id) {
            counters.request_count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        drop(map);
        let mut map = self.inner.write().await;
        map.entry(endpoint_id)
            .or_default()
            .request_count
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Get stats snapshot for all endpoints.
    pub async fn get_all_stats(&self) -> HashMap<Uuid, EndpointStatSnapshot> {
        let map = self.inner.read().await;
        let now = Instant::now();
        let mut result = HashMap::new();

        for (id, counters) in map.iter() {
            let throttle_count_1h: u64 = counters
                .throttle_buckets
                .iter()
                .filter(|(ts, _)| now.duration_since(*ts).as_secs() < RETENTION_SECS)
                .map(|(_, c)| c)
                .sum();
            let error_count_1h: u64 = counters
                .error_buckets
                .iter()
                .filter(|(ts, _)| now.duration_since(*ts).as_secs() < RETENTION_SECS)
                .map(|(_, c)| c)
                .sum();

            result.insert(
                *id,
                EndpointStatSnapshot {
                    throttle_count_1h,
                    error_count_1h,
                    request_count: counters.request_count.load(Ordering::Relaxed),
                },
            );
        }

        result
    }

    /// Evict buckets older than 1 hour.
    pub async fn cleanup(&self) {
        let mut map = self.inner.write().await;
        let now = Instant::now();
        for counters in map.values_mut() {
            counters
                .throttle_buckets
                .retain(|(ts, _)| now.duration_since(*ts).as_secs() < RETENTION_SECS);
            counters
                .error_buckets
                .retain(|(ts, _)| now.duration_since(*ts).as_secs() < RETENTION_SECS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_record_and_snapshot() {
        let stats = EndpointStats::new();
        let ep_id = Uuid::new_v4();

        stats.record_request(ep_id).await;
        stats.record_request(ep_id).await;
        stats.record_throttle(ep_id).await;
        stats.record_error(ep_id).await;
        stats.record_error(ep_id).await;

        let snaps = stats.get_all_stats().await;
        let snap = snaps.get(&ep_id).unwrap();
        assert_eq!(snap.request_count, 2);
        assert_eq!(snap.throttle_count_1h, 1);
        assert_eq!(snap.error_count_1h, 2);
    }

    #[tokio::test]
    async fn test_cleanup() {
        let stats = EndpointStats::new();
        let ep_id = Uuid::new_v4();

        stats.record_throttle(ep_id).await;
        stats.cleanup().await;

        // Recent bucket should still be there
        let snaps = stats.get_all_stats().await;
        assert_eq!(snaps.get(&ep_id).unwrap().throttle_count_1h, 1);
    }

    #[tokio::test]
    async fn test_multiple_endpoints() {
        let stats = EndpointStats::new();
        let ep1 = Uuid::new_v4();
        let ep2 = Uuid::new_v4();

        stats.record_request(ep1).await;
        stats.record_request(ep2).await;
        stats.record_request(ep2).await;

        let snaps = stats.get_all_stats().await;
        assert_eq!(snaps.get(&ep1).unwrap().request_count, 1);
        assert_eq!(snaps.get(&ep2).unwrap().request_count, 2);
    }

    /// Two `record_throttle` calls made within the same 60-second window must
    /// be aggregated into a single bucket. The snapshot must report
    /// `throttle_count_1h == 2` (sum of both increments in the one bucket),
    /// confirming that the bucket accumulates rather than creating a second entry.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_throttle_same_bucket_aggregates() {
        let stats = EndpointStats::new();
        let ep_id = Uuid::new_v4();

        // Both calls happen within milliseconds of each other — well within the
        // 60-second BUCKET_WINDOW_SECS threshold — so they must land in the same bucket.
        stats.record_throttle(ep_id).await;
        stats.record_throttle(ep_id).await;

        let snaps = stats.get_all_stats().await;
        let snap = snaps.get(&ep_id).unwrap();

        // The 1-hour sum must reflect both increments.
        assert_eq!(
            snap.throttle_count_1h, 2,
            "Two throttle calls within the same bucket window must aggregate to a count of 2"
        );

        // Verify there is exactly one bucket by reading the internal map directly.
        // A count of 2 from a single `get_all_stats` call is only possible when both
        // increments are in the same bucket (i.e., not two separate buckets each
        // contributing 1). We confirm this by checking the raw bucket vec length.
        //
        // `inner` is accessible here because this test lives in the same module
        // (`#[cfg(test)] mod tests` inside stats.rs).
        let map = stats.inner.read().await;
        let counters = map.get(&ep_id).unwrap();
        assert_eq!(
            counters.throttle_buckets.len(),
            1,
            "Both throttle calls within 60 s must be stored in exactly one bucket"
        );
    }
}
// #[cfg(test)] block above
