use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Instant;

use tokio::sync::RwLock;

use crate::api::cli_auth::CliSessionStore;
use crate::auth::KeyCache;
use crate::auth::oidc::MultiIdpValidator;
use crate::budget::BudgetSpendCache;
use crate::config::GatewayConfig;
use crate::endpoint::EndpointPool;
use crate::endpoint::stats::EndpointStats;

use crate::quota::QuotaCache;
use crate::ratelimit::RateLimiter;
use crate::spend::SpendTracker;
use crate::telemetry::Metrics;
use crate::translate::models::ModelCache;

/// TTL for setup tokens (5 minutes).
pub const SETUP_TOKEN_TTL_SECS: u64 = 300;

pub struct GatewayState {
    pub bedrock_client: aws_sdk_bedrockruntime::Client,
    pub bedrock_control_client: aws_sdk_bedrock::Client,
    pub model_cache: ModelCache,
    pub config: GatewayConfig,
    pub key_cache: KeyCache,
    pub rate_limiter: RateLimiter,
    pub idp_validator: Arc<MultiIdpValidator>,
    pub db_pool: Arc<RwLock<sqlx::PgPool>>,
    pub spend_tracker: Arc<SpendTracker>,
    pub metrics: Arc<Metrics>,
    pub virtual_keys_enabled: AtomicBool,
    pub admin_login_enabled: AtomicBool,
    pub cache_version: AtomicI64,
    pub session_token_ttl_hours: AtomicI64,
    /// HS256 key for signing gateway session tokens. Auto-generated and persisted to DB.
    pub session_signing_key: String,
    pub cli_sessions: CliSessionStore,
    pub http_client: reqwest::Client,

    pub budget_cache: Arc<BudgetSpendCache>,
    pub sns_client: Option<aws_sdk_sns::Client>,
    pub eb_client: Option<aws_sdk_eventbridge::Client>,
    pub quota_cache: Option<Arc<QuotaCache>>,
    pub bedrock_health: RwLock<Option<(Instant, bool)>>,
    pub endpoint_pool: Arc<EndpointPool>,
    pub endpoint_stats: Arc<EndpointStats>,
    pub aws_config: aws_config::SdkConfig,
    pub started_at: Instant,
}

impl GatewayState {
    /// Get a clone of the database pool. Cheap (Arc clone internally).
    /// When IAM auth is enabled, this pool is periodically swapped with a fresh one.
    pub async fn db(&self) -> sqlx::PgPool {
        self.db_pool.read().await.clone()
    }

    pub fn virtual_keys_enabled(&self) -> bool {
        self.virtual_keys_enabled.load(Ordering::Relaxed)
    }

    pub fn set_virtual_keys_enabled(&self, enabled: bool) {
        self.virtual_keys_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Admin login is enabled if the DB setting says so, OR if the explicit
    /// recovery flag ADMIN_PASSWORD_ENABLE=true is set. The ADMIN_PASSWORD env var
    /// alone does NOT force login on — users often forget to remove it after setup,
    /// so disabling admin login in the portal takes precedence by default.
    pub fn admin_login_enabled(&self) -> bool {
        // Explicit recovery flag: ADMIN_PASSWORD_ENABLE=true overrides everything
        if std::env::var("ADMIN_PASSWORD_ENABLE")
            .ok()
            .is_some_and(|v| v == "true" || v == "1")
        {
            return true;
        }
        // Otherwise respect the DB/portal setting
        self.admin_login_enabled.load(Ordering::Relaxed)
    }

    pub fn set_admin_login_enabled(&self, enabled: bool) {
        self.admin_login_enabled.store(enabled, Ordering::Relaxed);
    }
}
