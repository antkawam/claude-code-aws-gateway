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
                .filter(|c| c.healthy.load(Ordering::Relaxed))
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
                    .filter(|c| c.healthy.load(Ordering::Relaxed))
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
                .find(|c| c.healthy.load(Ordering::Relaxed)),
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
            .filter(|c| c.healthy.load(Ordering::Relaxed))
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
