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
    /// Bedrock inference profile IDs available on this endpoint (e.g. `us.anthropic.claude-opus-4-7`).
    /// Populated by the health check loop; empty until the first successful tick.
    pub available_models: Arc<RwLock<Vec<String>>>,
}

impl EndpointClient {
    /// Returns `true` if any entry in `available_models` ends with `bedrock_model_id`.
    ///
    /// Uses suffix matching so callers can query with a short suffix like
    /// `"anthropic.claude-opus-4-7"` and still match the full profile ID
    /// `"us.anthropic.claude-opus-4-7"`.  Using `ends_with` (not `contains`)
    /// avoids false positives where a shorter partial ID would match a longer one.
    pub async fn supports_model(&self, bedrock_model_id: &str) -> bool {
        let models = self.available_models.read().await;
        models.iter().any(|m| m.ends_with(bedrock_model_id))
    }
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
    /// Bedrock inference profile IDs available on the gateway's default Bedrock endpoint.
    /// Populated by the health check loop every 60s; empty until the first successful tick.
    pub default_available_models: RwLock<Vec<String>>,
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
            default_available_models: RwLock::new(vec![]),
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
            available_models: Arc::new(RwLock::new(vec![])),
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
                && client.config.enabled
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

    /// Filter `team_endpoints` to those whose `EndpointClient` supports `bedrock_model`.
    ///
    /// Uses `EndpointClient::supports_model` (suffix matching) so callers can pass
    /// either the full profile ID (`us.anthropic.claude-sonnet-4-6`) or just the
    /// suffix (`anthropic.claude-sonnet-4-6`).
    ///
    /// Endpoints whose client is not loaded in the pool are excluded.
    /// Endpoints with an empty `available_models` list (not yet health-checked) are excluded.
    pub async fn filter_by_model(
        &self,
        team_endpoints: &[Endpoint],
        bedrock_model: &str,
    ) -> Vec<Endpoint> {
        // Collect (id, Arc<EndpointClient>) pairs under the lock, then drop the lock
        // before any await points to avoid holding a read-lock across awaits.
        let clients_snapshot: std::collections::HashMap<_, _> = {
            let guard = self.clients.read().await;
            guard
                .iter()
                .map(|(id, client)| (*id, Arc::clone(client)))
                .collect()
        };
        let mut result = Vec::new();
        for ep in team_endpoints {
            if let Some(client) = clients_snapshot.get(&ep.id)
                && client.supports_model(bedrock_model).await
            {
                result.push(ep.clone());
            }
        }
        result
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
            available_models: Arc::new(RwLock::new(vec![])),
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
            default_available_models: RwLock::new(vec![]),
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
            available_models: Arc::new(RwLock::new(vec![])),
        }
    }

    async fn insert_client(pool: &EndpointPool, client: EndpointClient) {
        let mut clients = pool.clients.write().await;
        clients.insert(client.config.id, Arc::new(client));
    }

    /// `select_endpoint` with `"primary_fallback"` must skip team endpoints that
    /// have `enabled = false`, even when those endpoints are healthy. The first
    /// healthy AND enabled team endpoint must be returned.
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

// ── Slice 3: new tests ──

#[cfg(test)]
mod tests_slice3 {
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
            available_models: Arc::new(RwLock::new(vec![])),
        }
    }

    async fn insert_client(pool: &EndpointPool, client: EndpointClient) {
        let mut clients = pool.clients.write().await;
        clients.insert(client.config.id, Arc::new(client));
    }

    #[allow(dead_code)]
    async fn set_default(pool: &EndpointPool, id: Uuid) {
        let mut default_id = pool.default_endpoint_id.write().await;
        *default_id = Some(id);
    }

    /// When a user has affinity set to an endpoint that is healthy but
    /// `enabled = false`, `select_endpoint` with `"sticky_user"` must NOT
    /// return that disabled endpoint. It should fall through to the first
    /// healthy+enabled team endpoint instead.
    ///
    /// NOTE: This test is expected to FAIL. The sticky_user path at line ~167
    /// only checks `client.healthy` but not `client.config.enabled`. The
    /// builder must add an `enabled` check there.
    #[tokio::test]
    async fn test_sticky_user_disabled_affinity_falls_through() {
        let pool = EndpointPool::new();

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        let mut team_eps = Vec::new();

        // id_a: disabled but healthy — affinity points here, must be skipped
        {
            let mut ep = make_test_endpoint(id_a, "ep-a", false);
            ep.enabled = false;
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // id_b: enabled+healthy — should be returned as the first valid fallback
        {
            let ep = make_test_endpoint(id_b, "ep-b", false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // id_c: enabled+healthy — also a candidate, but after id_b
        {
            let ep = make_test_endpoint(id_c, "ep-c", false);
            team_eps.push(ep.clone());
            let client = make_test_client(ep);
            client.healthy.store(true, Ordering::Relaxed);
            insert_client(&pool, client).await;
        }

        // Point the user's affinity to the disabled endpoint
        pool.update_affinity("user@example.com", id_a).await;

        let selected = pool
            .select_endpoint(&team_eps, Some("user@example.com"), "sticky_user")
            .await
            .unwrap();

        // The disabled affinity endpoint must be rejected; the first enabled+healthy
        // endpoint in team order (id_b) must be returned.
        assert_eq!(
            selected.config.id, id_b,
            "sticky_user must not return a disabled affinity endpoint; expected id_b (first healthy+enabled)"
        );
    }

    /// When `team_endpoints` is empty and no default has been set on the pool,
    /// `select_endpoint` must return `None` even when clients exist in the pool.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_no_default_set_empty_team_returns_none() {
        let pool = EndpointPool::new();

        // Insert a client but do NOT call set_default
        let id = Uuid::new_v4();
        let ep = make_test_endpoint(id, "some-ep", false);
        let client = make_test_client(ep);
        client.healthy.store(true, Ordering::Relaxed);
        insert_client(&pool, client).await;

        // No team endpoints and no default endpoint registered
        let result = pool.select_endpoint(&[], None, "primary_fallback").await;

        assert!(
            result.is_none(),
            "select_endpoint must return None when team_endpoints is empty and no default is set"
        );
    }

    /// With a single healthy+enabled team endpoint, `select_endpoint` with
    /// `"round_robin"` must return that same endpoint on every call regardless
    /// of how many times it is invoked.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_single_endpoint_round_robin() {
        let pool = EndpointPool::new();

        let id = Uuid::new_v4();
        let ep = make_test_endpoint(id, "only-ep", false);
        let team_eps = vec![ep.clone()];
        let client = make_test_client(ep);
        client.healthy.store(true, Ordering::Relaxed);
        insert_client(&pool, client).await;

        for call_num in 1..=3 {
            let selected = pool
                .select_endpoint(&team_eps, None, "round_robin")
                .await
                .unwrap();
            assert_eq!(
                selected.config.id, id,
                "Call {call_num}: round_robin with a single endpoint must always return that endpoint"
            );
        }
    }

    /// `is_empty` and `len` must accurately reflect the number of clients
    /// currently loaded in the pool.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_is_empty_and_len() {
        let pool = EndpointPool::new();

        // Fresh pool: no clients loaded
        assert!(
            pool.is_empty().await,
            "Newly created pool must report is_empty() == true"
        );
        assert_eq!(
            pool.len().await,
            0,
            "Newly created pool must report len() == 0"
        );

        // Insert first client
        let id1 = Uuid::new_v4();
        insert_client(
            &pool,
            make_test_client(make_test_endpoint(id1, "ep-1", false)),
        )
        .await;

        assert!(
            !pool.is_empty().await,
            "Pool with one client must report is_empty() == false"
        );
        assert_eq!(
            pool.len().await,
            1,
            "Pool with one client must report len() == 1"
        );

        // Insert second client
        let id2 = Uuid::new_v4();
        insert_client(
            &pool,
            make_test_client(make_test_endpoint(id2, "ep-2", false)),
        )
        .await;

        assert_eq!(
            pool.len().await,
            2,
            "Pool with two clients must report len() == 2"
        );
        assert!(
            !pool.is_empty().await,
            "Pool with two clients must report is_empty() == false"
        );
    }
}

#[cfg(test)]
mod tests_slice4 {
    use super::*;
    use std::sync::atomic::Ordering;

    fn make_dummy_aws_config() -> aws_config::SdkConfig {
        aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .build()
    }

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

    /// `load_endpoints` must populate the pool with one client per provided
    /// `Endpoint` and make each accessible via `get_client`.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_load_endpoints_populates_pool() {
        let pool = EndpointPool::new();
        let config = make_dummy_aws_config();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        let ep1 = make_test_endpoint(id1, "ep-1", false);
        let ep2 = make_test_endpoint(id2, "ep-2", false);
        let ep3 = make_test_endpoint(id3, "ep-3", false);

        pool.load_endpoints(vec![ep1, ep2, ep3], &config).await;

        assert_eq!(
            pool.len().await,
            3,
            "load_endpoints with 3 endpoints must produce pool.len() == 3"
        );
        assert!(
            !pool.is_empty().await,
            "pool must not be empty after loading 3 endpoints"
        );
        assert!(
            pool.get_client(id1).await.is_some(),
            "client for id1 must be present after load"
        );
        assert!(
            pool.get_client(id2).await.is_some(),
            "client for id2 must be present after load"
        );
        assert!(
            pool.get_client(id3).await.is_some(),
            "client for id3 must be present after load"
        );
    }

    /// When one of the loaded endpoints has `is_default: true`, `select_endpoint`
    /// with no team endpoints must return that default endpoint once it is healthy.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_load_endpoints_sets_default() {
        let pool = EndpointPool::new();
        let config = make_dummy_aws_config();

        let non_default_id = Uuid::new_v4();
        let default_id = Uuid::new_v4();

        let non_default_ep = make_test_endpoint(non_default_id, "non-default", false);
        let default_ep = make_test_endpoint(default_id, "default-ep", true);

        pool.load_endpoints(vec![non_default_ep, default_ep], &config)
            .await;

        // `create_client` initialises healthy = false; mark the default as healthy.
        pool.get_client(default_id)
            .await
            .unwrap()
            .healthy
            .store(true, Ordering::Relaxed);

        let selected = pool.select_endpoint(&[], None, "primary_fallback").await;

        assert!(
            selected.is_some(),
            "select_endpoint must return the default endpoint when team_endpoints is empty"
        );
        assert_eq!(
            selected.unwrap().config.id,
            default_id,
            "selected endpoint must be the one marked is_default"
        );

        // Non-default endpoint must also be accessible via get_client.
        assert!(
            pool.get_client(non_default_id).await.is_some(),
            "non-default endpoint must still be reachable via get_client"
        );
    }

    /// A second call to `load_endpoints` with a different set of endpoints must
    /// completely replace the previous pool contents.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_load_endpoints_reload_replaces() {
        let pool = EndpointPool::new();
        let config = make_dummy_aws_config();

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        pool.load_endpoints(
            vec![
                make_test_endpoint(id_a, "ep-a", false),
                make_test_endpoint(id_b, "ep-b", false),
            ],
            &config,
        )
        .await;

        assert_eq!(pool.len().await, 2, "initial load must produce 2 clients");

        // Reload with a completely different endpoint.
        let id_c = Uuid::new_v4();
        pool.load_endpoints(vec![make_test_endpoint(id_c, "ep-c", false)], &config)
            .await;

        assert_eq!(
            pool.len().await,
            1,
            "after reload with 1 endpoint, pool.len() must be 1"
        );
        assert!(
            pool.get_client(id_a).await.is_none(),
            "old endpoint id_a must not be present after reload"
        );
        assert!(
            pool.get_client(id_b).await.is_none(),
            "old endpoint id_b must not be present after reload"
        );
        assert!(
            pool.get_client(id_c).await.is_some(),
            "new endpoint id_c must be present after reload"
        );
    }

    /// Calling `load_endpoints` with an empty slice must clear all existing pool
    /// entries.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_load_endpoints_empty_clears_pool() {
        let pool = EndpointPool::new();
        let config = make_dummy_aws_config();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        pool.load_endpoints(
            vec![
                make_test_endpoint(id1, "ep-1", false),
                make_test_endpoint(id2, "ep-2", false),
            ],
            &config,
        )
        .await;

        assert_eq!(pool.len().await, 2, "initial load must produce 2 clients");

        // Reload with an empty list — pool must be cleared.
        pool.load_endpoints(vec![], &config).await;

        assert!(
            pool.is_empty().await,
            "pool must be empty after load_endpoints(vec![])"
        );
        assert_eq!(
            pool.len().await,
            0,
            "pool.len() must be 0 after load_endpoints(vec![])"
        );
    }
}

// ── Dynamic Model Availability — Slice 1 tests ──

#[cfg(test)]
mod tests_dynamic_model_slice1 {
    use super::*;

    fn make_test_endpoint(
        id: uuid::Uuid,
        name: &str,
        is_default: bool,
    ) -> crate::db::schema::Endpoint {
        crate::db::schema::Endpoint {
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

    fn make_test_client_with_models(
        ep: crate::db::schema::Endpoint,
        models: Vec<String>,
    ) -> EndpointClient {
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
            available_models: Arc::new(tokio::sync::RwLock::new(models)),
        }
    }

    /// `supports_model` returns `true` when the exact profile ID is in the list.
    #[tokio::test]
    async fn test_supports_model_returns_true_for_exact_match() {
        let ep = make_test_endpoint(uuid::Uuid::new_v4(), "ep", false);
        let client = make_test_client_with_models(
            ep,
            vec![
                "us.anthropic.claude-opus-4-7".to_string(),
                "us.anthropic.claude-sonnet-4-6-20250514".to_string(),
            ],
        );

        assert!(
            client.supports_model("us.anthropic.claude-opus-4-7").await,
            "supports_model must return true for an exact match in available_models"
        );
    }

    /// `supports_model` returns `false` when the model ID is not in the list.
    #[tokio::test]
    async fn test_supports_model_returns_false_for_absent_model() {
        let ep = make_test_endpoint(uuid::Uuid::new_v4(), "ep", false);
        let client = make_test_client_with_models(
            ep,
            vec!["us.anthropic.claude-sonnet-4-6-20250514".to_string()],
        );

        assert!(
            !client.supports_model("us.anthropic.claude-opus-4-7").await,
            "supports_model must return false when the model ID is not in available_models"
        );
    }

    /// `supports_model` returns `false` when `available_models` is empty.
    #[tokio::test]
    async fn test_supports_model_empty_list_returns_false() {
        let ep = make_test_endpoint(uuid::Uuid::new_v4(), "ep", false);
        let client = make_test_client_with_models(ep, vec![]);

        assert!(
            !client.supports_model("us.anthropic.claude-opus-4-7").await,
            "supports_model must return false when available_models is empty"
        );
    }

    /// `supports_model` uses contains-based (substring) matching so that a
    /// caller can query with the suffix portion of the profile ID (without the
    /// region prefix) and still get a match.
    ///
    /// Health loop stores full IDs like `us.anthropic.claude-opus-4-7`.
    /// Routing code may construct the suffix `anthropic.claude-opus-4-7` from
    /// the model mapping. The method must match both forms.
    #[tokio::test]
    async fn test_supports_model_contains_suffix_match() {
        let ep = make_test_endpoint(uuid::Uuid::new_v4(), "ep", false);
        let client =
            make_test_client_with_models(ep, vec!["us.anthropic.claude-opus-4-7".to_string()]);

        // Query with the suffix (no region prefix)
        assert!(
            client.supports_model("anthropic.claude-opus-4-7").await,
            "supports_model must match when the query is a suffix of a stored profile ID"
        );
    }

    /// `supports_model` must NOT match a different model just because it shares a
    /// common prefix with a stored ID.
    ///
    /// E.g. `us.anthropic.claude-opus-4` should NOT match
    /// `us.anthropic.claude-opus-4-7`.
    #[tokio::test]
    async fn test_supports_model_no_false_positive_partial_prefix() {
        let ep = make_test_endpoint(uuid::Uuid::new_v4(), "ep", false);
        let client =
            make_test_client_with_models(ep, vec!["us.anthropic.claude-opus-4-7".to_string()]);

        // A shorter string that is a prefix of the stored ID should NOT match
        assert!(
            !client.supports_model("anthropic.claude-opus-4").await,
            "supports_model must not return a false positive for a partial/shorter model ID"
        );
    }

    /// `available_models` starts empty on a fresh `EndpointClient` (before the
    /// health loop populates it).
    #[tokio::test]
    async fn test_available_models_starts_empty_on_new_client() {
        // Build a client the same way `create_client` will (no models supplied).
        let ep = make_test_endpoint(uuid::Uuid::new_v4(), "ep", false);
        let client = make_test_client_with_models(ep, vec![]);

        let models = client.available_models.read().await;
        assert!(
            models.is_empty(),
            "available_models must be empty before the health loop populates it"
        );
    }

    /// Writing to `available_models` is reflected in subsequent `supports_model`
    /// calls, simulating the health loop storing the profile list.
    #[tokio::test]
    async fn test_supports_model_reflects_update_after_write() {
        let ep = make_test_endpoint(uuid::Uuid::new_v4(), "ep", false);
        let client = make_test_client_with_models(ep, vec![]);

        // Sanity: absent before write
        assert!(
            !client
                .supports_model("us.anthropic.claude-sonnet-4-6-20250514")
                .await,
            "model must not be found before the list is populated"
        );

        // Simulate health loop storing profiles
        {
            let mut models = client.available_models.write().await;
            models.push("us.anthropic.claude-sonnet-4-6-20250514".to_string());
        }

        assert!(
            client
                .supports_model("us.anthropic.claude-sonnet-4-6-20250514")
                .await,
            "supports_model must return true after available_models is updated"
        );
    }
}

// ── Dynamic Model Availability — Slice 3 tests ──
// Tests for model-based filtering of endpoint candidates at request time.

#[cfg(test)]
mod tests_model_filtering_slice3 {
    use super::*;

    // ── helpers ────────────────────────────────────────────────────────────

    fn make_test_endpoint(id: Uuid, name: &str) -> Endpoint {
        Endpoint {
            id,
            name: name.to_string(),
            role_arn: None,
            external_id: None,
            inference_profile_arn: None,
            region: "us-east-1".to_string(),
            routing_prefix: "us".to_string(),
            priority: 0,
            is_default: false,
            enabled: true,
            created_at: chrono::Utc::now(),
        }
    }

    fn make_test_client_with_models(ep: Endpoint, models: Vec<String>) -> EndpointClient {
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
            available_models: Arc::new(RwLock::new(models)),
        }
    }

    async fn insert_client(pool: &EndpointPool, client: EndpointClient) {
        let mut clients = pool.clients.write().await;
        clients.insert(client.config.id, Arc::new(client));
    }

    // ── Test 1 ─────────────────────────────────────────────────────────────

    /// `filter_by_model` must return only those endpoints whose `EndpointClient`
    /// has the requested model in `available_models`.
    ///
    /// Given 3 endpoints where only 2 support `"anthropic.claude-sonnet-4-6-20250514"`,
    /// the filter must return exactly those 2 and exclude the third.
    #[tokio::test]
    async fn test_filter_returns_only_matching_endpoints() {
        let pool = EndpointPool::new();

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        let target_model = "us.anthropic.claude-sonnet-4-6-20250514";

        // Endpoint A: supports the target model
        insert_client(
            &pool,
            make_test_client_with_models(
                make_test_endpoint(id_a, "ep-a"),
                vec![
                    target_model.to_string(),
                    "us.anthropic.claude-opus-4-7".to_string(),
                ],
            ),
        )
        .await;

        // Endpoint B: supports the target model (only)
        insert_client(
            &pool,
            make_test_client_with_models(
                make_test_endpoint(id_b, "ep-b"),
                vec![target_model.to_string()],
            ),
        )
        .await;

        // Endpoint C: does NOT support the target model
        insert_client(
            &pool,
            make_test_client_with_models(
                make_test_endpoint(id_c, "ep-c"),
                vec!["us.anthropic.claude-opus-4-7".to_string()],
            ),
        )
        .await;

        let team_eps = vec![
            make_test_endpoint(id_a, "ep-a"),
            make_test_endpoint(id_b, "ep-b"),
            make_test_endpoint(id_c, "ep-c"),
        ];

        let filtered = pool
            .filter_by_model(&team_eps, "anthropic.claude-sonnet-4-6-20250514")
            .await;

        assert_eq!(
            filtered.len(),
            2,
            "filter_by_model must return exactly 2 endpoints that support the model"
        );

        let filtered_ids: Vec<Uuid> = filtered.iter().map(|e| e.id).collect();
        assert!(
            filtered_ids.contains(&id_a),
            "endpoint A (supports model) must be in the filtered result"
        );
        assert!(
            filtered_ids.contains(&id_b),
            "endpoint B (supports model) must be in the filtered result"
        );
        assert!(
            !filtered_ids.contains(&id_c),
            "endpoint C (does not support model) must be excluded from the filtered result"
        );
    }

    // ── Test 2 ─────────────────────────────────────────────────────────────

    /// `filter_by_model` must return an empty vec when no endpoint in
    /// `team_endpoints` supports the requested model.
    #[tokio::test]
    async fn test_filter_returns_empty_when_no_endpoint_supports_model() {
        let pool = EndpointPool::new();

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        // Both endpoints only have claude-opus-4-7 — not the requested sonnet model
        insert_client(
            &pool,
            make_test_client_with_models(
                make_test_endpoint(id_a, "ep-a"),
                vec!["us.anthropic.claude-opus-4-7".to_string()],
            ),
        )
        .await;
        insert_client(
            &pool,
            make_test_client_with_models(
                make_test_endpoint(id_b, "ep-b"),
                vec!["us.anthropic.claude-opus-4-7".to_string()],
            ),
        )
        .await;

        let team_eps = vec![
            make_test_endpoint(id_a, "ep-a"),
            make_test_endpoint(id_b, "ep-b"),
        ];

        let filtered = pool
            .filter_by_model(&team_eps, "anthropic.claude-sonnet-4-6-20250514")
            .await;

        assert!(
            filtered.is_empty(),
            "filter_by_model must return empty vec when no endpoint supports the model"
        );
    }

    // ── Test 3 ─────────────────────────────────────────────────────────────

    /// Endpoints with an empty `available_models` list (not yet health-checked)
    /// must be excluded from `filter_by_model` results. An empty model list means
    /// the endpoint hasn't received its first health check yet and we cannot
    /// confirm model availability.
    #[tokio::test]
    async fn test_filter_excludes_endpoints_with_empty_available_models() {
        let pool = EndpointPool::new();

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        // Both endpoints have empty available_models (startup state, no health tick yet)
        insert_client(
            &pool,
            make_test_client_with_models(make_test_endpoint(id_a, "ep-a"), vec![]),
        )
        .await;
        insert_client(
            &pool,
            make_test_client_with_models(make_test_endpoint(id_b, "ep-b"), vec![]),
        )
        .await;

        let team_eps = vec![
            make_test_endpoint(id_a, "ep-a"),
            make_test_endpoint(id_b, "ep-b"),
        ];

        let filtered = pool
            .filter_by_model(&team_eps, "anthropic.claude-sonnet-4-6-20250514")
            .await;

        assert!(
            filtered.is_empty(),
            "filter_by_model must return empty vec for endpoints with empty available_models (not yet health-checked)"
        );
    }

    // ── Test 4 ─────────────────────────────────────────────────────────────

    /// When only endpoint B supports the requested model, `select_endpoint`
    /// (called on the pre-filtered list) must route to B even if the normal
    /// routing strategy (e.g. `primary_fallback`) would have preferred A.
    ///
    /// This test exercises the full Slice 3 flow:
    ///   1. Build `team_endpoints` with A first, B second.
    ///   2. Call `filter_by_model` → returns only [B].
    ///   3. Call `select_endpoint` on the filtered list → must pick B.
    #[tokio::test]
    async fn test_select_endpoint_routes_to_model_supporting_endpoint() {
        let pool = EndpointPool::new();

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        let target_model = "us.anthropic.claude-sonnet-4-6-20250514";

        // Endpoint A: healthy, but does NOT support the requested model.
        // Under normal `primary_fallback` without filtering this would be chosen first.
        insert_client(
            &pool,
            make_test_client_with_models(
                make_test_endpoint(id_a, "ep-a"),
                vec!["us.anthropic.claude-opus-4-7".to_string()],
            ),
        )
        .await;

        // Endpoint B: healthy, supports the target model.
        insert_client(
            &pool,
            make_test_client_with_models(
                make_test_endpoint(id_b, "ep-b"),
                vec![target_model.to_string()],
            ),
        )
        .await;

        // team_endpoints: A has higher priority (index 0), B is index 1
        let team_eps = vec![
            make_test_endpoint(id_a, "ep-a"),
            make_test_endpoint(id_b, "ep-b"),
        ];

        // Step 1: filter — only B remains
        let filtered = pool
            .filter_by_model(&team_eps, "anthropic.claude-sonnet-4-6-20250514")
            .await;

        assert_eq!(
            filtered.len(),
            1,
            "only endpoint B supports the model; filtered list must have exactly 1 entry"
        );
        assert_eq!(
            filtered[0].id, id_b,
            "the single filtered endpoint must be B"
        );

        // Step 2: select from the filtered list — must pick B, not A
        let selected = pool
            .select_endpoint(&filtered, None, "primary_fallback")
            .await
            .expect("select_endpoint must return Some when filtered list has a healthy endpoint");

        assert_eq!(
            selected.config.id, id_b,
            "select_endpoint on the filtered list must route to endpoint B (the only one supporting the model)"
        );
    }

    // ── Test 5: clear error response (see src/api/handlers.rs) ────────────
    //
    // Belongs in src/api/handlers.rs #[cfg(test)] mod tests_model_filtering_slice3
    // because the `handlers` module is private and cannot be accessed from here.
    //
    // That test calls `build_model_unavailable_error(model_name)` — a new
    // `pub(crate)` function the builder must add to handlers.rs — and asserts:
    //   • HTTP 400 status
    //   • JSON body: error.type == "invalid_request_error"
    //   • error.message contains the model name
}

// ── Slice 5: edge-case tests ──

#[cfg(test)]
mod tests_slice5 {
    use super::*;
    use std::collections::HashSet;
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
            available_models: Arc::new(RwLock::new(vec![])),
        }
    }

    async fn insert_client(pool: &EndpointPool, client: EndpointClient) {
        let mut clients = pool.clients.write().await;
        clients.insert(client.config.id, Arc::new(client));
    }

    /// `mark_unhealthy` sets `healthy` to `false`; `mark_healthy` restores it to
    /// `true`. Both methods operate directly on the `EndpointClient` without
    /// requiring pool involvement.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_mark_healthy_and_unhealthy() {
        let ep = make_test_endpoint(Uuid::new_v4(), "ep", false);
        let client = make_test_client(ep);

        // Starts healthy (make_test_client sets healthy = true)
        assert!(
            client.healthy.load(Ordering::Relaxed),
            "client must start healthy"
        );

        EndpointPool::mark_unhealthy(&client);
        assert!(
            !client.healthy.load(Ordering::Relaxed),
            "mark_unhealthy must set healthy to false"
        );

        EndpointPool::mark_healthy(&client);
        assert!(
            client.healthy.load(Ordering::Relaxed),
            "mark_healthy must set healthy to true"
        );
    }

    /// `get_all_clients` must return all clients currently in the pool.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_get_all_clients() {
        let pool = EndpointPool::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        for (id, name) in [(id1, "ep-1"), (id2, "ep-2"), (id3, "ep-3")] {
            let ep = make_test_endpoint(id, name, false);
            insert_client(&pool, make_test_client(ep)).await;
        }

        let all = pool.get_all_clients().await;

        assert_eq!(all.len(), 3, "get_all_clients must return all 3 clients");

        let returned_ids: HashSet<Uuid> = all.iter().map(|c| c.config.id).collect();
        assert!(
            returned_ids.contains(&id1),
            "get_all_clients must include id1"
        );
        assert!(
            returned_ids.contains(&id2),
            "get_all_clients must include id2"
        );
        assert!(
            returned_ids.contains(&id3),
            "get_all_clients must include id3"
        );
    }

    /// An unknown routing strategy must fall through to the `_` arm of the match,
    /// which behaves identically to `"primary_fallback"`: it returns the first
    /// healthy+enabled endpoint in team order.
    ///
    /// Expected to PASS.
    #[tokio::test]
    async fn test_select_endpoint_unknown_strategy_uses_primary_fallback() {
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

        // Use a strategy name that does not match any known arm
        let selected = pool
            .select_endpoint(&team_eps, None, "some_future_strategy")
            .await
            .unwrap();

        // The `_` arm is identical to primary_fallback: returns the first healthy+enabled
        // endpoint in team_endpoints order, which is ids[0].
        assert_eq!(
            selected.config.id, ids[0],
            "unknown strategy must fall through to primary_fallback behavior (first healthy endpoint)"
        );
    }
}
