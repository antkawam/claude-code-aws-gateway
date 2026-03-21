use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Simple sliding-window rate limiter. Tracks request timestamps per key.
#[derive(Clone)]
pub struct RateLimiter {
    windows: Arc<RwLock<HashMap<Uuid, SlidingWindow>>>,
}

struct SlidingWindow {
    timestamps: Vec<Instant>,
    limit_rpm: u32,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            windows: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if a request is allowed. Returns Ok(remaining) or Err(retry_after_secs).
    pub async fn check(&self, key_id: Uuid, limit_rpm: u32) -> Result<u32, u64> {
        let now = Instant::now();
        let window_duration = std::time::Duration::from_secs(60);

        let mut windows = self.windows.write().await;
        let window = windows.entry(key_id).or_insert_with(|| SlidingWindow {
            timestamps: Vec::new(),
            limit_rpm,
        });

        // Update limit in case it changed
        window.limit_rpm = limit_rpm;

        // Evict timestamps older than 60s
        window
            .timestamps
            .retain(|t| now.duration_since(*t) < window_duration);

        if window.timestamps.len() as u32 >= limit_rpm {
            // Calculate when the oldest request in the window will expire
            let oldest = window.timestamps[0];
            let retry_after = window_duration
                .checked_sub(now.duration_since(oldest))
                .map(|d| d.as_secs() + 1)
                .unwrap_or(1);
            Err(retry_after)
        } else {
            window.timestamps.push(now);
            let remaining = limit_rpm - window.timestamps.len() as u32;
            Ok(remaining)
        }
    }

    /// Evict stale entries (no requests in the last 2 minutes).
    pub async fn cleanup(&self) {
        let now = Instant::now();
        let stale = std::time::Duration::from_secs(120);
        let mut windows = self.windows.write().await;
        let before = windows.len();
        windows.retain(|_, w| {
            w.timestamps
                .last()
                .is_some_and(|t| now.duration_since(*t) < stale)
        });
        let removed = before - windows.len();
        if removed > 0 {
            tracing::debug!(removed, remaining = windows.len(), "Rate limiter cleanup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limit_allows_under_limit() {
        let limiter = RateLimiter::new();
        let key_id = Uuid::new_v4();
        let result = limiter.check(key_id, 10).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 9);
    }

    #[tokio::test]
    async fn test_rate_limit_blocks_over_limit() {
        let limiter = RateLimiter::new();
        let key_id = Uuid::new_v4();
        for _ in 0..5 {
            limiter.check(key_id, 5).await.unwrap();
        }
        let result = limiter.check(key_id, 5).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rate_limit_independent_keys() {
        let limiter = RateLimiter::new();
        let key1 = Uuid::new_v4();
        let key2 = Uuid::new_v4();
        for _ in 0..5 {
            limiter.check(key1, 5).await.unwrap();
        }
        // key2 should still be allowed
        let result = limiter.check(key2, 5).await;
        assert!(result.is_ok());
    }
}
