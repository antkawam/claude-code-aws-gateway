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
