use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use uuid::Uuid;

use crate::db::schema::Endpoint;

pub mod stats;

/// Shared capability map type — maps `(profile_id, beta_name)` to `CapabilityEntry`.
type BetaCapabilityMap = Arc<RwLock<HashMap<(String, String), CapabilityEntry>>>;

/// Pair of preserved cache Arcs carried from an existing `EndpointClient` into
/// a reloaded one so that learned capability data and model lists survive
/// routine `cache_version`-triggered reloads.
type PreservedCaches = (BetaCapabilityMap, Arc<RwLock<Vec<String>>>);

/// TTL for non-`AdminOverride` capability cache entries.
pub const CAPABILITY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Map from model-ID suffix to `(suffix, beta_name, display_label)`.
///
/// Used by:
/// - Health-loop seed probing (iterates beta_name across every available profile).
/// - `/v1/models` advertising (emits a suffixed variant for every `Some(true)` cache entry).
pub const SUFFIX_BETA_MAP: &[(&str, &str, &str)] =
    &[("[1m]", "context-1m-2025-08-07", "1M context")];

/// Who set a capability cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeSource {
    /// Written by the health-loop seed probe.
    SeedProbe,
    /// Written opportunistically when a real request succeeds with this beta.
    RequestSuccess,
    /// Written when Bedrock returns a ValidationException naming the beta.
    RequestRejection,
    /// Written by an operator via admin API / CLI. Ignores TTL and is never
    /// overwritten by automatic probing.
    AdminOverride,
}

/// One entry in the per-`(profile, beta)` capability cache.
#[derive(Debug, Clone)]
pub struct CapabilityEntry {
    pub supported: bool,
    pub learned_at: Instant,
    pub source: ProbeSource,
}

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
    /// Per-`(profile_id, beta_name)` capability cache.
    ///
    /// Key:   `(profile_id, beta_name)` — e.g. `("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")`.
    /// Value: `CapabilityEntry` — most recent probe outcome with TTL and source.
    ///
    /// `AdminOverride` entries ignore TTL and are never overwritten by automatic probing.
    /// Non-override entries expire after `CAPABILITY_TTL` (24 h).
    pub beta_capabilities: Arc<RwLock<HashMap<(String, String), CapabilityEntry>>>,
    /// Per-model AIP override map: Anthropic logical model ID → AIP ARN.
    /// Populated at endpoint load time from `endpoint_aip_overrides` DB rows.
    /// Empty for endpoints that have not been configured with any AIP overrides.
    pub aip_overrides: HashMap<String, String>,
    /// Subset of `available_models` whose entries were resolved from AIP overrides
    /// (not already present in the CRI list). Updated each health-loop tick alongside
    /// `available_models`. Used by `should_probe_profile` to gate capability probes.
    pub aip_derived_profile_ids: tokio::sync::RwLock<Vec<String>>,
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

    /// Query the beta capability cache for `(profile, beta)`.
    ///
    /// Returns:
    /// - `Some(true)` — entry exists, supported, and either `AdminOverride` or not yet expired.
    /// - `Some(false)` — entry exists, not supported, and either `AdminOverride` or not yet expired.
    /// - `None` — entry absent, or non-`AdminOverride` entry whose TTL has elapsed.
    pub async fn is_beta_supported(&self, profile: &str, beta: &str) -> Option<bool> {
        let map = self.beta_capabilities.read().await;
        let key = (profile.to_string(), beta.to_string());
        match map.get(&key) {
            None => None,
            Some(entry) => {
                if entry.source == ProbeSource::AdminOverride {
                    // Admin overrides never expire.
                    Some(entry.supported)
                } else if entry.learned_at + CAPABILITY_TTL <= Instant::now() {
                    // Expired non-override entry — treat as absent.
                    None
                } else {
                    Some(entry.supported)
                }
            }
        }
    }

    /// Record that `(profile, beta)` is supported.
    ///
    /// If the existing entry was written by `AdminOverride` and the incoming
    /// `source` is anything other than `AdminOverride`, the call is a no-op.
    /// Admin overrides stay until explicitly cleared via `forget_capability`.
    pub async fn mark_supported(&self, profile: &str, beta: &str, source: ProbeSource) {
        let mut map = self.beta_capabilities.write().await;
        let key = (profile.to_string(), beta.to_string());
        // Never overwrite an admin override from any other source.
        if let Some(existing) = map.get(&key)
            && existing.source == ProbeSource::AdminOverride
            && source != ProbeSource::AdminOverride
        {
            return;
        }
        map.insert(
            key,
            CapabilityEntry {
                supported: true,
                learned_at: Instant::now(),
                source,
            },
        );
    }

    /// Remove the `(profile, beta)` entry from the capability cache entirely.
    ///
    /// After this call, `is_beta_supported(profile, beta)` returns `None`.
    /// Used by the admin DELETE handler to clear an override and let the
    /// health-loop re-probe on the next cycle.
    pub async fn forget_capability(&self, profile: &str, beta: &str) {
        let mut map = self.beta_capabilities.write().await;
        map.remove(&(profile.to_string(), beta.to_string()));
    }

    /// Record that `(profile, beta)` is NOT supported.
    ///
    /// If the existing entry was written by `AdminOverride` and the incoming
    /// `source` is anything other than `AdminOverride`, the call is a no-op.
    /// Admin overrides stay until explicitly cleared via `forget_capability`.
    pub async fn mark_unsupported(&self, profile: &str, beta: &str, source: ProbeSource) {
        let mut map = self.beta_capabilities.write().await;
        let key = (profile.to_string(), beta.to_string());
        // Never overwrite an admin override from any other source.
        if let Some(existing) = map.get(&key)
            && existing.source == ProbeSource::AdminOverride
            && source != ProbeSource::AdminOverride
        {
            return;
        }
        map.insert(
            key,
            CapabilityEntry {
                supported: false,
                learned_at: Instant::now(),
                source,
            },
        );
    }

    /// Return `(profile, beta)` pairs that the health loop should seed-probe.
    ///
    /// A pair is included when:
    /// - No cache entry exists for it, OR
    /// - The entry exists, is not `AdminOverride`, and its TTL has elapsed.
    ///
    /// `AdminOverride` entries are never re-probed; they stay until explicitly cleared.
    pub async fn expired_seed_pairs(&self, profiles: &[String]) -> Vec<(String, String)> {
        let map = self.beta_capabilities.read().await;
        let now = Instant::now();
        let mut pairs = Vec::new();
        for profile in profiles {
            for &(_, beta_name, _) in SUFFIX_BETA_MAP {
                let key = (profile.clone(), beta_name.to_string());
                match map.get(&key) {
                    None => {
                        // Absent — must probe.
                        pairs.push((profile.clone(), beta_name.to_string()));
                    }
                    Some(entry) => {
                        if entry.source == ProbeSource::AdminOverride {
                            // Admin overrides are never re-probed.
                        } else if entry.learned_at + CAPABILITY_TTL <= now {
                            // Expired non-override — needs re-probe.
                            pairs.push((profile.clone(), beta_name.to_string()));
                        }
                        // Fresh non-override — skip.
                    }
                }
            }
        }
        pairs
    }

    /// Returns the AIP ARN for `model_id` from the new `aip_overrides` map, or
    /// `None` if no override exists for this model.
    ///
    /// Lookup order:
    /// 1. Raw match on `model_id` — preserves admin alias-key precedence.
    /// 2. Canonicalize `model_id` via `canonicalize_model_id`; if `Some(canonical)`,
    ///    retry the HashMap with the canonical form.
    ///
    /// This helper reads **only** the `aip_overrides` HashMap (populated from the
    /// `endpoint_aip_overrides` table). It does NOT fall back to
    /// `config.inference_profile_arn` (the legacy column). The legacy fallback is
    /// the responsibility of the request dispatcher — this strict boundary makes
    /// each code path testable in isolation.
    pub fn aip_override_for(&self, model_id: &str) -> Option<&str> {
        // 1. Try raw match first (admin can pin a non-canonical override key
        //    if they have an alias-shaped use case).
        if let Some(arn) = self.aip_overrides.get(model_id) {
            return Some(arn.as_str());
        }
        // 2. Try canonical form.
        let canonical = crate::translate::canonicalize::canonicalize_model_id(model_id)?;
        self.aip_overrides.get(&canonical).map(|s| s.as_str())
    }

    /// Returns `true` iff the `aip_overrides` map has at least one entry.
    ///
    /// Like `aip_override_for`, this ignores `config.inference_profile_arn`.
    /// An endpoint with only the legacy column set returns `false` here.
    pub fn has_any_aip_overrides(&self) -> bool {
        !self.aip_overrides.is_empty()
    }
}

// ── Seed-probe helpers ────────────────────────────────────────────────────────

/// Outcome of a single seed-probe InvokeModel call.
///
/// Used by the health loop to determine how to update the beta capability cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The model accepted the beta (HTTP 200).
    Supported,
    /// The model rejected the beta with a ValidationException that names it.
    Unsupported,
    /// The result was inconclusive: throttle, 5xx, network error, or a
    /// ValidationException that does NOT name the beta. Cache is unchanged;
    /// the pair will be re-probed on the next health-loop tick.
    Inconclusive,
}

/// Classify a raw `InvokeModel` probe result into a `ProbeOutcome`.
///
/// This is a pure function — no I/O, no `async`.
///
/// Arguments:
/// - `success`: `true` when `invoke_model().send().await` returned `Ok(_)`.
/// - `is_validation_exception`: `true` when the SDK error is a `ValidationException`.
/// - `error_message`: the full Debug-formatted SDK error string (used for beta-name matching).
/// - `beta`: the beta name we probed (e.g. `"context-1m-2025-08-07"`).
///
/// Mapping:
/// - `success == true` → `Supported`
/// - `is_validation_exception == true` AND `error_message` contains `beta` (case-insensitive) → `Unsupported`
/// - anything else → `Inconclusive`
pub fn classify_probe_outcome(
    success: bool,
    is_validation_exception: bool,
    error_message: &str,
    beta: &str,
) -> ProbeOutcome {
    if success {
        return ProbeOutcome::Supported;
    }
    if is_validation_exception && error_message.to_lowercase().contains(&beta.to_lowercase()) {
        return ProbeOutcome::Unsupported;
    }
    ProbeOutcome::Inconclusive
}

/// Apply a probe outcome to an `EndpointClient`'s beta capability cache.
///
/// - `Supported` → `mark_supported(profile, beta, SeedProbe)`
/// - `Unsupported` → `mark_unsupported(profile, beta, SeedProbe)`
/// - `Inconclusive` → no-op (cache unchanged)
pub async fn apply_probe_outcome(
    client: &EndpointClient,
    profile: &str,
    beta: &str,
    outcome: ProbeOutcome,
) {
    match outcome {
        ProbeOutcome::Supported => {
            client
                .mark_supported(profile, beta, ProbeSource::SeedProbe)
                .await;
        }
        ProbeOutcome::Unsupported => {
            client
                .mark_unsupported(profile, beta, ProbeSource::SeedProbe)
                .await;
        }
        ProbeOutcome::Inconclusive => {
            // No cache change — retry next tick.
        }
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
    ///
    /// Existing `beta_capabilities` and `available_models` Arcs are preserved for
    /// endpoints that are already in the pool (matched by UUID), so that learned
    /// capability data survives routine config reloads triggered by cache_version bumps.
    /// Only genuinely new endpoints get a fresh empty cache.
    ///
    /// `aip_overrides` are NOT loaded (empty maps on every client). Use
    /// [`load_endpoints_with_db`] when a DB pool is available to also populate
    /// per-endpoint AIP override maps.
    pub async fn load_endpoints(
        &self,
        endpoints: Vec<Endpoint>,
        base_aws_config: &aws_config::SdkConfig,
    ) {
        self.load_endpoints_inner(endpoints, base_aws_config, None)
            .await
    }

    /// Load/reload endpoint clients from database endpoint configs and populate
    /// each client's `aip_overrides` map from the `endpoint_aip_overrides` table.
    ///
    /// Equivalent to [`load_endpoints`] but fetches per-endpoint AIP override rows
    /// so that `EndpointClient::aip_override_for` and `has_any_aip_overrides` work
    /// correctly at runtime.
    pub async fn load_endpoints_with_db(
        &self,
        endpoints: Vec<Endpoint>,
        base_aws_config: &aws_config::SdkConfig,
        pool: &sqlx::PgPool,
    ) {
        self.load_endpoints_inner(endpoints, base_aws_config, Some(pool))
            .await
    }

    async fn load_endpoints_inner(
        &self,
        endpoints: Vec<Endpoint>,
        base_aws_config: &aws_config::SdkConfig,
        pool: Option<&sqlx::PgPool>,
    ) {
        let mut new_clients = HashMap::new();
        let mut new_default: Option<Uuid> = None;

        // Snapshot the current pool so we can preserve cache Arcs for unchanged endpoints.
        let preserved: HashMap<Uuid, PreservedCaches> = {
            let old_clients = self.clients.read().await;
            old_clients
                .iter()
                .map(|(id, c)| {
                    (
                        *id,
                        (
                            Arc::clone(&c.beta_capabilities),
                            Arc::clone(&c.available_models),
                        ),
                    )
                })
                .collect()
        };

        for ep in endpoints {
            if ep.is_default {
                new_default = Some(ep.id);
            }

            // Load AIP overrides from DB when a pool is available.
            let aip_overrides: HashMap<String, String> = if let Some(p) = pool {
                match crate::db::endpoint_aip_overrides::list_by_endpoint(p, ep.id).await {
                    Ok(rows) => rows.into_iter().map(|r| (r.model_id, r.aip_arn)).collect(),
                    Err(e) => {
                        tracing::warn!(endpoint_id = %ep.id, %e, "Failed to load AIP overrides for endpoint — using empty map");
                        HashMap::new()
                    }
                }
            } else {
                HashMap::new()
            };

            let preserved_caches = preserved.get(&ep.id).cloned();
            let client = Self::create_client_with_caches(
                &ep,
                base_aws_config,
                preserved_caches,
                aip_overrides,
            )
            .await;
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

    async fn create_client_with_caches(
        endpoint: &Endpoint,
        _base_config: &aws_config::SdkConfig,
        preserved_caches: Option<PreservedCaches>,
        aip_overrides: HashMap<String, String>,
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

        // Reuse existing cache Arcs when reloading a known endpoint, so that
        // learned capability data and available-model lists survive config reloads.
        let (beta_capabilities, available_models) = match preserved_caches {
            Some((beta_cap, avail_models)) => (beta_cap, avail_models),
            None => (
                Arc::new(RwLock::new(HashMap::new())),
                Arc::new(RwLock::new(vec![])),
            ),
        };

        Some(EndpointClient {
            config: endpoint.clone(),
            runtime_client,
            control_client,
            quota_cache: crate::quota::QuotaCache::new(quota_client),
            healthy: AtomicBool::new(false),
            last_health_check: AtomicI64::new(0),
            available_models,
            beta_capabilities,
            aip_overrides,
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
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

    /// Insert a pre-built `EndpointClient` directly into the pool.
    ///
    /// Only available when the `integration` feature is enabled.  Used by
    /// integration tests to inject fixture clients without going through
    /// `load_endpoints` (which requires real AWS credentials).
    #[cfg(feature = "integration")]
    pub async fn insert_client_for_testing(&self, client: EndpointClient) {
        let mut clients = self.clients.write().await;
        clients.insert(client.config.id, Arc::new(client));
    }

    /// Scan all rows in `endpoint_aip_overrides` and flag any whose `model_id` is
    /// not a canonical fixed-point (i.e. `canonicalize_model_id(model_id) !=
    /// Some(model_id.to_string())`).
    ///
    /// For each non-canonical row:
    /// - If the same `endpoint_id` already has a row keyed by the canonical form,
    ///   emit `tracing::warn!` (the older dated row will shadow the canonical one in
    ///   raw-match fast-path — admin attention needed).
    /// - Otherwise emit `tracing::info!` flagging the non-canonical key.
    ///
    /// Rows are **never** modified or deleted by this scan.
    pub async fn scan_non_canonical_aip_overrides(pool: &sqlx::PgPool) {
        // Fetch all rows from the table.
        let rows: Vec<(uuid::Uuid, String)> = match sqlx::query_as(
            "SELECT endpoint_id, model_id FROM endpoint_aip_overrides ORDER BY endpoint_id, model_id",
        )
        .fetch_all(pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "scan_non_canonical_aip_overrides: failed to fetch rows");
                return;
            }
        };

        // Build a set of (endpoint_id, model_id) pairs for O(1) sibling lookup.
        let all_keys: std::collections::HashSet<(uuid::Uuid, String)> = rows
            .iter()
            .map(|(ep_id, model_id)| (*ep_id, model_id.clone()))
            .collect();

        for (endpoint_id, model_id) in &rows {
            match crate::translate::canonicalize::canonicalize_model_id(model_id) {
                None => {
                    // Canonicalizer can't handle this key at all — flag as non-canonical.
                    tracing::info!(
                        %endpoint_id,
                        model_id = %model_id,
                        "non-canonical aip override key (canonicalization failed)"
                    );
                }
                Some(ref canonical) if canonical != model_id => {
                    // model_id is not the canonical fixed point.
                    if all_keys.contains(&(*endpoint_id, canonical.clone())) {
                        tracing::warn!(
                            %endpoint_id,
                            raw = %model_id,
                            %canonical,
                            "non_canonical_aip_override_collides_with_canonical_sibling"
                        );
                    } else {
                        tracing::info!(
                            %endpoint_id,
                            model_id = %model_id,
                            %canonical,
                            "non-canonical aip override key (no sibling conflict)"
                        );
                    }
                }
                Some(_) => {
                    // canonical == model_id: already a fixed-point; nothing to do.
                }
            }
        }
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

/// Result returned by [`compute_health_state`].
pub struct HealthState {
    /// Whether the endpoint is considered healthy.
    pub healthy: bool,
    /// Deduplicated union of CRI profiles + AIP-resolved model IDs.
    pub available_models: Vec<String>,
    /// Subset of `available_models` whose entries were contributed exclusively
    /// by AIP overrides (not already present in the CRI list).
    /// Empty for pure-CRI endpoints and on any failure.
    pub aip_derived_profile_ids: Vec<String>,
}

/// Compute the unified health state for a single endpoint.
///
/// Encapsulates the spec §Slice 2 Health loop changes logic:
///
/// 1. **CRI fails** → `(false, vec![])`. `get_foundation_model` is never called.
/// 2. **CRI ok + zero `aip_overrides` + no legacy ARN** → `(true, cri_list)`.
///    `get_foundation_model` is never called.
/// 3. **CRI ok + zero `aip_overrides` + legacy ARN set** → call
///    `get_foundation_model(legacy_arn)` once. On success parse and return
///    `(true, vec!["<prefix>.<bedrock_suffix>"])`. On failure `(false, vec![])`.
///    (Auto-migration safety net — behaves as today when new table is empty.)
/// 4. **CRI ok + `aip_overrides` non-empty** → call `get_foundation_model` once
///    per override. If ANY fails → `(false, vec![])`. If all succeed → deduplicated
///    union of `cri_list` ∪ AIP-resolved entries, `(true, union)`.
///
/// Returns a [`HealthState`] struct with `healthy`, `available_models`, and
/// `aip_derived_profile_ids` (the subset of `available_models` that came
/// exclusively from AIP overrides, i.e. not already covered by the CRI list).
pub async fn compute_health_state<F, Fut>(
    cri_list_result: Result<Vec<String>, String>,
    aip_overrides: &HashMap<String, String>,
    get_foundation_model: F,
    routing_prefix: &str,
    legacy_inference_profile_arn: Option<&str>,
) -> HealthState
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    // ── Branch 1: CRI failure → immediately unhealthy ─────────────────────────
    let cri_list = match cri_list_result {
        Ok(list) => list,
        Err(_) => {
            return HealthState {
                healthy: false,
                available_models: vec![],
                aip_derived_profile_ids: vec![],
            };
        }
    };

    // ── Branch 2: CRI ok, no new overrides, no legacy ARN → pure CRI ──────────
    if aip_overrides.is_empty() && legacy_inference_profile_arn.is_none() {
        return HealthState {
            healthy: true,
            available_models: cri_list,
            aip_derived_profile_ids: vec![],
        };
    }

    // ── Branch 3: CRI ok, no new overrides, legacy ARN set ────────────────────
    if aip_overrides.is_empty() {
        // Safety: guarded by `is_empty()` + `is_none()` check above — if we
        // reach here, `legacy_inference_profile_arn` is `Some`.
        let legacy_arn = legacy_inference_profile_arn.expect("guarded above");
        match get_foundation_model(legacy_arn.to_string()).await {
            Ok(fm_arn) => {
                match crate::translate::models::parse_foundation_model_from_arn(&fm_arn) {
                    Ok(bedrock_suffix) => {
                        let model_id = format!("{routing_prefix}.{bedrock_suffix}");
                        // Legacy ARN path: the single resolved entry is AIP-derived
                        // (there is no CRI list to compare against).
                        let aip_derived = vec![model_id.clone()];
                        return HealthState {
                            healthy: true,
                            available_models: vec![model_id],
                            aip_derived_profile_ids: aip_derived,
                        };
                    }
                    Err(_) => {
                        return HealthState {
                            healthy: false,
                            available_models: vec![],
                            aip_derived_profile_ids: vec![],
                        };
                    }
                }
            }
            Err(_) => {
                return HealthState {
                    healthy: false,
                    available_models: vec![],
                    aip_derived_profile_ids: vec![],
                };
            }
        }
    }

    // ── Branch 4: CRI ok + aip_overrides non-empty ────────────────────────────
    // Build union: CRI list first (exact dedup), then AIP-resolved entries.
    //
    // Dedup strategy for AIP entries: skip adding an AIP-resolved model if any
    // already-present entry is a prefix of the new model ID. This handles both:
    //   - Exact match (same string already in union).
    //   - Short-form CRI entry covering a long-form AIP entry, e.g.
    //     CRI "us.anthropic.claude-sonnet-4-5" already covers the AIP-resolved
    //     "us.anthropic.claude-sonnet-4-5-20250929-v1:0".
    let mut union: Vec<String> = Vec::new();
    let mut aip_derived: Vec<String> = Vec::new();

    // Seed with CRI list first (no duplicates expected, but guard anyway).
    for model in &cri_list {
        if !union.contains(model) {
            union.push(model.clone());
        }
    }

    // Helper: returns true iff `candidate` is already semantically covered
    // by an entry in `union` (exact match OR existing is a prefix of candidate).
    fn is_covered(union: &[String], candidate: &str) -> bool {
        union
            .iter()
            .any(|existing| candidate.starts_with(existing.as_str()))
    }

    // Resolve each AIP override. Sequential — mirrors existing health-loop style.
    for aip_arn in aip_overrides.values() {
        match get_foundation_model(aip_arn.clone()).await {
            Ok(fm_arn) => {
                match crate::translate::models::parse_foundation_model_from_arn(&fm_arn) {
                    Ok(bedrock_suffix) => {
                        let model_id = format!("{routing_prefix}.{bedrock_suffix}");
                        if !is_covered(&union, &model_id) {
                            // This entry is not already in the CRI list → AIP-derived.
                            aip_derived.push(model_id.clone());
                            union.push(model_id);
                        }
                        // If already covered by CRI, it is NOT added to aip_derived.
                    }
                    Err(_) => {
                        return HealthState {
                            healthy: false,
                            available_models: vec![],
                            aip_derived_profile_ids: vec![],
                        };
                    }
                }
            }
            Err(_) => {
                return HealthState {
                    healthy: false,
                    available_models: vec![],
                    aip_derived_profile_ids: vec![],
                };
            }
        }
    }

    HealthState {
        healthy: true,
        available_models: union,
        aip_derived_profile_ids: aip_derived,
    }
}

/// Returns `true` if a capability probe should be run for `profile_id`.
///
/// - If `capability_probe_aip_enabled` is `true`, all profiles are probed.
/// - If `false`, profiles present in `aip_derived_profile_ids` are skipped;
///   CRI-only profiles (not in that slice) continue to be probed.
pub fn should_probe_profile(
    profile_id: &str,
    aip_derived_profile_ids: &[String],
    capability_probe_aip_enabled: bool,
) -> bool {
    if capability_probe_aip_enabled {
        return true;
    }
    // Flag is false: skip if this profile is AIP-derived.
    !aip_derived_profile_ids.iter().any(|id| id == profile_id)
}

/// Resolves the effective value of the `CAPABILITY_PROBE_AIP` flag from two
/// optional sources, in priority order:
///
/// 1. `db_setting` — value from the `proxy_settings` table (highest priority).
/// 2. `env_value`  — value of the `CAPABILITY_PROBE_AIP` env var.
/// 3. Hard-coded default: `true`.
///
/// Parsing is case-insensitive: `"true"` / `"True"` / `"TRUE"` → `true`,
/// `"false"` / `"False"` / `"FALSE"` → `false`. Any other value falls through
/// to the next source.
pub fn effective_capability_probe_aip(db_setting: Option<&str>, env_value: Option<&str>) -> bool {
    fn parse_bool(s: &str) -> Option<bool> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("true") {
            Some(true)
        } else if trimmed.eq_ignore_ascii_case("false") {
            Some(false)
        } else {
            None
        }
    }

    if let Some(db) = db_setting
        && let Some(v) = parse_bool(db)
    {
        return v;
    }
    if let Some(env) = env_value
        && let Some(v) = parse_bool(env)
    {
        return v;
    }
    // Default: probe everything.
    true
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
            beta_capabilities: Arc::new(RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
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
            beta_capabilities: Arc::new(RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
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
            beta_capabilities: Arc::new(RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
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
    // #[cfg(test)] — Covers: select_endpoint sticky_user + enabled guard
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
            beta_capabilities: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
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
            beta_capabilities: Arc::new(RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
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
            beta_capabilities: Arc::new(RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
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

// ── 1M context / capability-cache — Slice 1 contract tests ──
//
// These tests are written BEFORE the production types exist. They will
// fail to compile until the Builder adds:
//   - `pub struct CapabilityEntry { pub supported: bool, pub learned_at: Instant, pub source: ProbeSource }`
//   - `pub enum ProbeSource { SeedProbe, RequestSuccess, RequestRejection, AdminOverride }`
//   - `pub const CAPABILITY_TTL: Duration`
//   - `pub const SUFFIX_BETA_MAP: &[(&str, &str, &str)]`
//   - Field `pub beta_capabilities: Arc<RwLock<HashMap<(String, String), CapabilityEntry>>>` on `EndpointClient`
//   - Methods `is_beta_supported`, `mark_supported`, `mark_unsupported`, `expired_seed_pairs`
//     on `EndpointClient`

#[cfg(test)]
mod tests_1m_capability_cache {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    // ── helpers ────────────────────────────────────────────────────────────

    fn make_test_endpoint_for_cache(id: uuid::Uuid) -> crate::db::schema::Endpoint {
        crate::db::schema::Endpoint {
            id,
            name: "test-ep".to_string(),
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

    /// Build a minimal `EndpointClient` with no real AWS credentials.
    /// Only the `beta_capabilities` field is exercised in these tests.
    fn make_cache_client() -> EndpointClient {
        let ep = make_test_endpoint_for_cache(uuid::Uuid::new_v4());
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
            last_health_check: std::sync::atomic::AtomicI64::new(0),
            available_models: Arc::new(tokio::sync::RwLock::new(vec![])),
            // Builder must add this field:
            beta_capabilities: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
        }
    }

    // ── Test 1 ─────────────────────────────────────────────────────────────

    /// Querying a key that was never inserted must return `None`.
    #[tokio::test]
    async fn is_beta_supported_returns_none_when_absent() {
        let client = make_cache_client();
        let result = client
            .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
            .await;
        assert_eq!(
            result, None,
            "is_beta_supported must return None for a key that has never been inserted"
        );
    }

    // ── Test 2 ─────────────────────────────────────────────────────────────

    /// After `mark_supported`, querying the same key must return `Some(true)`.
    #[tokio::test]
    async fn mark_supported_then_query_returns_some_true() {
        let client = make_cache_client();
        client
            .mark_supported(
                "us.anthropic.claude-opus-4-7",
                "context-1m-2025-08-07",
                ProbeSource::SeedProbe,
            )
            .await;
        let result = client
            .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
            .await;
        assert_eq!(
            result,
            Some(true),
            "is_beta_supported must return Some(true) immediately after mark_supported"
        );
    }

    // ── Test 3 ─────────────────────────────────────────────────────────────

    /// Last writer wins: `mark_unsupported` after `mark_supported` on the same
    /// key must leave the cache reflecting `supported = false`.
    #[tokio::test]
    async fn mark_unsupported_overrides_earlier_supported() {
        let client = make_cache_client();
        client
            .mark_supported(
                "us.anthropic.claude-sonnet-4-6",
                "context-1m-2025-08-07",
                ProbeSource::SeedProbe,
            )
            .await;
        client
            .mark_unsupported(
                "us.anthropic.claude-sonnet-4-6",
                "context-1m-2025-08-07",
                ProbeSource::RequestRejection,
            )
            .await;
        let result = client
            .is_beta_supported("us.anthropic.claude-sonnet-4-6", "context-1m-2025-08-07")
            .await;
        assert_eq!(
            result,
            Some(false),
            "mark_unsupported after mark_supported must yield Some(false) — last writer wins"
        );
    }

    // ── Test 4 ─────────────────────────────────────────────────────────────

    /// Last writer wins (opposite direction): `mark_supported` after
    /// `mark_unsupported` on the same key must yield `Some(true)`.
    #[tokio::test]
    async fn mark_supported_overrides_earlier_unsupported() {
        let client = make_cache_client();
        client
            .mark_unsupported(
                "us.anthropic.claude-haiku-4-5",
                "context-1m-2025-08-07",
                ProbeSource::SeedProbe,
            )
            .await;
        client
            .mark_supported(
                "us.anthropic.claude-haiku-4-5",
                "context-1m-2025-08-07",
                ProbeSource::RequestSuccess,
            )
            .await;
        let result = client
            .is_beta_supported("us.anthropic.claude-haiku-4-5", "context-1m-2025-08-07")
            .await;
        assert_eq!(
            result,
            Some(true),
            "mark_supported after mark_unsupported must yield Some(true) — last writer wins"
        );
    }

    // ── Test 5 ─────────────────────────────────────────────────────────────

    /// A non-`AdminOverride` entry with `learned_at` older than `CAPABILITY_TTL`
    /// must be treated as absent — `is_beta_supported` must return `None`.
    #[tokio::test]
    async fn non_override_entry_expires_after_ttl() {
        let client = make_cache_client();
        // Directly insert a backdated entry (learned_at = now - TTL - 1s)
        {
            let mut map = client.beta_capabilities.write().await;
            map.insert(
                (
                    "us.anthropic.claude-opus-4-7".to_string(),
                    "context-1m-2025-08-07".to_string(),
                ),
                CapabilityEntry {
                    supported: true,
                    learned_at: Instant::now() - (CAPABILITY_TTL + Duration::from_secs(1)),
                    source: ProbeSource::SeedProbe,
                },
            );
        }
        let result = client
            .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
            .await;
        assert_eq!(
            result, None,
            "is_beta_supported must return None for a SeedProbe entry whose TTL has elapsed"
        );
    }

    // ── Test 6 ─────────────────────────────────────────────────────────────

    /// An `AdminOverride` entry must be returned even when `learned_at` is far
    /// in the past (TTL is ignored for admin overrides).
    #[tokio::test]
    async fn admin_override_ignores_ttl() {
        let client = make_cache_client();
        // Directly insert a backdated AdminOverride entry (learned_at = now - 48h)
        {
            let mut map = client.beta_capabilities.write().await;
            map.insert(
                (
                    "us.anthropic.claude-opus-4-7".to_string(),
                    "context-1m-2025-08-07".to_string(),
                ),
                CapabilityEntry {
                    supported: true,
                    learned_at: Instant::now() - Duration::from_secs(48 * 3600),
                    source: ProbeSource::AdminOverride,
                },
            );
        }
        let result = client
            .is_beta_supported("us.anthropic.claude-opus-4-7", "context-1m-2025-08-07")
            .await;
        assert_eq!(
            result,
            Some(true),
            "AdminOverride entries must not expire regardless of learned_at age"
        );
    }

    // ── Test 7 ─────────────────────────────────────────────────────────────

    /// A fresh client with no cache entries must report every
    /// `(profile, beta)` pair from `SUFFIX_BETA_MAP` as needing a seed probe.
    #[tokio::test]
    async fn expired_seed_pairs_returns_absent_pairs() {
        let client = make_cache_client();
        let profiles = vec!["us.anthropic.claude-opus-4-7".to_string()];
        let pairs = client.expired_seed_pairs(&profiles).await;

        // SUFFIX_BETA_MAP currently has one entry: ("[1m]", "context-1m-2025-08-07", "1M context")
        // So for one profile we expect exactly one pair.
        assert_eq!(
            pairs.len(),
            1,
            "fresh client must report one pair per profile × SUFFIX_BETA_MAP entry"
        );
        assert!(
            pairs.contains(&(
                "us.anthropic.claude-opus-4-7".to_string(),
                "context-1m-2025-08-07".to_string()
            )),
            "the absent pair must be (profile, context-1m-2025-08-07)"
        );
    }

    // ── Test 8 ─────────────────────────────────────────────────────────────

    /// A pair with a fresh (non-expired) entry must NOT appear in
    /// `expired_seed_pairs`.
    #[tokio::test]
    async fn expired_seed_pairs_skips_fresh_entries() {
        let client = make_cache_client();
        // Pre-populate with a fresh SeedProbe entry (just now)
        client
            .mark_supported(
                "us.anthropic.claude-opus-4-7",
                "context-1m-2025-08-07",
                ProbeSource::SeedProbe,
            )
            .await;

        let profiles = vec!["us.anthropic.claude-opus-4-7".to_string()];
        let pairs = client.expired_seed_pairs(&profiles).await;

        assert!(
            pairs.is_empty(),
            "expired_seed_pairs must return empty when all pairs have fresh cache entries"
        );
    }

    // ── Test 9 ─────────────────────────────────────────────────────────────

    /// A `SeedProbe` entry whose TTL has elapsed must re-appear in
    /// `expired_seed_pairs` (re-probe is needed).
    #[tokio::test]
    async fn expired_seed_pairs_returns_expired_non_override() {
        let client = make_cache_client();
        // Directly insert a backdated SeedProbe entry
        {
            let mut map = client.beta_capabilities.write().await;
            map.insert(
                (
                    "us.anthropic.claude-opus-4-7".to_string(),
                    "context-1m-2025-08-07".to_string(),
                ),
                CapabilityEntry {
                    supported: true,
                    learned_at: Instant::now() - (CAPABILITY_TTL + Duration::from_secs(60)),
                    source: ProbeSource::SeedProbe,
                },
            );
        }

        let profiles = vec!["us.anthropic.claude-opus-4-7".to_string()];
        let pairs = client.expired_seed_pairs(&profiles).await;

        assert_eq!(
            pairs.len(),
            1,
            "expired SeedProbe entry must appear in expired_seed_pairs (re-probe needed)"
        );
        assert!(
            pairs.contains(&(
                "us.anthropic.claude-opus-4-7".to_string(),
                "context-1m-2025-08-07".to_string()
            )),
            "the expired pair must be (profile, context-1m-2025-08-07)"
        );
    }

    // ── Test 10 ────────────────────────────────────────────────────────────

    /// An `AdminOverride` entry must NEVER appear in `expired_seed_pairs`,
    /// even when its `learned_at` is far in the past. Admin overrides are set
    /// explicitly by an operator and must not be re-probed automatically.
    #[tokio::test]
    async fn expired_seed_pairs_skips_admin_overrides() {
        let client = make_cache_client();
        // Directly insert a backdated AdminOverride entry
        {
            let mut map = client.beta_capabilities.write().await;
            map.insert(
                (
                    "us.anthropic.claude-opus-4-7".to_string(),
                    "context-1m-2025-08-07".to_string(),
                ),
                CapabilityEntry {
                    supported: false,
                    learned_at: Instant::now() - Duration::from_secs(48 * 3600),
                    source: ProbeSource::AdminOverride,
                },
            );
        }

        let profiles = vec!["us.anthropic.claude-opus-4-7".to_string()];
        let pairs = client.expired_seed_pairs(&profiles).await;

        assert!(
            pairs.is_empty(),
            "AdminOverride entries must not appear in expired_seed_pairs — operators set them explicitly"
        );
    }
}

// ── Task 2: Health-loop seed probing — contract tests ──
//
// These tests are written BEFORE the production types exist. They will fail to
// compile until the Builder adds, somewhere in `src/endpoint/mod.rs` (or a
// sibling module `src/endpoint/probe.rs` re-exported from here):
//
//   pub enum ProbeOutcome { Supported, Unsupported, Inconclusive }
//
//   pub fn classify_probe_outcome(
//       success: bool,
//       is_validation_exception: bool,
//       error_message: &str,
//       beta: &str,
//   ) -> ProbeOutcome { ... }
//
//   pub async fn apply_probe_outcome(
//       client: &EndpointClient,
//       profile: &str,
//       beta: &str,
//       outcome: ProbeOutcome,
//   ) { ... }
//
// `classify_probe_outcome` is a pure function — no await, no I/O.
// `apply_probe_outcome` calls `mark_supported` / `mark_unsupported` on the
// client, or does nothing for `Inconclusive`.

#[cfg(test)]
mod tests_seed_probe {
    use super::*;
    use std::sync::atomic::AtomicBool;

    // ── helper: build a minimal EndpointClient (same pattern as tests_1m_capability_cache) ──

    fn make_cache_client() -> EndpointClient {
        let ep = crate::db::schema::Endpoint {
            id: uuid::Uuid::new_v4(),
            name: "probe-test-ep".to_string(),
            role_arn: None,
            external_id: None,
            inference_profile_arn: None,
            region: "us-east-1".to_string(),
            routing_prefix: "us".to_string(),
            priority: 0,
            is_default: false,
            enabled: true,
            created_at: chrono::Utc::now(),
        };
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
            last_health_check: std::sync::atomic::AtomicI64::new(0),
            available_models: Arc::new(tokio::sync::RwLock::new(vec![])),
            beta_capabilities: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            aip_overrides: HashMap::new(),
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
        }
    }

    // ── Tests 1-7: classify_probe_outcome — pure function, no async ──

    /// Test 1: HTTP 200 (success=true) must always yield `ProbeOutcome::Supported`,
    /// regardless of the error_message and is_validation_exception values.
    #[test]
    fn classify_success_returns_supported() {
        let outcome = classify_probe_outcome(true, false, "", "context-1m-2025-08-07");
        assert_eq!(
            outcome,
            ProbeOutcome::Supported,
            "success=true must yield ProbeOutcome::Supported"
        );
    }

    /// Test 2: ValidationException whose message contains the exact beta name
    /// must yield `ProbeOutcome::Unsupported`.
    #[test]
    fn classify_validation_with_beta_named_returns_unsupported() {
        let outcome = classify_probe_outcome(
            false,
            true,
            "ValidationException: Invalid beta flag context-1m-2025-08-07 for model X",
            "context-1m-2025-08-07",
        );
        assert_eq!(
            outcome,
            ProbeOutcome::Unsupported,
            "ValidationException whose message names the beta must yield ProbeOutcome::Unsupported"
        );
    }

    /// Test 3: Beta name matching must be case-insensitive.
    ///
    /// Bedrock's error format is not contractually cased; CCAG must not miss a
    /// rejection because the SDK upcased the beta name.
    #[test]
    fn classify_validation_with_beta_named_case_insensitive() {
        // Beta name in the error message is uppercased — must still match.
        let outcome = classify_probe_outcome(
            false,
            true,
            "ValidationException: Invalid beta flag CONTEXT-1M-2025-08-07 for model X",
            "context-1m-2025-08-07",
        );
        assert_eq!(
            outcome,
            ProbeOutcome::Unsupported,
            "classify_probe_outcome must match the beta name case-insensitively"
        );
    }

    /// Test 4: A ValidationException that does NOT name the beta must yield
    /// `ProbeOutcome::Inconclusive`.
    ///
    /// Rationale: a validation error unrelated to the beta (e.g. quota,
    /// malformed input, access denied) must not poison the cache as
    /// "unsupported". We cannot distinguish the cause, so we leave the cache
    /// unchanged and retry on the next tick.
    #[test]
    fn classify_validation_without_beta_named_returns_inconclusive() {
        let outcome = classify_probe_outcome(
            false,
            true,
            "ValidationException: AccessDeniedException: User is not authorized",
            "context-1m-2025-08-07",
        );
        assert_eq!(
            outcome,
            ProbeOutcome::Inconclusive,
            "ValidationException that does not name the beta must yield ProbeOutcome::Inconclusive"
        );
    }

    /// Test 5: ThrottlingException must yield `ProbeOutcome::Inconclusive`.
    ///
    /// Throttling tells us nothing about beta support; we must retry next tick
    /// rather than caching a false negative.
    #[test]
    fn classify_throttling_returns_inconclusive() {
        let outcome = classify_probe_outcome(
            false,
            false,
            "ThrottlingException: Rate exceeded",
            "context-1m-2025-08-07",
        );
        assert_eq!(
            outcome,
            ProbeOutcome::Inconclusive,
            "ThrottlingException must yield ProbeOutcome::Inconclusive"
        );
    }

    /// Test 6: InternalServerError / 5xx must yield `ProbeOutcome::Inconclusive`.
    ///
    /// Transient server-side errors must not be recorded as definitive
    /// unsupported evidence.
    #[test]
    fn classify_5xx_returns_inconclusive() {
        let outcome = classify_probe_outcome(
            false,
            false,
            "InternalServerError: An internal server error occurred",
            "context-1m-2025-08-07",
        );
        assert_eq!(
            outcome,
            ProbeOutcome::Inconclusive,
            "InternalServerError must yield ProbeOutcome::Inconclusive"
        );
    }

    /// Test 7: Network/dispatch failures must yield `ProbeOutcome::Inconclusive`.
    ///
    /// A connection reset or DNS failure is not evidence of beta support or
    /// rejection — it means we could not reach Bedrock at all.
    #[test]
    fn classify_network_error_returns_inconclusive() {
        let outcome = classify_probe_outcome(
            false,
            false,
            "dispatch failure: connection reset by peer",
            "context-1m-2025-08-07",
        );
        assert_eq!(
            outcome,
            ProbeOutcome::Inconclusive,
            "Network/dispatch failure must yield ProbeOutcome::Inconclusive"
        );
    }

    // ── Tests 8-11: apply_probe_outcome — integration-shaped, drives EndpointClient ──

    /// Test 8: `ProbeOutcome::Supported` must write `(supported=true, source=SeedProbe)`
    /// into the client's beta capability cache.
    #[tokio::test]
    async fn probe_outcome_supported_marks_cache() {
        let client = make_cache_client();
        let profile = "us.anthropic.claude-opus-4-7";
        let beta = "context-1m-2025-08-07";

        // Pre-condition: cache is empty
        assert_eq!(
            client.is_beta_supported(profile, beta).await,
            None,
            "cache must be empty before apply_probe_outcome"
        );

        apply_probe_outcome(&client, profile, beta, ProbeOutcome::Supported).await;

        let result = client.is_beta_supported(profile, beta).await;
        assert_eq!(
            result,
            Some(true),
            "ProbeOutcome::Supported must write Some(true) into the cache"
        );

        // Also verify source is SeedProbe
        let map = client.beta_capabilities.read().await;
        let entry = map
            .get(&(profile.to_string(), beta.to_string()))
            .expect("entry must exist after Supported outcome");
        assert_eq!(
            entry.source,
            ProbeSource::SeedProbe,
            "apply_probe_outcome(Supported) must record source=SeedProbe"
        );
    }

    /// Test 9: `ProbeOutcome::Unsupported` must write `(supported=false, source=SeedProbe)`
    /// into the client's beta capability cache.
    #[tokio::test]
    async fn probe_outcome_unsupported_marks_cache_false() {
        let client = make_cache_client();
        let profile = "us.anthropic.claude-haiku-4-5-20251001-v1:0";
        let beta = "context-1m-2025-08-07";

        apply_probe_outcome(&client, profile, beta, ProbeOutcome::Unsupported).await;

        let result = client.is_beta_supported(profile, beta).await;
        assert_eq!(
            result,
            Some(false),
            "ProbeOutcome::Unsupported must write Some(false) into the cache"
        );

        let map = client.beta_capabilities.read().await;
        let entry = map
            .get(&(profile.to_string(), beta.to_string()))
            .expect("entry must exist after Unsupported outcome");
        assert_eq!(
            entry.source,
            ProbeSource::SeedProbe,
            "apply_probe_outcome(Unsupported) must record source=SeedProbe"
        );
    }

    /// Test 10: `ProbeOutcome::Inconclusive` on a fresh (empty) cache must leave
    /// the cache unchanged — `is_beta_supported` must still return `None`.
    ///
    /// This is the "throttling / 5xx / network error" path: we learned nothing,
    /// so we do not touch the cache and the pair will be re-probed next tick.
    #[tokio::test]
    async fn probe_outcome_inconclusive_leaves_cache_unchanged() {
        let client = make_cache_client();
        let profile = "us.anthropic.claude-sonnet-4-6-20250514";
        let beta = "context-1m-2025-08-07";

        apply_probe_outcome(&client, profile, beta, ProbeOutcome::Inconclusive).await;

        let result = client.is_beta_supported(profile, beta).await;
        assert_eq!(
            result, None,
            "ProbeOutcome::Inconclusive on empty cache must leave it empty (still None)"
        );
    }

    /// Test 11: `ProbeOutcome::Inconclusive` must NOT overwrite an existing cache
    /// entry.
    ///
    /// Scenario: a previous successful probe wrote `Some(true)`. Then a
    /// throttling event fires before the 24h TTL expires. The `Inconclusive`
    /// outcome must not clobber the earlier `Supported` value.
    #[tokio::test]
    async fn probe_outcome_inconclusive_does_not_overwrite_existing() {
        let client = make_cache_client();
        let profile = "us.anthropic.claude-opus-4-7";
        let beta = "context-1m-2025-08-07";

        // Pre-populate with a supported entry (simulates prior successful probe)
        client
            .mark_supported(profile, beta, ProbeSource::SeedProbe)
            .await;

        assert_eq!(
            client.is_beta_supported(profile, beta).await,
            Some(true),
            "pre-condition: cache must have Some(true) before Inconclusive outcome"
        );

        // Apply an Inconclusive outcome (e.g. throttling on re-probe attempt)
        apply_probe_outcome(&client, profile, beta, ProbeOutcome::Inconclusive).await;

        let result = client.is_beta_supported(profile, beta).await;
        assert_eq!(
            result,
            Some(true),
            "ProbeOutcome::Inconclusive must not overwrite an existing Some(true) cache entry"
        );
    }
}
// ── AIP override helpers — Task 1 (Slice 1) tests ──
//
// These tests cover the two new methods that the builder will add to
// `EndpointClient`:
//
//   - `aip_override_for(model_id: &str) -> Option<&str>`
//     Returns the AIP ARN from the in-memory `aip_overrides` HashMap when a
//     matching entry exists, `None` otherwise.
//
//   - `has_any_aip_overrides() -> bool`
//     Returns `true` iff `aip_overrides` is non-empty.
//
// The `aip_overrides: HashMap<String, String>` field is populated at load time
// by reading `endpoint_aip_overrides` rows from the DB — the builder wires that
// in `load_endpoints` / `create_client`. Here we test the helper methods
// directly by constructing an `EndpointClient` with a known map.
//
// None of these tests require a database connection.

#[cfg(test)]
mod tests_aip_override_helpers {
    use super::*;
    use std::collections::HashMap;

    /// Build a minimal `EndpointClient` with the provided `aip_overrides` map
    /// and an optional `inference_profile_arn` on the config (legacy column).
    ///
    /// The AWS SDK clients are constructed with dummy configs — they are never
    /// invoked in these unit tests.
    fn make_client_with_overrides(
        aip_overrides: HashMap<String, String>,
        legacy_inference_profile_arn: Option<String>,
    ) -> EndpointClient {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::AtomicI64;

        let id = Uuid::new_v4();
        let config = crate::db::schema::Endpoint {
            id,
            name: "test-ep".to_string(),
            role_arn: None,
            external_id: None,
            inference_profile_arn: legacy_inference_profile_arn,
            region: "us-east-1".to_string(),
            routing_prefix: "us".to_string(),
            priority: 0,
            is_default: false,
            enabled: true,
            created_at: chrono::Utc::now(),
        };

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
            config,
            runtime_client,
            control_client,
            quota_cache: crate::quota::QuotaCache::new(quota_client),
            healthy: AtomicBool::new(true),
            last_health_check: AtomicI64::new(0),
            available_models: Arc::new(RwLock::new(vec![])),
            beta_capabilities: Arc::new(RwLock::new(HashMap::new())),
            aip_overrides,
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
        }
    }

    // ── aip_override_for ──────────────────────────────────────────────────────

    /// `aip_override_for` returns `Some(arn)` when an exact model_id entry
    /// exists in the `aip_overrides` map.
    #[test]
    fn test_aip_override_for_returns_arn_when_present() {
        let mut map = HashMap::new();
        let expected_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged";
        map.insert("claude-sonnet-4-5".to_string(), expected_arn.to_string());

        let client = make_client_with_overrides(map, None);

        let result = client.aip_override_for("claude-sonnet-4-5");
        assert_eq!(
            result,
            Some(expected_arn),
            "aip_override_for must return Some(arn) for a model present in the map"
        );
    }

    /// `aip_override_for` returns `None` when the requested model has no entry
    /// in the `aip_overrides` map, even when other models are present.
    #[test]
    fn test_aip_override_for_returns_none_for_absent_model() {
        let mut map = HashMap::new();
        map.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged"
                .to_string(),
        );
        map.insert(
            "claude-opus-4-7".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-tagged"
                .to_string(),
        );

        let client = make_client_with_overrides(map, None);

        let result = client.aip_override_for("claude-haiku-4-5");
        assert!(
            result.is_none(),
            "aip_override_for must return None for a model not in the override map"
        );
    }

    /// `aip_override_for` returns `None` when the map is empty.
    #[test]
    fn test_aip_override_for_returns_none_when_map_empty() {
        let client = make_client_with_overrides(HashMap::new(), None);

        let result = client.aip_override_for("claude-sonnet-4-5");
        assert!(
            result.is_none(),
            "aip_override_for must return None when aip_overrides map is empty"
        );
    }

    /// Boundary test: when no row exists in the new table (`aip_overrides` is
    /// empty) but `inference_profile_arn` is set on the legacy column,
    /// `aip_override_for` STILL returns `None`.
    ///
    /// The helper reads only the new `aip_overrides` map. The legacy fallback
    /// (force-substitution from `config.inference_profile_arn`) is the
    /// responsibility of the request dispatcher (Task 4), not this helper.
    /// This test codifies the boundary between the two code paths.
    #[test]
    fn test_aip_override_for_ignores_legacy_inference_profile_arn() {
        let legacy_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/legacy-sonnet";

        // Empty new-table map, but legacy column is set
        let client = make_client_with_overrides(HashMap::new(), Some(legacy_arn.to_string()));

        // The helper must return None — it does NOT fall through to the legacy column.
        // The dispatcher will check config.inference_profile_arn separately.
        let result = client.aip_override_for("claude-sonnet-4-5");
        assert!(
            result.is_none(),
            "aip_override_for must return None when aip_overrides is empty, even if \
             config.inference_profile_arn is set (legacy fallback is the dispatcher's job)"
        );
    }

    /// `aip_override_for` performs exact-key matching only. A partial model name
    /// (e.g. `"claude-sonnet"`) must NOT match the full key `"claude-sonnet-4-5"`.
    #[test]
    fn test_aip_override_for_exact_key_match_only() {
        let mut map = HashMap::new();
        map.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged"
                .to_string(),
        );

        let client = make_client_with_overrides(map, None);

        // Partial key must not match
        assert!(
            client.aip_override_for("claude-sonnet").is_none(),
            "aip_override_for must not match on a partial model name"
        );
        // Different version must not match
        assert!(
            client.aip_override_for("claude-sonnet-4-6").is_none(),
            "aip_override_for must not match a different version of the same model family"
        );
    }

    // ── has_any_aip_overrides ─────────────────────────────────────────────────

    /// `has_any_aip_overrides` returns `true` when at least one entry exists.
    #[test]
    fn test_has_any_aip_overrides_true_when_non_empty() {
        let mut map = HashMap::new();
        map.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet"
                .to_string(),
        );

        let client = make_client_with_overrides(map, None);
        assert!(
            client.has_any_aip_overrides(),
            "has_any_aip_overrides must return true when the map has entries"
        );
    }

    /// `has_any_aip_overrides` returns `false` when the map is empty.
    #[test]
    fn test_has_any_aip_overrides_false_when_empty() {
        let client = make_client_with_overrides(HashMap::new(), None);
        assert!(
            !client.has_any_aip_overrides(),
            "has_any_aip_overrides must return false when aip_overrides is empty"
        );
    }

    /// `has_any_aip_overrides` returns `false` even when the legacy
    /// `inference_profile_arn` is set, as long as the new map is empty.
    ///
    /// This mirrors the boundary test for `aip_override_for`: the legacy column
    /// is invisible to the new helpers.
    #[test]
    fn test_has_any_aip_overrides_false_with_only_legacy_column() {
        let legacy_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/legacy";
        let client = make_client_with_overrides(HashMap::new(), Some(legacy_arn.to_string()));

        assert!(
            !client.has_any_aip_overrides(),
            "has_any_aip_overrides must return false when aip_overrides is empty, \
             even if the legacy inference_profile_arn column is set"
        );
    }

    /// Multiple entries: `has_any_aip_overrides` remains `true` after inserting
    /// multiple models, and `aip_override_for` returns the correct ARN for each.
    #[test]
    fn test_multiple_overrides_all_accessible() {
        let sonnet_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet";
        let opus_arn = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus";

        let mut map = HashMap::new();
        map.insert("claude-sonnet-4-5".to_string(), sonnet_arn.to_string());
        map.insert("claude-opus-4-7".to_string(), opus_arn.to_string());

        let client = make_client_with_overrides(map, None);

        assert!(
            client.has_any_aip_overrides(),
            "has_any_aip_overrides must be true for a map with 2 entries"
        );
        assert_eq!(
            client.aip_override_for("claude-sonnet-4-5"),
            Some(sonnet_arn),
            "must return correct sonnet ARN"
        );
        assert_eq!(
            client.aip_override_for("claude-opus-4-7"),
            Some(opus_arn),
            "must return correct opus ARN"
        );
        assert!(
            client.aip_override_for("claude-haiku-4-5").is_none(),
            "haiku has no override, must return None"
        );
    }

    // ── AC5.1 — Canonicalization path ────────────────────────────────────────

    /// AC5.1: `aip_override_for("claude-sonnet-4-6-20250514")` against a map
    /// containing only `("claude-sonnet-4-6", "arn:...")` returns `Some("arn:...")`.
    ///
    /// After Task 5 the implementation must:
    ///   1. Try raw match → miss (map has "claude-sonnet-4-6", not the dated form).
    ///   2. Canonicalize input → "claude-sonnet-4-6".
    ///   3. Hit canonical key → return ARN.
    ///
    /// PRE-IMPLEMENTATION FAILURE MODE: assertion-fail (returns None because the
    /// current implementation does raw HashMap::get only).
    #[test]
    fn test_aip_override_for_canonicalize_dated_variant() {
        let canonical_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-46";
        let mut map = HashMap::new();
        map.insert("claude-sonnet-4-6".to_string(), canonical_arn.to_string());

        let client = make_client_with_overrides(map, None);

        let result = client.aip_override_for("claude-sonnet-4-6-20250514");
        assert_eq!(
            result,
            Some(canonical_arn),
            "aip_override_for(\"claude-sonnet-4-6-20250514\") must find the \
             \"claude-sonnet-4-6\" override via canonicalization (date-strip), \
             got {:?}",
            result
        );
    }

    // ── AC5.2 — Auto-prefix path ──────────────────────────────────────────────

    /// AC5.2: `aip_override_for("opus-4-6")` against a map containing only
    /// `("claude-opus-4-6", "arn:...")` returns `Some("arn:...")`.
    ///
    /// After Task 5 the implementation must:
    ///   1. Try raw match → miss (map has "claude-opus-4-6").
    ///   2. Canonicalize "opus-4-6" → "claude-opus-4-6" (auto-prefix).
    ///   3. Hit canonical key → return ARN.
    ///
    /// PRE-IMPLEMENTATION FAILURE MODE: assertion-fail (returns None).
    #[test]
    fn test_aip_override_for_auto_prefix_path() {
        let canonical_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-46";
        let mut map = HashMap::new();
        map.insert("claude-opus-4-6".to_string(), canonical_arn.to_string());

        let client = make_client_with_overrides(map, None);

        let result = client.aip_override_for("opus-4-6");
        assert_eq!(
            result,
            Some(canonical_arn),
            "aip_override_for(\"opus-4-6\") must find the \"claude-opus-4-6\" override \
             via auto-prefix canonicalization, got {:?}",
            result
        );
    }

    // ── AC5.3 — Canonicalizer rejects → no fallback ───────────────────────────

    /// AC5.3: `aip_override_for("Sonnet 4.7")` against any map returns `None`.
    ///
    /// "Sonnet 4.7" contains a space, which is outside `[a-zA-Z0-9.:-]`, so
    /// `canonicalize_model_id` returns `None`. There must be no further fallback;
    /// the function must immediately return `None` without fabricating a match.
    ///
    /// PRE-IMPLEMENTATION FAILURE MODE: the current raw-HashMap-get implementation
    /// already returns None for this input, so the test passes before Task 5.
    /// It is a regression guard ensuring the canonical-fallback code path added in
    /// Task 5 does NOT accidentally over-reach for inputs the canonicalizer rejects.
    #[test]
    fn test_aip_override_for_canonicalizer_rejection_returns_none() {
        let mut map = HashMap::new();
        map.insert(
            "claude-sonnet-4-7".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-47"
                .to_string(),
        );

        let client = make_client_with_overrides(map, None);

        let result = client.aip_override_for("Sonnet 4.7");
        assert!(
            result.is_none(),
            "aip_override_for(\"Sonnet 4.7\") must return None because the canonicalizer \
             rejects inputs containing spaces; no fallback path must remain, got {:?}",
            result
        );
    }

    // ── AC5.4 — Raw match takes precedence over canonical match ───────────────

    /// AC5.4: Given map `{"claude-sonnet-4-6-20250514" -> "raw-arn",
    /// "claude-sonnet-4-6" -> "canonical-arn"}` and input
    /// `"claude-sonnet-4-6-20250514"`, returns `"raw-arn"` (NOT `"canonical-arn"`).
    ///
    /// The raw match fast-path must be attempted first and must short-circuit
    /// before the canonicalize-then-lookup step.
    ///
    /// PRE-IMPLEMENTATION FAILURE MODE: the current raw HashMap::get implementation
    /// already passes this test. It is a regression guard ensuring that adding the
    /// canonical fallback in Task 5 does NOT break raw-match precedence.
    #[test]
    fn test_aip_override_for_raw_takes_precedence_over_canonical() {
        let raw_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/raw-dated";
        let canonical_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/canonical";

        let mut map = HashMap::new();
        // Both the dated key AND the canonical key are present.
        map.insert(
            "claude-sonnet-4-6-20250514".to_string(),
            raw_arn.to_string(),
        );
        map.insert("claude-sonnet-4-6".to_string(), canonical_arn.to_string());

        let client = make_client_with_overrides(map, None);

        let result = client.aip_override_for("claude-sonnet-4-6-20250514");
        assert_eq!(
            result,
            Some(raw_arn),
            "raw-match must take precedence: input \"claude-sonnet-4-6-20250514\" \
             must return the raw-keyed ARN, not the canonical-keyed ARN; got {:?}",
            result
        );
    }
}

// ── AC5.7 — Startup normalization pass (tracing output capture) ───────────────
//
// These tests cover `EndpointPool::scan_non_canonical_aip_overrides`, a
// startup diagnostic that scans all rows in `endpoint_aip_overrides` and logs
// any whose `model_id` is not a canonical fixed-point (i.e.
// `canonicalize_model_id(model_id) != Some(model_id)`).  Rows with a
// canonical sibling on the same endpoint are logged at WARN; others at INFO.
// The scan never modifies or deletes rows.
//
// The unit-level test module below is intentionally empty because the behavior
// is covered end-to-end by `tests/integration/canonicalize_admin_tests.rs`
// (AC5.7-integration), which runs against a real DB.
#[cfg(test)]
mod tests_aip_normalization_startup {
    // Intentionally empty until the Builder exposes
    // `EndpointPool::scan_non_canonical_aip_overrides`.
    // The integration-level AC5.7 test in
    // `tests/integration/canonicalize_admin_tests.rs` covers the behavior
    // end-to-end against a real DB.
}

// ── Health loop union — Task 3 (Slice 2A) tests ──────────────────────────────
//
// These tests exercise `compute_health_state`, a pure-ish async function the
// builder will add to this module. The function encapsulates the unified
// health-loop body from spec §Slice 2 Health loop changes:
//
//   pub async fn compute_health_state<F, Fut>(
//       cri_list_result: Result<Vec<String>, String>,
//       aip_overrides: &HashMap<String, String>,
//       get_foundation_model: F,
//       routing_prefix: &str,
//       legacy_inference_profile_arn: Option<&str>,
//   ) -> (bool, Vec<String>)
//   where
//       F: Fn(String) -> Fut,
//       Fut: Future<Output = Result<String, String>>,
//
// Parameters:
//   cri_list_result   — The Ok/Err outcome of `ListInferenceProfiles` filtered to
//                       this endpoint's routing prefix. `Ok(vec)` carries the
//                       profile IDs already filtered to `<prefix>.` form (e.g.
//                       `["us.anthropic.claude-haiku-4-5", ...]`).
//   aip_overrides     — The endpoint's `aip_overrides` HashMap (model_id → AIP ARN).
//                       From `EndpointClient.aip_overrides`.
//   get_foundation_model — Async callback that takes an AIP ARN (`String`) and
//                       returns `Ok(foundation_model_arn)` or `Err(reason)`. In
//                       production this calls `GetInferenceProfile`; in tests it
//                       is a simple closure.
//   routing_prefix    — The endpoint's routing prefix (e.g. `"us"`). Used to
//                       construct the `<prefix>.<bedrock_suffix>` model ID for
//                       each resolved AIP foundation model.
//   legacy_inference_profile_arn — `client.config.inference_profile_arn.as_deref()`.
//                       Non-None only for endpoints that haven't completed the
//                       auto-migration to the new table. When non-None AND
//                       `aip_overrides` is empty, the function falls back to the
//                       legacy single-ARN path (validate via get_foundation_model,
//                       populate available_models with the resolved foundation model).
//
// Returns:
//   (healthy: bool, available_models: Vec<String>)
//
//   - `healthy` is true iff CRI list succeeded AND every AIP override resolved.
//   - `available_models` is the deduplicated union of CRI profiles + AIP-resolved
//     `<routing_prefix>.<bedrock_suffix>` model IDs. On any failure, the function
//     returns `(false, <empty-or-partial-vec>)` — callers must not rely on
//     partial content when healthy=false.
//
// None of these tests require a database connection or AWS credentials.
// The `get_foundation_model` callback is a sync-wrapped async closure.

// ── tests_health_loop_union ─────────────────────────────────────────────────
//
// These tests exercise `compute_health_state`.  After Task 5 (Slice 2C), the
// function returns a `HealthState` struct instead of a bare tuple:
//
//   pub struct HealthState {
//       pub healthy: bool,
//       pub available_models: Vec<String>,
//       pub aip_derived_profile_ids: Vec<String>,
//   }
//
// All tests in this block destructure `HealthState` using the struct field
// syntax.  Tests 1-8 are the original Task 3 tests updated to the new return
// type; tests 9-10 are new Task 5 additions.

#[cfg(test)]
mod tests_health_loop_union {
    use super::*;
    use std::collections::HashMap;

    // ── Test 1: CRI + two AIP overrides → healthy union ───────────────────────

    /// An endpoint with a successful CRI list and two healthy AIP overrides
    /// must produce healthy=true and available_models = dedup'd union of CRI
    /// profiles + AIP-resolved foundation models.
    ///
    /// CRI list:  ["us.anthropic.claude-haiku-4-5", "us.anthropic.claude-sonnet-4-5"]
    /// AIP overrides:
    ///   "claude-sonnet-4-5" → sonnet AIP ARN (foundation model resolves to same
    ///                          Bedrock suffix as the CRI entry → dedup)
    ///   "claude-opus-4-7"   → opus   AIP ARN (foundation model resolves to new entry)
    ///
    /// Expected available_models (order-insensitive):
    ///   ["us.anthropic.claude-haiku-4-5",
    ///    "us.anthropic.claude-sonnet-4-5",
    ///    "us.anthropic.claude-opus-4-7"]
    #[tokio::test]
    async fn test_health_cri_plus_two_aip_overrides_healthy_union() {
        let cri_result = Ok(vec![
            "us.anthropic.claude-haiku-4-5".to_string(),
            "us.anthropic.claude-sonnet-4-5".to_string(),
        ]);

        let mut aip_overrides = HashMap::new();
        aip_overrides.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged"
                .to_string(),
        );
        aip_overrides.insert(
            "claude-opus-4-7".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-tagged"
                .to_string(),
        );

        // Mock callback: each AIP ARN resolves to a foundation-model ARN whose
        // tail is the Bedrock model suffix (without the routing prefix).
        let get_fm = |arn: String| async move {
            if arn.contains("sonnet-tagged") {
                // Resolves to same suffix as the CRI entry — will be deduped.
                Ok("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"
                    .to_string())
            } else if arn.contains("opus-tagged") {
                Ok(
                    "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-opus-4-7"
                        .to_string(),
                )
            } else {
                Err(format!("unexpected ARN in test: {arn}"))
            }
        };

        let HealthState {
            healthy,
            mut available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(
            cri_result,
            &aip_overrides,
            get_fm,
            "us",
            None, // no legacy ARN
        )
        .await;

        assert!(
            healthy,
            "endpoint must be healthy when CRI list succeeded and all AIP overrides resolved"
        );

        available_models.sort();
        assert_eq!(
            available_models.len(),
            3,
            "available_models must contain exactly 3 entries (Haiku + Sonnet + Opus, \
             with Sonnet deduped from CRI and AIP); got: {available_models:?}"
        );
        assert!(
            available_models.contains(&"us.anthropic.claude-haiku-4-5".to_string()),
            "available_models must include the Haiku CRI entry; got: {available_models:?}"
        );
        assert!(
            available_models
                .iter()
                .any(|m| m.contains("claude-sonnet-4-5")),
            "available_models must include a Sonnet entry; got: {available_models:?}"
        );
        assert!(
            available_models
                .iter()
                .any(|m| m.contains("claude-opus-4-7")),
            "available_models must include an Opus entry resolved from the AIP; \
             got: {available_models:?}"
        );
    }

    // ── Test 2: One failing AIP override → healthy=false ──────────────────────

    /// When one AIP override returns an error from get_foundation_model, the
    /// endpoint must be marked unhealthy and available_models must NOT be
    /// advanced (to avoid advertising models that are unreachable).
    ///
    /// CRI list succeeds, but the Opus AIP call fails.
    #[tokio::test]
    async fn test_health_one_failing_aip_override_marks_unhealthy() {
        let cri_result = Ok(vec![
            "us.anthropic.claude-haiku-4-5".to_string(),
            "us.anthropic.claude-sonnet-4-5".to_string(),
        ]);

        let mut aip_overrides = HashMap::new();
        aip_overrides.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-ok"
                .to_string(),
        );
        aip_overrides.insert(
            "claude-opus-4-7".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-fail"
                .to_string(),
        );

        let get_fm = |arn: String| async move {
            if arn.contains("sonnet-ok") {
                Ok("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"
                    .to_string())
            } else {
                // Opus AIP call fails — simulates GetInferenceProfile returning an error.
                Err("GetInferenceProfile failed: ResourceNotFoundException".to_string())
            }
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(cri_result, &aip_overrides, get_fm, "us", None).await;

        assert!(
            !healthy,
            "endpoint must be unhealthy when any AIP override fails to resolve; \
             got healthy=true with available_models={available_models:?}"
        );
        // available_models must NOT be advanced when the endpoint is unhealthy.
        assert!(
            available_models.is_empty(),
            "available_models must be empty when the endpoint is unhealthy \
             (spec: 'available_models not advanced'); got: {available_models:?}"
        );
    }

    // ── Test 3: CRI-only endpoint (zero AIP overrides) → unchanged behavior ───

    /// Regression guard: an endpoint with no AIP overrides must behave exactly
    /// as today — available_models is the CRI list, healthy iff CRI succeeded.
    #[tokio::test]
    async fn test_health_cri_only_no_aip_overrides_regression() {
        let cri_result = Ok(vec![
            "us.anthropic.claude-haiku-4-5".to_string(),
            "us.anthropic.claude-sonnet-4-5".to_string(),
            "us.anthropic.claude-opus-4-7".to_string(),
        ]);

        // No AIP overrides — get_foundation_model should never be called.
        let get_fm = |arn: String| async move {
            panic!(
                "get_foundation_model must NOT be called when aip_overrides is empty; \
                 called with ARN: {arn}"
            );
            #[allow(unreachable_code)]
            Err(String::new())
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(
            cri_result,
            &HashMap::new(), // empty overrides
            get_fm,
            "us",
            None,
        )
        .await;

        assert!(
            healthy,
            "CRI-only endpoint must be healthy when CRI list succeeded"
        );
        let mut available_models = available_models;
        available_models.sort();
        assert_eq!(
            available_models,
            vec![
                "us.anthropic.claude-haiku-4-5",
                "us.anthropic.claude-opus-4-7",
                "us.anthropic.claude-sonnet-4-5",
            ],
            "available_models must be exactly the CRI list for a CRI-only endpoint"
        );
    }

    /// Regression guard: CRI failure on a CRI-only endpoint marks it unhealthy.
    #[tokio::test]
    async fn test_health_cri_only_failure_marks_unhealthy() {
        let cri_result: Result<Vec<String>, String> =
            Err("ListInferenceProfiles failed: AccessDeniedException".to_string());

        let get_fm = |arn: String| async move {
            panic!(
                "get_foundation_model must NOT be called when aip_overrides is empty; \
                 called with ARN: {arn}"
            );
            #[allow(unreachable_code)]
            Err(String::new())
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(cri_result, &HashMap::new(), get_fm, "us", None).await;

        assert!(
            !healthy,
            "CRI-only endpoint must be unhealthy when CRI list fails"
        );
        assert!(
            available_models.is_empty(),
            "available_models must be empty when CRI list fails; got: {available_models:?}"
        );
    }

    // ── Test 4: Legacy inference_profile_arn (no new table) → today's behavior ─

    /// Auto-migration safety net: when `aip_overrides` is empty AND the legacy
    /// `inference_profile_arn` column is set, the function must fall back to the
    /// legacy single-ARN path:
    ///   - Call get_foundation_model with the legacy ARN.
    ///   - If it succeeds, populate available_models with the resolved
    ///     `<routing_prefix>.<bedrock_suffix>` model ID.
    ///   - healthy = success of the get_foundation_model call.
    ///
    /// This branch fires only when the auto-migration has NOT yet run for this
    /// endpoint (empty new table + legacy column still set).
    #[tokio::test]
    async fn test_health_legacy_arn_no_new_table_preserves_today_behavior() {
        // CRI list succeeds (it's always called)
        let cri_result = Ok(vec![]);

        let legacy_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/legacy-sonnet";

        let get_fm = |arn: String| async move {
            assert_eq!(
                arn, legacy_arn,
                "get_foundation_model must be called with the legacy ARN"
            );
            // Resolves to a Sonnet foundation model ARN.
            Ok("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"
                .to_string())
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(
            cri_result,
            &HashMap::new(), // empty new table
            get_fm,
            "us",
            Some(legacy_arn), // legacy column is set
        )
        .await;

        assert!(
            healthy,
            "endpoint must be healthy when the legacy ARN resolves successfully"
        );
        assert_eq!(
            available_models.len(),
            1,
            "available_models must have exactly one entry (the legacy AIP's foundation model); \
             got: {available_models:?}"
        );
        assert!(
            available_models[0].contains("claude-sonnet-4-5"),
            "available_models entry must represent the resolved foundation model in \
             '<routing_prefix>.<bedrock_suffix>' form; got: {}",
            available_models[0]
        );
        assert!(
            available_models[0].starts_with("us."),
            "available_models entry must start with the routing prefix 'us.'; \
             got: {}",
            available_models[0]
        );
    }

    /// Legacy path failure: when the legacy ARN's get_foundation_model call fails,
    /// the endpoint must be unhealthy and available_models must be empty.
    #[tokio::test]
    async fn test_health_legacy_arn_failure_marks_unhealthy() {
        let cri_result = Ok(vec![]);

        let legacy_arn =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/legacy-broken";

        let get_fm = |arn: String| async move {
            assert!(arn.contains("legacy-broken"), "unexpected ARN: {arn}");
            Err("GetInferenceProfile failed: ResourceNotFoundException".to_string())
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(cri_result, &HashMap::new(), get_fm, "us", Some(legacy_arn)).await;

        assert!(
            !healthy,
            "endpoint must be unhealthy when the legacy ARN fails to resolve"
        );
        assert!(
            available_models.is_empty(),
            "available_models must be empty when legacy ARN resolution fails; \
             got: {available_models:?}"
        );
    }

    // ── Test 5: Dedup — AIP foundation model already in CRI list ──────────────

    /// When an AIP override resolves to a foundation model whose
    /// `<routing_prefix>.<bedrock_suffix>` form is already present in the CRI list,
    /// the model must appear exactly once in available_models.
    ///
    /// This guards against the double-emit scenario where both the CRI branch and
    /// the AIP branch independently contribute the same Sonnet entry.
    #[tokio::test]
    async fn test_health_dedup_aip_foundation_model_coincides_with_cri() {
        // CRI list already contains the Sonnet entry.
        let cri_result = Ok(vec![
            "us.anthropic.claude-haiku-4-5".to_string(),
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
        ]);

        let mut aip_overrides = HashMap::new();
        // This AIP resolves to the SAME Bedrock suffix as the CRI Sonnet entry.
        aip_overrides.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-dup"
                .to_string(),
        );

        let get_fm = |_arn: String| async move {
            // Resolves to the same foundation model as the CRI "sonnet" entry.
            Ok("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"
                .to_string())
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(cri_result, &aip_overrides, get_fm, "us", None).await;

        assert!(healthy, "endpoint must be healthy");

        let sonnet_count = available_models
            .iter()
            .filter(|m| m.contains("claude-sonnet-4-5"))
            .count();
        assert_eq!(
            sonnet_count, 1,
            "Sonnet must appear exactly once in available_models after dedup; \
             got {sonnet_count} occurrences in: {available_models:?}"
        );
        assert_eq!(
            available_models.len(),
            2,
            "available_models must have exactly 2 entries (Haiku + Sonnet deduped); \
             got: {available_models:?}"
        );
    }

    // ── Test 6: CRI failure with AIP overrides → unhealthy regardless of AIP ───

    /// If the CRI ListInferenceProfiles call fails, the endpoint is unhealthy
    /// even if all AIP overrides would have resolved successfully.
    /// (Health rule: CRI AND every AIP override must succeed.)
    #[tokio::test]
    async fn test_health_cri_failure_with_aip_overrides_unhealthy() {
        let cri_result: Result<Vec<String>, String> =
            Err("ListInferenceProfiles failed: ThrottlingException".to_string());

        let mut aip_overrides = HashMap::new();
        aip_overrides.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-ok"
                .to_string(),
        );

        let get_fm = |_arn: String| async move {
            Ok("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"
                .to_string())
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids: _,
        } = compute_health_state(cri_result, &aip_overrides, get_fm, "us", None).await;

        assert!(
            !healthy,
            "endpoint must be unhealthy when CRI list fails, regardless of AIP resolution"
        );
        assert!(
            available_models.is_empty(),
            "available_models must be empty when CRI list fails; got: {available_models:?}"
        );
    }

    // ── Test 7 (new — Task 5): aip_derived_profile_ids populated correctly ─────

    /// An endpoint with two AIP overrides and a non-overlapping CRI list.
    /// `aip_derived_profile_ids` must contain exactly the two `<prefix>.<suffix>`
    /// strings resolved from AIP overrides, NOT the CRI-only entries.
    ///
    /// CRI list: ["us.anthropic.claude-haiku-4-5"]  (Haiku — CRI only)
    /// AIP overrides:
    ///   "claude-sonnet-4-5" → sonnet-only AIP ARN  (distinct from CRI)
    ///   "claude-opus-4-7"   → opus AIP ARN          (distinct from CRI)
    ///
    /// Expected aip_derived_profile_ids contains the resolved Sonnet and Opus
    /// model IDs; Haiku must NOT appear in aip_derived_profile_ids.
    #[tokio::test]
    async fn test_health_state_aip_derived_profile_ids_populated() {
        let cri_result = Ok(vec!["us.anthropic.claude-haiku-4-5".to_string()]);

        let mut aip_overrides = HashMap::new();
        aip_overrides.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-aip"
                .to_string(),
        );
        aip_overrides.insert(
            "claude-opus-4-7".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-aip"
                .to_string(),
        );

        let get_fm = |arn: String| async move {
            if arn.contains("sonnet-aip") {
                Ok("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"
                    .to_string())
            } else if arn.contains("opus-aip") {
                Ok(
                    "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-opus-4-7"
                        .to_string(),
                )
            } else {
                Err(format!("unexpected ARN: {arn}"))
            }
        };

        let HealthState {
            healthy,
            available_models,
            mut aip_derived_profile_ids,
        } = compute_health_state(cri_result, &aip_overrides, get_fm, "us", None).await;

        assert!(healthy, "endpoint must be healthy");
        assert_eq!(
            available_models.len(),
            3,
            "available_models must have 3 entries (Haiku + Sonnet + Opus); \
             got: {available_models:?}"
        );

        // The AIP-derived set must contain exactly the two AIP-resolved entries.
        aip_derived_profile_ids.sort();
        assert_eq!(
            aip_derived_profile_ids.len(),
            2,
            "aip_derived_profile_ids must contain exactly 2 entries (Sonnet + Opus); \
             got: {aip_derived_profile_ids:?}"
        );
        assert!(
            aip_derived_profile_ids
                .iter()
                .any(|id| id.contains("claude-sonnet-4-5")),
            "aip_derived_profile_ids must include the AIP-resolved Sonnet entry; \
             got: {aip_derived_profile_ids:?}"
        );
        assert!(
            aip_derived_profile_ids
                .iter()
                .any(|id| id.contains("claude-opus-4-7")),
            "aip_derived_profile_ids must include the AIP-resolved Opus entry; \
             got: {aip_derived_profile_ids:?}"
        );
        // Haiku is CRI-only and must NOT appear in aip_derived_profile_ids.
        assert!(
            !aip_derived_profile_ids
                .iter()
                .any(|id| id.contains("claude-haiku-4-5")),
            "aip_derived_profile_ids must NOT include the CRI-only Haiku entry; \
             got: {aip_derived_profile_ids:?}"
        );
    }

    // ── Test 8 (new — Task 5): overlapping AIP + CRI → CRI provenance wins ────

    /// When an AIP override resolves to a `<prefix>.<suffix>` that is already
    /// in the CRI list, the dedup logic treats it as CRI provenance.  The model
    /// must appear in `available_models` exactly once, and it must NOT appear
    /// in `aip_derived_profile_ids`.
    ///
    /// CRI list: ["us.anthropic.claude-haiku-4-5",
    ///            "us.anthropic.claude-sonnet-4-5-20250929-v1:0"]
    /// AIP override: "claude-sonnet-4-5" → ARN that resolves to the same Sonnet suffix
    ///
    /// Expected: aip_derived_profile_ids is EMPTY (overlap treated as CRI).
    #[tokio::test]
    async fn test_health_state_aip_derived_excludes_cri_dedup_overlap() {
        let cri_result = Ok(vec![
            "us.anthropic.claude-haiku-4-5".to_string(),
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
        ]);

        let mut aip_overrides = HashMap::new();
        // AIP resolves to the same suffix that is already in the CRI list.
        aip_overrides.insert(
            "claude-sonnet-4-5".to_string(),
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-overlap"
                .to_string(),
        );

        let get_fm = |_arn: String| async move {
            Ok("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0"
                .to_string())
        };

        let HealthState {
            healthy,
            available_models,
            aip_derived_profile_ids,
        } = compute_health_state(cri_result, &aip_overrides, get_fm, "us", None).await;

        assert!(healthy, "endpoint must be healthy");
        assert_eq!(
            available_models.len(),
            2,
            "available_models must have exactly 2 entries (Haiku + Sonnet deduped); \
             got: {available_models:?}"
        );

        // Because the AIP-resolved Sonnet is already covered by a CRI entry,
        // CRI provenance wins and aip_derived_profile_ids must be empty.
        assert!(
            aip_derived_profile_ids.is_empty(),
            "aip_derived_profile_ids must be empty when all AIP-resolved entries \
             are already covered by CRI; got: {aip_derived_profile_ids:?}"
        );
    }
}

// ── Capability-probe opt-out — Task 5 (Slice 2C) tests ───────────────────────
//
// These tests exercise two pure helpers added to this module:
//
//   pub fn should_probe_profile(
//       profile_id: &str,
//       aip_derived_profile_ids: &[String],
//       capability_probe_aip_enabled: bool,
//   ) -> bool
//
// and (in a separate module below):
//
//   pub fn effective_capability_probe_aip(
//       db_setting: Option<&str>,
//       env_value: Option<&str>,
//   ) -> bool
//
// Behaviour of `should_probe_profile`:
//   - flag true  → always return true.
//   - flag false, profile is in aip_derived_profile_ids → return false.
//   - flag false, profile is NOT in aip_derived_profile_ids → return true.
//
// None of these tests require a database connection or AWS credentials.

#[cfg(test)]
mod tests_capability_probe_aip {
    use super::*;

    // ── Test 1: flag=true, AIP-derived entry → probe ──────────────────────────

    /// When `capability_probe_aip_enabled` is `true`, every profile is probed
    /// regardless of whether it is AIP-derived.  This is the default path.
    #[test]
    fn test_should_probe_returns_true_when_flag_enabled_aip_entry() {
        let aip_derived = vec!["us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string()];
        let result = should_probe_profile(
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            &aip_derived,
            true, // flag enabled
        );
        assert!(
            result,
            "should_probe_profile must return true when flag is enabled, \
             even for an AIP-derived entry"
        );
    }

    // ── Test 2: flag=true, CRI-only entry → probe ─────────────────────────────

    /// When `capability_probe_aip_enabled` is `true`, CRI-only profiles are
    /// also always probed (this was the pre-Task-5 behaviour).
    #[test]
    fn test_should_probe_returns_true_when_flag_enabled_cri_entry() {
        let aip_derived: Vec<String> =
            vec!["us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string()];
        let result = should_probe_profile(
            "us.anthropic.claude-haiku-4-5", // Haiku — CRI only, not in aip_derived
            &aip_derived,
            true, // flag enabled
        );
        assert!(
            result,
            "should_probe_profile must return true for a CRI-only profile \
             when flag is enabled"
        );
    }

    // ── Test 3: flag=false, AIP-derived entry → skip ─────────────────────────

    /// When `capability_probe_aip_enabled` is `false` and the profile ID is
    /// present in `aip_derived_profile_ids`, the probe must be skipped.
    ///
    /// This is the core gate behaviour added by Task 5.
    #[test]
    fn test_should_probe_skips_aip_entry_when_flag_disabled() {
        let aip_derived = vec![
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
            "us.anthropic.claude-opus-4-7".to_string(),
        ];
        let result = should_probe_profile(
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            &aip_derived,
            false, // flag disabled
        );
        assert!(
            !result,
            "should_probe_profile must return false (skip) when flag is disabled \
             and the profile is AIP-derived"
        );
    }

    // ── Test 4: flag=false, CRI-only entry → probe ───────────────────────────

    /// When `capability_probe_aip_enabled` is `false` but the profile is NOT in
    /// `aip_derived_profile_ids`, the probe must proceed (CRI entries are always
    /// probed regardless of the flag).
    #[test]
    fn test_should_probe_runs_cri_entry_when_flag_disabled() {
        let aip_derived = vec!["us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string()];
        let result = should_probe_profile(
            "us.anthropic.claude-haiku-4-5", // CRI only — not in aip_derived
            &aip_derived,
            false, // flag disabled
        );
        assert!(
            result,
            "should_probe_profile must return true (probe) for a CRI-only profile \
             even when the capability probe AIP flag is disabled"
        );
    }

    // ── Test 5: flag=false, empty AIP set → all profiles probed ──────────────

    /// When `aip_derived_profile_ids` is empty (e.g. a pure CRI endpoint) and
    /// the flag is `false`, every profile is still probed because none match
    /// the (empty) AIP-derived set.
    #[test]
    fn test_should_probe_empty_aip_set_with_flag_disabled() {
        let result = should_probe_profile(
            "us.anthropic.claude-haiku-4-5",
            &[], // empty AIP-derived set
            false,
        );
        assert!(
            result,
            "should_probe_profile must return true when aip_derived_profile_ids \
             is empty, regardless of flag value"
        );
    }
}

// ── Effective-flag resolution — Task 5 (Slice 2C) tests ─────────────────────
//
// These tests exercise:
//
//   pub fn effective_capability_probe_aip(
//       db_setting: Option<&str>,    // value from proxy_settings.capability_probe_aip
//       env_value: Option<&str>,     // value of CAPABILITY_PROBE_AIP env var if set
//   ) -> bool
//
// Precedence:
//   1. `db_setting` (if Some) — DB wins.
//   2. `env_value`  (if Some and parseable) — env used when DB absent.
//   3. Default `true` — both absent, or env value is malformed.
//
// Parsing is case-insensitive: "TRUE", "False", "1", "0", etc. are valid.

#[cfg(test)]
mod tests_effective_capability_probe_flag {
    use super::*;

    // ── Test 8: DB setting overrides env (DB=true, env=false → true) ──────────

    /// DB setting `"true"` wins over env var `"false"`.  DB is the highest
    /// precedence source.
    #[test]
    fn test_effective_flag_db_overrides_env_true_over_false() {
        let result = effective_capability_probe_aip(
            Some("true"),  // DB setting
            Some("false"), // env var
        );
        assert!(result, "DB setting 'true' must win over env 'false'");
    }

    // ── Test 9: DB setting overrides env (DB=false, env=true → false) ─────────

    /// DB setting `"false"` wins over env var `"true"`.
    #[test]
    fn test_effective_flag_db_overrides_env_false_over_true() {
        let result = effective_capability_probe_aip(
            Some("false"), // DB setting
            Some("true"),  // env var
        );
        assert!(!result, "DB setting 'false' must win over env 'true'");
    }

    // ── Test 10: DB absent → env var used ────────────────────────────────────

    /// When the DB has no row, the env var value is used.
    #[test]
    fn test_effective_flag_env_used_when_db_absent() {
        let result = effective_capability_probe_aip(
            None,          // no DB row
            Some("false"), // env var says false
        );
        assert!(
            !result,
            "env var 'false' must be respected when no DB setting is present"
        );
    }

    // ── Test 11: both absent → default true ──────────────────────────────────

    /// When neither the DB row nor the env var is set, the effective flag is
    /// the hard-coded default: `true` (probe everything).
    #[test]
    fn test_effective_flag_default_true_when_both_absent() {
        let result = effective_capability_probe_aip(
            None, // no DB row
            None, // no env var
        );
        assert!(
            result,
            "effective_capability_probe_aip must default to true when both \
             db_setting and env_value are absent"
        );
    }

    // ── Test 12: malformed env var → fall back to default ────────────────────

    /// An unrecognised env var value (e.g. `"yes"`, `"enabled"`) must fall
    /// back to the default `true` rather than panic or incorrectly parse.
    #[test]
    fn test_effective_flag_malformed_falls_back_to_default() {
        let result = effective_capability_probe_aip(
            None,        // no DB row
            Some("yes"), // unrecognised value
        );
        assert!(
            result,
            "malformed env var 'yes' must fall back to the default of true"
        );
    }

    // ── Test 13: case-insensitive parsing ────────────────────────────────────

    /// Both `"TRUE"` and `"False"` (mixed case) must parse correctly.
    #[test]
    fn test_effective_flag_case_insensitive() {
        let upper_true = effective_capability_probe_aip(Some("TRUE"), None);
        assert!(
            upper_true,
            "DB setting 'TRUE' (upper-case) must parse as true"
        );

        let mixed_false = effective_capability_probe_aip(Some("False"), None);
        assert!(
            !mixed_false,
            "DB setting 'False' (mixed-case) must parse as false"
        );
    }
}
