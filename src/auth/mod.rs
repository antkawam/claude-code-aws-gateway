pub mod oidc;
pub mod session;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::db;

/// Cached key info for the auth hot path.
#[derive(Debug, Clone)]
pub struct CachedKey {
    pub id: Uuid,
    pub name: Option<String>,
    pub user_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub rate_limit_rpm: Option<i32>,
}

/// In-memory key cache. Maps key_hash -> CachedKey.
/// Zero database calls per proxied request.
#[derive(Clone)]
pub struct KeyCache {
    inner: Arc<RwLock<HashMap<String, CachedKey>>>,
}

impl Default for KeyCache {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load all active keys from database into memory.
    pub async fn load_from_db(&self, pool: &sqlx::PgPool) -> anyhow::Result<usize> {
        let keys = db::keys::get_active_keys(pool).await?;
        let count = keys.len();
        let mut map = self.inner.write().await;
        map.clear();
        for key in keys {
            map.insert(
                key.key_hash.clone(),
                CachedKey {
                    id: key.id,
                    name: key.name,
                    user_id: key.user_id,
                    team_id: key.team_id,
                    rate_limit_rpm: key.rate_limit_rpm,
                },
            );
        }
        Ok(count)
    }

    /// Validate a raw API key. Returns the cached key info if valid.
    pub async fn validate(&self, raw_key: &str) -> Option<CachedKey> {
        let hash = db::keys::hash_key(raw_key);
        let map = self.inner.read().await;
        map.get(&hash).cloned()
    }

    /// Add a key to the cache (called after creating a new key).
    pub async fn insert(&self, key_hash: String, cached: CachedKey) {
        let mut map = self.inner.write().await;
        map.insert(key_hash, cached);
    }

    /// Number of keys in the cache.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Whether the cache is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::keys::hash_key;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_new_cache_is_empty() {
        let cache = KeyCache::new();
        assert_eq!(cache.len().await, 0);
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn test_insert_and_validate() {
        let cache = KeyCache::new();
        let id = Uuid::new_v4();
        let key_hash = hash_key("sk-proxy-test");
        cache
            .insert(
                key_hash,
                CachedKey {
                    id,
                    name: None,
                    user_id: None,
                    team_id: None,
                    rate_limit_rpm: None,
                },
            )
            .await;

        let result = cache.validate("sk-proxy-test").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, id);
    }

    #[tokio::test]
    async fn test_validate_unknown_returns_none() {
        let cache = KeyCache::new();
        let result = cache.validate("nonexistent").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_validate_returns_correct_metadata() {
        let cache = KeyCache::new();
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let key_hash = hash_key("sk-proxy-meta");
        cache
            .insert(
                key_hash,
                CachedKey {
                    id,
                    name: Some("test-key".to_string()),
                    user_id: Some(user_id),
                    team_id: Some(team_id),
                    rate_limit_rpm: Some(100),
                },
            )
            .await;

        let result = cache.validate("sk-proxy-meta").await.unwrap();
        assert_eq!(result.id, id);
        assert_eq!(result.name.as_deref(), Some("test-key"));
        assert_eq!(result.user_id, Some(user_id));
        assert_eq!(result.team_id, Some(team_id));
        assert_eq!(result.rate_limit_rpm, Some(100));
    }

    #[tokio::test]
    async fn test_multiple_keys_independent() {
        let cache = KeyCache::new();
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        let raw_keys = ["sk-proxy-aaa", "sk-proxy-bbb", "sk-proxy-ccc"];

        for (i, raw) in raw_keys.iter().enumerate() {
            cache
                .insert(
                    hash_key(raw),
                    CachedKey {
                        id: ids[i],
                        name: Some(format!("key-{}", i)),
                        user_id: None,
                        team_id: None,
                        rate_limit_rpm: None,
                    },
                )
                .await;
        }

        for (i, raw) in raw_keys.iter().enumerate() {
            let result = cache.validate(raw).await.unwrap();
            assert_eq!(result.id, ids[i]);
            assert_eq!(result.name.as_deref(), Some(format!("key-{}", i).as_str()));
        }
    }

    #[test]
    fn test_hash_determinism() {
        let h1 = hash_key("same-input");
        let h2 = hash_key("same-input");
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn test_cache_len_tracks_insertions() {
        let cache = KeyCache::new();
        for i in 0..3 {
            cache
                .insert(
                    hash_key(&format!("sk-proxy-len-{}", i)),
                    CachedKey {
                        id: Uuid::new_v4(),
                        name: None,
                        user_id: None,
                        team_id: None,
                        rate_limit_rpm: None,
                    },
                )
                .await;
        }
        assert_eq!(cache.len().await, 3);
    }
}
