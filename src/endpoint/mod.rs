use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::Instant;

use tokio::sync::RwLock;
use uuid::Uuid;

use crate::db::schema::Endpoint;

pub mod stats;

/// A Bedrock client for a specific endpoint (account/region).
pub struct EndpointClient {
    pub config: Endpoint,
    pub runtime_client: aws_sdk_bedrockruntime::Client,
    pub control_client: aws_sdk_bedrock::Client,
    pub quota_cache: crate::quota::QuotaCache,
    pub healthy: AtomicBool,
    pub last_health_check: AtomicI64,
}

/// Manages multiple Bedrock clients and request routing.
pub struct EndpointPool {
    clients: RwLock<HashMap<Uuid, Arc<EndpointClient>>>,
    /// The designated default endpoint ID (fallback for unassigned teams).
    default_endpoint_id: RwLock<Option<Uuid>>,
    /// user_identity -> (endpoint_id, last_used)
    user_affinity: RwLock<HashMap<String, (Uuid, Instant)>>,
    /// Maximum entries in the affinity map before LRU eviction.
    max_affinity_entries: usize,
    /// Counter for round-robin strategy.
    round_robin_counter: AtomicUsize,
}

/// Affinity TTL: 30 minutes of inactivity.
const AFFINITY_TTL_SECS: u64 = 1800;

impl Default for EndpointPool {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointPool {
    pub fn new() -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            default_endpoint_id: RwLock::new(None),
            user_affinity: RwLock::new(HashMap::new()),
            max_affinity_entries: 10_000,
            round_robin_counter: AtomicUsize::new(0),
        }
    }

    /// Load/reload endpoint clients from database endpoint configs.
    /// For endpoints with role_arn, uses STS AssumeRole (via AssumeRoleProvider).
    /// For endpoints without role_arn (NULL), uses the gateway's own credentials.
    pub async fn load_endpoints(
        &self,
        endpoints: Vec<Endpoint>,
        base_aws_config: &aws_config::SdkConfig,
    ) {
        let mut new_clients = HashMap::new();
        let mut new_default: Option<Uuid> = None;

        for ep in endpoints {
            if ep.is_default {
                new_default = Some(ep.id);
            }
            let client = Self::create_client(&ep, base_aws_config).await;
            if let Some(c) = client {
                new_clients.insert(ep.id, Arc::new(c));
            }
        }

        let mut clients = self.clients.write().await;
        *clients = new_clients;
        drop(clients);

        let mut default_id = self.default_endpoint_id.write().await;
        *default_id = new_default;
    }

    async fn create_client(
        endpoint: &Endpoint,
        _base_config: &aws_config::SdkConfig,
    ) -> Option<EndpointClient> {
        let region = aws_config::Region::new(endpoint.region.clone());

        let sdk_config = if let Some(role_arn) = &endpoint.role_arn {
            // Cross-account: assume role using STS
            let sts_config = aws_config::from_env().region(region.clone()).load().await;
            let sts_client = aws_sdk_sts::Client::new(&sts_config);

            let assume_role_provider =
                aws_credential_types::provider::SharedCredentialsProvider::new(
                    AssumeRoleProvider {
                        sts_client,
                        role_arn: role_arn.clone(),
                        external_id: endpoint.external_id.clone(),
                        session_name: format!("ccag-{}", &endpoint.id.to_string()[..8]),
                    },
                );

            aws_config::from_env()
                .region(region)
                .credentials_provider(assume_role_provider)
                .load()
                .await
        } else {
            // Same account: use gateway's own credentials with the endpoint's region
            aws_config::from_env().region(region).load().await
        };

        let runtime_client = aws_sdk_bedrockruntime::Client::new(&sdk_config);
        let control_client = aws_sdk_bedrock::Client::new(&sdk_config);
        let quota_client = aws_sdk_servicequotas::Client::new(&sdk_config);

        Some(EndpointClient {
            config: endpoint.clone(),
            runtime_client,
            control_client,
            quota_cache: crate::quota::QuotaCache::new(quota_client),
            healthy: AtomicBool::new(false),
            last_health_check: AtomicI64::new(0),
        })
    }

    /// Select an endpoint for a request.
    ///
    /// - If team has assigned endpoints, uses those with the team's routing strategy.
    /// - If team has no assigned endpoints, falls back to the designated default endpoint.
    /// - Routing strategy changes take immediate effect (strategy is checked on every call).
    pub async fn select_endpoint(
        &self,
        team_endpoints: &[Endpoint],
        user_identity: Option<&str>,
        routing_strategy: &str,
    ) -> Option<Arc<EndpointClient>> {
        let clients = self.clients.read().await;

        if clients.is_empty() {
            return None;
        }

        // If no team-specific endpoints, fall back to the designated default endpoint.
        if team_endpoints.is_empty() {
            let default_id = self.default_endpoint_id.read().await;
            return default_id
                .and_then(|id| clients.get(&id))
                .filter(|c| c.config.enabled && c.healthy.load(Ordering::Relaxed))
                .map(Arc::clone);
        }

        let candidate_ids: Vec<Uuid> = team_endpoints.iter().map(|e| e.id).collect();

        // Sticky user: check affinity first. Bypassed if strategy is not sticky_user.
        if routing_strategy == "sticky_user"
            && let Some(identity) = user_identity
        {
            let affinity = self.user_affinity.read().await;
            if let Some((ep_id, last_seen)) = affinity.get(identity)
                && last_seen.elapsed().as_secs() < AFFINITY_TTL_SECS
                && candidate_ids.contains(ep_id)
                && let Some(client) = clients.get(ep_id)
                && client.healthy.load(Ordering::Relaxed)
            {
                return Some(Arc::clone(client));
            }
        }

        // Build ordered candidate list (by team assignment priority).
        let mut candidates: Vec<_> = candidate_ids
            .iter()
            .filter_map(|id| clients.get(id).map(Arc::clone))
            .collect();
        // team_endpoints is already ordered by te.priority from the DB query.
        // Re-sort to match that order (team priority takes precedence over global endpoint priority).
        candidates.sort_by_key(|c| {
            team_endpoints
                .iter()
                .position(|e| e.id == c.config.id)
                .unwrap_or(usize::MAX)
        });

        match routing_strategy {
            "round_robin" => {
                let healthy: Vec<_> = candidates
                    .into_iter()
                    .filter(|c| c.config.enabled && c.healthy.load(Ordering::Relaxed))
                    .collect();
                if healthy.is_empty() {
                    return None;
                }
                let idx = self.round_robin_counter.fetch_add(1, Ordering::Relaxed) % healthy.len();
                Some(Arc::clone(&healthy[idx]))
            }
            // Unknown strategy — fall through to primary_fallback.
            _ => candidates
                .into_iter()
                .find(|c| c.config.enabled && c.healthy.load(Ordering::Relaxed)),
        }
    }

    /// Get ordered list of fallback endpoints for a request (excluding the primary).
    pub async fn get_fallback_endpoints(
        &self,
        primary_id: Uuid,
        team_endpoints: &[Endpoint],
    ) -> Vec<Arc<EndpointClient>> {
        let clients = self.clients.read().await;

        // No team endpoints → no fallbacks (default endpoint has no fallback chain).
        if team_endpoints.is_empty() {
            return Vec::new();
        }

        let candidate_ids: Vec<Uuid> = team_endpoints.iter().map(|e| e.id).collect();

        let mut fallbacks: Vec<_> = candidate_ids
            .iter()
            .filter(|id| **id != primary_id)
            .filter_map(|id| clients.get(id).map(Arc::clone))
            .filter(|c| c.config.enabled && c.healthy.load(Ordering::Relaxed))
            .collect();
        fallbacks.sort_by_key(|c| {
            team_endpoints
                .iter()
                .position(|e| e.id == c.config.id)
                .unwrap_or(usize::MAX)
        });
        fallbacks
    }

    /// Update user affinity after a successful request.
    pub async fn update_affinity(&self, user_identity: &str, endpoint_id: Uuid) {
        let mut affinity = self.user_affinity.write().await;

        // LRU eviction if at capacity
        if affinity.len() >= self.max_affinity_entries
            && !affinity.contains_key(user_identity)
            && let Some(oldest_key) = affinity
                .iter()
                .min_by_key(|(_, (_, ts))| *ts)
                .map(|(k, _)| k.clone())
        {
            affinity.remove(&oldest_key);
        }

        affinity.insert(user_identity.to_string(), (endpoint_id, Instant::now()));
    }

    /// Clean up expired affinity entries.
    pub async fn cleanup_affinity(&self) {
        let mut affinity = self.user_affinity.write().await;
        affinity.retain(|_, (_, ts)| ts.elapsed().as_secs() < AFFINITY_TTL_SECS);
    }

    /// Get a specific endpoint client by ID.
    pub async fn get_client(&self, id: Uuid) -> Option<Arc<EndpointClient>> {
        let clients = self.clients.read().await;
        clients.get(&id).cloned()
    }

    /// Get all endpoint clients.
    pub async fn get_all_clients(&self) -> Vec<Arc<EndpointClient>> {
        let clients = self.clients.read().await;
        clients.values().cloned().collect()
    }

    /// Check if the pool has any clients loaded.
    pub async fn is_empty(&self) -> bool {
        let clients = self.clients.read().await;
        clients.is_empty()
    }

    /// Get the number of endpoints in the pool.
    pub async fn len(&self) -> usize {
        let clients = self.clients.read().await;
        clients.len()
    }

    /// Mark an endpoint as unhealthy.
    pub fn mark_unhealthy(client: &EndpointClient) {
        client.healthy.store(false, Ordering::Relaxed);
    }

    /// Mark an endpoint as healthy.
    pub fn mark_healthy(client: &EndpointClient) {
        client.healthy.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn make_test_endpoint(id: Uuid, name: &str, is_default: bool) -> Endpoint {
        Endpoint {
            id,
            name: name.to_string(),
            role_arn: None,
            external_id: None,
            inference_profile_arn: None,
            region: "us-east-1".to_string(),
            routing_prefix: "us".to_string(),
            priority: 0,
            is_default,
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }

    fn make_test_client(ep: Endpoint) -> EndpointClient {
        let runtime_client = aws_sdk_bedrockruntime::Client::from_conf(
            aws_sdk_bedrockruntime::Config::builder()
                .behavior_version(aws_sdk_bedrockruntime::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let control_client = aws_sdk_bedrock::Client::from_conf(
            aws_sdk_bedrock::Config::builder()
                .behavior_version(aws_sdk_bedrock::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let quota_client = aws_sdk_servicequotas::Client::from_conf(
            aws_sdk_servicequotas::Config::builder()
                .behavior_version(aws_sdk_servicequotas::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        EndpointClient {
            config: ep,
            runtime_client,
            control_client,
            quota_cache: crate::quota::QuotaCache::new(quota_client),
            healthy: AtomicBool::new(true),
            last_health_check: AtomicI64::new(0),
        }
    }

    async fn insert_client(pool: &EndpointPool, client: EndpointClient) {
        let mut clients = pool.clients.write().await;
        clients.insert(client.config.id, Arc::new(client));
    }

    async fn set_default(pool: &EndpointPool, id: Uuid) {
        let mut default_id = pool.default_endpoint_id.write().await;
        *default_id = Some(id);
    }

    // ── select_endpoint ──

    #[tokio::test]
    async fn test_select_empty_pool_returns_none() {
        let pool = EndpointPool::new();
        let result = pool.select_endpoint(&[], None, "primary_fallback").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_select_no_team_endpoints_uses_default() {
        let pool = EndpointPool::new();
        let id = Uuid::new_v4();
        let ep = make_test_endpoint(id, "default-ep", true);
        let client = make_test_client(ep);
        client.healthy.store(true, Ordering::Relaxed);
        insert_client(&pool, client).await;
        set_default(&pool, id).await;

        let result = pool.select_endpoint(&[], None, "primary_fallback").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().config.id, id);
    }

    #[tokio::test]
    async fn test_select_no_team_endpoints_unhealthy_default() {
        let pool = EndpointPool::new();
        let id = Uuid::new_v4();
        let ep = make_test_endpoint(id, "default-ep", true);
        let client = make_test_client(ep);
        client.healthy.store(false, Ordering::Relaxed);
        insert_client(&pool, client).await;
        set_default(&pool, id).await;

        let result = pool.select_endpoint(&[], None, "primary_fallback").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_round_robin_distributes() {
        let pool = EndpointPool::new();
        let mut ids = Vec::new();
        let mut team_eps = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        let mut counts = HashMap::new();
        for _ in 0..6 {
            let selected = pool
                .select_endpoint(&team_eps, None, "round_robin")
                .await
                .unwrap();
            *counts.entry(selected.config.id).or_insert(0u32) += 1;
        }

        for id in &ids {
            assert_eq!(
                counts.get(id).copied().unwrap_or(0),
                2,
                "Each endpoint should be selected exactly 2 times"
            );
        }
    }

    #[tokio::test]
    async fn test_round_robin_skips_unhealthy() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut healthy_ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            if i == 1 {
                client.healthy.store(false, Ordering::Relaxed);
            } else {
                client.healthy.store(true, Ordering::Relaxed);
                healthy_ids.push(id);
            }
            insert_client(&pool, client).await;
        }

        for _ in 0..6 {
            let selected = pool
                .select_endpoint(&team_eps, None, "round_robin")
                .await
                .unwrap();
            assert!(
                healthy_ids.contains(&selected.config.id),
                "Should only select healthy endpoints"
            );
        }
    }

    #[tokio::test]
    async fn test_round_robin_all_unhealthy() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(false, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        let result = pool.select_endpoint(&team_eps, None, "round_robin").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_primary_fallback_picks_first_healthy() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        let selected = pool
            .select_endpoint(&team_eps, None, "primary_fallback")
            .await
            .unwrap();
        assert_eq!(
            selected.config.id, ids[0],
            "Should pick the first healthy endpoint"
        );
    }

    #[tokio::test]
    async fn test_primary_fallback_skips_unhealthy() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            if i == 0 {
                client.healthy.store(false, Ordering::Relaxed);
            } else {
                client.healthy.store(true, Ordering::Relaxed);
            }
            insert_client(&pool, client).await;
        }

        let selected = pool
            .select_endpoint(&team_eps, None, "primary_fallback")
            .await
            .unwrap();
        assert_eq!(
            selected.config.id, ids[1],
            "Should skip unhealthy first and pick second"
        );
    }

    #[tokio::test]
    async fn test_sticky_user_returns_affinity() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Set affinity to the third endpoint
        pool.update_affinity("user@example.com", ids[2]).await;

        let selected = pool
            .select_endpoint(&team_eps, Some("user@example.com"), "sticky_user")
            .await
            .unwrap();
        assert_eq!(
            selected.config.id, ids[2],
            "Should return affinity endpoint"
        );
    }

    #[tokio::test]
    async fn test_sticky_user_expired_falls_through() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Manually insert expired affinity (past TTL)
        {
            let mut affinity = pool.user_affinity.write().await;
            affinity.insert(
                "user@example.com".to_string(),
                (
                    ids[2],
                    Instant::now() - Duration::from_secs(AFFINITY_TTL_SECS + 60),
                ),
            );
        }

        let selected = pool
            .select_endpoint(&team_eps, Some("user@example.com"), "sticky_user")
            .await
            .unwrap();
        // With expired affinity, falls through to primary_fallback which picks first healthy
        assert_eq!(
            selected.config.id, ids[0],
            "Should fall through to first healthy endpoint"
        );
    }

    #[tokio::test]
    async fn test_sticky_user_unhealthy_falls_through() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            // Mark the affinity endpoint (index 2) as unhealthy
            if i == 2 {
                client.healthy.store(false, Ordering::Relaxed);
            } else {
                client.healthy.store(true, Ordering::Relaxed);
            }
            insert_client(&pool, client).await;
        }

        // Set affinity to the unhealthy third endpoint
        pool.update_affinity("user@example.com", ids[2]).await;

        let selected = pool
            .select_endpoint(&team_eps, Some("user@example.com"), "sticky_user")
            .await
            .unwrap();
        // Unhealthy affinity endpoint should be skipped, falls through to primary_fallback
        assert_eq!(
            selected.config.id, ids[0],
            "Should fall through when affinity endpoint is unhealthy"
        );
    }

    // ── get_fallback_endpoints ──

    #[tokio::test]
    async fn test_fallback_excludes_primary() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        let fallbacks = pool.get_fallback_endpoints(ids[0], &team_eps).await;
        assert_eq!(fallbacks.len(), 2);
        for fb in &fallbacks {
            assert_ne!(
                fb.config.id, ids[0],
                "Fallbacks should not include the primary"
            );
        }
    }

    #[tokio::test]
    async fn test_fallback_empty_for_no_team_endpoints() {
        let pool = EndpointPool::new();
        let id = Uuid::new_v4();
        let ep = make_test_endpoint(id, "default-ep", true);
        let client = make_test_client(ep);
        client.healthy.store(true, Ordering::Relaxed);
        insert_client(&pool, client).await;
        set_default(&pool, id).await;

        let fallbacks = pool.get_fallback_endpoints(id, &[]).await;
        assert!(
            fallbacks.is_empty(),
            "No fallbacks when team_endpoints is empty"
        );
    }

    // ── affinity ──

    #[tokio::test]
    async fn test_update_affinity_creates_entry() {
        let pool = EndpointPool::new();
        let id = Uuid::new_v4();

        pool.update_affinity("user@test.com", id).await;

        let affinity = pool.user_affinity.read().await;
        assert!(affinity.contains_key("user@test.com"));
        let (ep_id, _) = affinity.get("user@test.com").unwrap();
        assert_eq!(*ep_id, id);
    }

    #[tokio::test]
    async fn test_affinity_lru_eviction() {
        let pool = EndpointPool {
            clients: RwLock::new(HashMap::new()),
            default_endpoint_id: RwLock::new(None),
            user_affinity: RwLock::new(HashMap::new()),
            max_affinity_entries: 3,
            round_robin_counter: AtomicUsize::new(0),
        };

        let ep_id = Uuid::new_v4();

        // Insert 3 entries with staggered timestamps so LRU ordering is deterministic
        {
            let mut affinity = pool.user_affinity.write().await;
            affinity.insert(
                "user1".to_string(),
                (ep_id, Instant::now() - Duration::from_secs(30)),
            );
            affinity.insert(
                "user2".to_string(),
                (ep_id, Instant::now() - Duration::from_secs(20)),
            );
            affinity.insert(
                "user3".to_string(),
                (ep_id, Instant::now() - Duration::from_secs(10)),
            );
        }

        // Insert a 4th — should evict user1 (oldest timestamp)
        pool.update_affinity("user4", ep_id).await;

        let affinity = pool.user_affinity.read().await;
        assert_eq!(affinity.len(), 3);
        assert!(
            !affinity.contains_key("user1"),
            "user1 (oldest) should have been evicted"
        );
        assert!(affinity.contains_key("user2"));
        assert!(affinity.contains_key("user3"));
        assert!(affinity.contains_key("user4"));
    }

    #[tokio::test]
    async fn test_cleanup_removes_expired() {
        let pool = EndpointPool::new();
        let ep_id = Uuid::new_v4();

        // Insert one fresh and one expired entry
        {
            let mut affinity = pool.user_affinity.write().await;
            affinity.insert("fresh_user".to_string(), (ep_id, Instant::now()));
            affinity.insert(
                "expired_user".to_string(),
                (
                    ep_id,
                    Instant::now() - Duration::from_secs(AFFINITY_TTL_SECS + 60),
                ),
            );
        }

        pool.cleanup_affinity().await;

        let affinity = pool.user_affinity.read().await;
        assert!(
            affinity.contains_key("fresh_user"),
            "Fresh entry should remain"
        );
        assert!(
            !affinity.contains_key("expired_user"),
            "Expired entry should be removed"
        );
    }

    // ── Slice 1: new tests ──

    /// `get_fallback_endpoints` returns endpoints in the order they appear in
    /// `team_endpoints`, which represents team-assignment priority. The first
    /// entry in `team_endpoints` is the highest-priority primary; fallbacks are
    /// the remaining entries in that same order.
    #[tokio::test]
    async fn test_fallback_ordering_matches_priority() {
        let pool = EndpointPool::new();

        // Create 3 endpoints. We add them to the pool in reverse priority order
        // (lowest priority first) to prove that fallback order is driven by
        // team_endpoints position, not insertion order.
        let id_high = Uuid::new_v4();
        let id_mid = Uuid::new_v4();
        let id_low = Uuid::new_v4();

        for (id, name) in [
            (id_low, "low-priority"),
            (id_mid, "mid-priority"),
            (id_high, "high-priority"),
        ] {
            let ep = make_test_endpoint(id, name, false);
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // team_endpoints ordered high -> mid -> low (simulates DB priority sort)
        let team_eps = vec![
            make_test_endpoint(id_high, "high-priority", false),
            make_test_endpoint(id_mid, "mid-priority", false),
            make_test_endpoint(id_low, "low-priority", false),
        ];

        // Exclude id_high (the primary); expect fallbacks in mid -> low order
        let fallbacks = pool.get_fallback_endpoints(id_high, &team_eps).await;

        assert_eq!(fallbacks.len(), 2, "Should have 2 fallback endpoints");
        assert_eq!(
            fallbacks[0].config.id, id_mid,
            "First fallback should be mid-priority"
        );
        assert_eq!(
            fallbacks[1].config.id, id_low,
            "Second fallback should be low-priority"
        );
    }

    /// When a user has a valid, healthy affinity entry but the affinity endpoint
    /// is NOT in the team's assigned endpoint list, `select_endpoint` must ignore
    /// the affinity and fall through to `primary_fallback` behavior.
    #[tokio::test]
    async fn test_sticky_user_endpoint_not_in_team_falls_through() {
        let pool = EndpointPool::new();

        // Endpoint A: in pool, healthy, but NOT in the team's assigned list
        let id_a = Uuid::new_v4();
        let ep_a = make_test_endpoint(id_a, "endpoint-a", false);
        let client_a = make_test_client(ep_a);
        client_a.healthy.store(true, Ordering::Relaxed);
        insert_client(&pool, client_a).await;

        // Endpoints B and C: in pool, healthy, and in the team's assigned list
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let mut team_eps = Vec::new();
        for (id, name) in [(id_b, "endpoint-b"), (id_c, "endpoint-c")] {
            let ep = make_test_endpoint(id, name, false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Set affinity for the user to endpoint A (not in team list)
        pool.update_affinity("user@example.com", id_a).await;

        let selected = pool
            .select_endpoint(&team_eps, Some("user@example.com"), "sticky_user")
            .await
            .unwrap();

        // Affinity check fails (id_a not in candidate_ids); falls through to
        // primary_fallback which picks the first healthy endpoint in team_eps.
        assert_eq!(
            selected.config.id, id_b,
            "Should fall through to first healthy team endpoint when affinity endpoint is not in team list"
        );
    }

    /// Calling `update_affinity` twice for the same user must overwrite the
    /// existing entry (both the endpoint ID and the timestamp are updated).
    #[tokio::test]
    async fn test_update_affinity_overwrites_entry() {
        let pool = EndpointPool::new();
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        pool.update_affinity("user1", id_a).await;

        // Capture the timestamp of the first write
        let ts_after_first = {
            let affinity = pool.user_affinity.read().await;
            let (ep_id, ts) = affinity.get("user1").unwrap();
            assert_eq!(*ep_id, id_a, "Should point to id_a after first write");
            *ts
        };

        // Overwrite with id_b
        pool.update_affinity("user1", id_b).await;

        let affinity = pool.user_affinity.read().await;
        assert_eq!(affinity.len(), 1, "Should still have exactly one entry");
        let (ep_id, ts_after_second) = affinity.get("user1").unwrap();
        assert_eq!(*ep_id, id_b, "Endpoint should now point to id_b");
        assert!(
            *ts_after_second >= ts_after_first,
            "Timestamp after second write must be >= timestamp after first write"
        );
    }

    /// The round-robin counter uses wrapping modular arithmetic. When the
    /// counter is near `usize::MAX`, it must wrap to 0 without panicking and
    /// still distribute requests across healthy endpoints.
    #[tokio::test]
    async fn test_round_robin_counter_wraparound() {
        let pool = EndpointPool::new();
        let mut ids = Vec::new();
        let mut team_eps = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Seed counter at usize::MAX - 1 so the next fetch_add wraps around
        pool.round_robin_counter
            .store(usize::MAX - 1, Ordering::Relaxed);

        // Call select_endpoint 6 times -- must not panic
        let mut results = Vec::new();
        for _ in 0..6 {
            let selected = pool
                .select_endpoint(&team_eps, None, "round_robin")
                .await
                .unwrap();
            results.push(selected.config.id);
        }

        // All returned IDs must be valid team endpoint IDs
        for id in &results {
            assert!(
                ids.contains(id),
                "Selected endpoint must be one of the team endpoints"
            );
        }

        assert_eq!(results.len(), 6, "Should have 6 results");
    }

    /// When `team_endpoints` contains UUIDs that are not loaded in the pool's
    /// client map, `select_endpoint` must return `None` rather than panicking
    /// or selecting from a different set of endpoints.
    #[tokio::test]
    async fn test_select_unknown_endpoint_ids_in_team_list() {
        let pool = EndpointPool::new();

        // Load one real endpoint into the pool (but NOT assigned to this team)
        let real_id = Uuid::new_v4();
        let real_ep = make_test_endpoint(real_id, "real-ep", false);
        let real_client = make_test_client(real_ep);
        real_client.healthy.store(true, Ordering::Relaxed);
        insert_client(&pool, real_client).await;

        // Build a team_endpoints list that references IDs NOT in the pool
        let ghost_id_1 = Uuid::new_v4();
        let ghost_id_2 = Uuid::new_v4();
        let team_eps = vec![
            make_test_endpoint(ghost_id_1, "ghost-1", false),
            make_test_endpoint(ghost_id_2, "ghost-2", false),
        ];

        // primary_fallback: filter_map produces empty candidates -> None
        let result_pf = pool
            .select_endpoint(&team_eps, None, "primary_fallback")
            .await;
        assert!(
            result_pf.is_none(),
            "primary_fallback should return None when all team endpoint IDs are unknown"
        );

        // round_robin: healthy list is empty -> None
        let result_rr = pool.select_endpoint(&team_eps, None, "round_robin").await;
        assert!(
            result_rr.is_none(),
            "round_robin should return None when all team endpoint IDs are unknown"
        );
    }

    /// When the default endpoint exists in the pool but has `enabled = false`,
    /// `select_endpoint` with empty `team_endpoints` should return `None`.
    ///
    /// NOTE: This test is expected to FAIL until the builder adds an `enabled`
    /// check in the default-endpoint path of `select_endpoint`. The current
    /// implementation only checks `healthy`, not `enabled`.
    #[tokio::test]
    async fn test_default_endpoint_disabled_returns_none() {
        let pool = EndpointPool::new();
        let id = Uuid::new_v4();

        // Create a default endpoint that is explicitly disabled
        let mut ep = make_test_endpoint(id, "disabled-default", true);
        ep.enabled = false;

        let client = make_test_client(ep);
        // Mark healthy so only the `enabled` flag should cause rejection
        client.healthy.store(true, Ordering::Relaxed);
        insert_client(&pool, client).await;
        set_default(&pool, id).await;

        let result = pool.select_endpoint(&[], None, "primary_fallback").await;
        assert!(
            result.is_none(),
            "Disabled default endpoint must not be returned by select_endpoint"
        );
    }
}

// ── Slice 2: new tests ──

#[cfg(test)]
mod tests_slice2 {
    use super::*;
    use std::sync::atomic::Ordering;

    fn make_test_endpoint(id: Uuid, name: &str, is_default: bool) -> Endpoint {
        Endpoint {
            id,
            name: name.to_string(),
            role_arn: None,
            external_id: None,
            inference_profile_arn: None,
            region: "us-east-1".to_string(),
            routing_prefix: "us".to_string(),
            priority: 0,
            is_default,
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }

    fn make_test_client(ep: Endpoint) -> EndpointClient {
        let runtime_client = aws_sdk_bedrockruntime::Client::from_conf(
            aws_sdk_bedrockruntime::Config::builder()
                .behavior_version(aws_sdk_bedrockruntime::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let control_client = aws_sdk_bedrock::Client::from_conf(
            aws_sdk_bedrock::Config::builder()
                .behavior_version(aws_sdk_bedrock::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let quota_client = aws_sdk_servicequotas::Client::from_conf(
            aws_sdk_servicequotas::Config::builder()
                .behavior_version(aws_sdk_servicequotas::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        EndpointClient {
            config: ep,
            runtime_client,
            control_client,
            quota_cache: crate::quota::QuotaCache::new(quota_client),
            healthy: AtomicBool::new(true),
            last_health_check: AtomicI64::new(0),
        }
    }

    async fn insert_client(pool: &EndpointPool, client: EndpointClient) {
        let mut clients = pool.clients.write().await;
        clients.insert(client.config.id, Arc::new(client));
    }

    /// `select_endpoint` with `"primary_fallback"` must skip team endpoints that
    /// have `enabled = false`, even when those endpoints are healthy. The first
    /// healthy AND enabled team endpoint must be returned.
    ///
    /// NOTE: Expected to FAIL until the builder adds an `enabled` check in the
    /// team-endpoint path of `select_endpoint`.
    #[tokio::test]
    async fn test_select_team_endpoint_disabled_skipped_primary_fallback() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let mut ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            if i == 0 {
                // First endpoint: disabled but healthy — must be skipped
                ep.enabled = false;
            }
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        let selected = pool
            .select_endpoint(&team_eps, None, "primary_fallback")
            .await
            .unwrap();

        assert_eq!(
            selected.config.id, ids[1],
            "primary_fallback must skip disabled first endpoint and pick the second (first healthy+enabled)"
        );
    }

    /// `select_endpoint` with `"round_robin"` must never return a team endpoint
    /// that has `enabled = false`, even when it is healthy.
    ///
    /// NOTE: Expected to FAIL until the builder adds an `enabled` check in the
    /// round-robin path of `select_endpoint`.
    #[tokio::test]
    async fn test_select_team_endpoint_disabled_skipped_round_robin() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let disabled_id;
        let enabled_id;

        // Endpoint 0: disabled+healthy (must never be selected)
        {
            let id = Uuid::new_v4();
            disabled_id = id;
            let mut ep = make_test_endpoint(id, "ep-disabled", false);
            ep.enabled = false;
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Endpoint 1: enabled+healthy (must always be selected)
        {
            let id = Uuid::new_v4();
            enabled_id = id;
            let ep = make_test_endpoint(id, "ep-enabled", false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        for _ in 0..10 {
            let selected = pool
                .select_endpoint(&team_eps, None, "round_robin")
                .await
                .unwrap();
            assert_ne!(
                selected.config.id, disabled_id,
                "round_robin must never select the disabled endpoint"
            );
            assert_eq!(
                selected.config.id, enabled_id,
                "round_robin must always select the only enabled+healthy endpoint"
            );
        }
    }

    /// `get_fallback_endpoints` must exclude endpoints that are unhealthy.
    /// When the second of three team endpoints is unhealthy, only the third
    /// (healthy, non-primary) endpoint should appear in the fallback list.
    ///
    /// This test verifies existing health-filtering behaviour and is expected to PASS.
    #[tokio::test]
    async fn test_fallback_excludes_unhealthy() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            // Mark the second endpoint (i == 1) as unhealthy
            client.healthy.store(i != 1, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Use ids[0] as the primary; expect only ids[2] as the fallback
        let fallbacks = pool.get_fallback_endpoints(ids[0], &team_eps).await;

        assert_eq!(
            fallbacks.len(),
            1,
            "Only one healthy non-primary fallback expected"
        );
        assert_eq!(
            fallbacks[0].config.id, ids[2],
            "The only fallback should be the third (healthy) endpoint"
        );
    }

    /// `get_fallback_endpoints` must exclude team endpoints that have
    /// `enabled = false`, even when those endpoints are healthy.
    ///
    /// NOTE: Expected to FAIL until the builder adds an `enabled` check in
    /// `get_fallback_endpoints`.
    #[tokio::test]
    async fn test_fallback_excludes_disabled() {
        let pool = EndpointPool::new();
        let mut team_eps = Vec::new();
        let mut ids = Vec::new();

        for i in 0..3 {
            let id = Uuid::new_v4();
            ids.push(id);
            let mut ep = make_test_endpoint(id, &format!("ep-{i}"), false);
            if i == 1 {
                // Second endpoint: disabled but healthy — must be excluded from fallbacks
                ep.enabled = false;
            }
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Use ids[0] as primary; only ids[2] is enabled+healthy non-primary
        let fallbacks = pool.get_fallback_endpoints(ids[0], &team_eps).await;

        assert_eq!(
            fallbacks.len(),
            1,
            "Disabled endpoint must be excluded; only one fallback expected"
        );
        assert_eq!(
            fallbacks[0].config.id, ids[2],
            "The only fallback should be the third (enabled+healthy) endpoint"
        );
    }

    /// `get_client` returns `Some` for a known endpoint ID and `None` for an
    /// unknown one. Verifies the basic map lookup contract.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_get_client_existing_and_missing() {
        let pool = EndpointPool::new();

        let existing_id = Uuid::new_v4();
        let ep = make_test_endpoint(existing_id, "known-ep", false);
        let client = make_test_client(ep);
        insert_client(&pool, client).await;

        // Known ID: must return Some
        let found = pool.get_client(existing_id).await;
        assert!(
            found.is_some(),
            "get_client must return Some for a known ID"
        );
        assert_eq!(
            found.unwrap().config.id,
            existing_id,
            "Returned client must have the requested ID"
        );

        // Unknown ID: must return None
        let missing_id = Uuid::new_v4();
        let not_found = pool.get_client(missing_id).await;
        assert!(
            not_found.is_none(),
            "get_client must return None for an unknown ID"
        );
    }
}

/// STS AssumeRole credential provider.
#[derive(Debug)]
struct AssumeRoleProvider {
    sts_client: aws_sdk_sts::Client,
    role_arn: String,
    external_id: Option<String>,
    session_name: String,
}

impl aws_credential_types::provider::ProvideCredentials for AssumeRoleProvider {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::new(async {
            let mut req = self
                .sts_client
                .assume_role()
                .role_arn(&self.role_arn)
                .role_session_name(&self.session_name);

            if let Some(ext_id) = &self.external_id {
                req = req.external_id(ext_id);
            }

            let result = req.send().await.map_err(|e| {
                aws_credential_types::provider::error::CredentialsError::provider_error(Box::new(
                    std::io::Error::other(e.to_string()),
                ))
            })?;

            let creds = result.credentials().ok_or_else(|| {
                aws_credential_types::provider::error::CredentialsError::provider_error(Box::new(
                    std::io::Error::other("No credentials in AssumeRole response"),
                ))
            })?;

            let expiration = {
                let exp_dt = creds.expiration();
                Some(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(exp_dt.secs() as u64),
                )
            };

            Ok(aws_credential_types::Credentials::new(
                creds.access_key_id(),
                creds.secret_access_key(),
                Some(creds.session_token().to_string()),
                expiration,
                "ccag-assume-role",
            ))
        })
    }
}
