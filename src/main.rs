use ccag::api;
use ccag::auth;
use ccag::budget;
use ccag::config;
use ccag::db;

use ccag::proxy;
use ccag::ratelimit;
use ccag::spend;
use ccag::telemetry;
use ccag::translate;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use auth::oidc::{IdpConfig, MultiIdpValidator};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install default crypto provider for rustls (needed for TLS)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_format = std::env::var("LOG_FORMAT").unwrap_or_default();
    if log_format.eq_ignore_ascii_case("json") {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .init();
    }

    let mut config = config::GatewayConfig::from_env()?;
    let addr = config.listen_addr();

    tracing::info!(%addr, "Starting Claude Code AWS Gateway");

    // Warn about default credentials
    if config.admin_password == "admin" {
        tracing::warn!("Using default ADMIN_PASSWORD='admin' — set ADMIN_PASSWORD for production");
    }

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let bedrock_client = aws_sdk_bedrockruntime::Client::new(&aws_config);
    let bedrock_control_client = aws_sdk_bedrock::Client::new(&aws_config);
    let service_quotas_client = aws_sdk_servicequotas::Client::new(&aws_config);
    let quota_cache = Arc::new(ccag::quota::QuotaCache::new(service_quotas_client));
    let model_cache = translate::models::ModelCache::new();

    // Startup probe: verify Bedrock connectivity (non-blocking)
    {
        let control_client = bedrock_control_client.clone();
        tokio::spawn(async move {
            match control_client
                .list_inference_profiles()
                .max_results(1)
                .send()
                .await
            {
                Ok(_) => tracing::info!("Bedrock connectivity verified"),
                Err(e) => {
                    tracing::warn!(%e, "Bedrock connectivity check failed — gateway will start anyway")
                }
            }
        });
    }

    // Auto-detect routing prefix from AWS SDK region
    let aws_region = aws_config
        .region()
        .map(|r| r.as_ref().to_string())
        .unwrap_or_else(|| "us-east-1".to_string());
    config.bedrock_routing_prefix = config::GatewayConfig::resolve_routing_prefix(&aws_region);
    tracing::info!(
        aws_region = %aws_region,
        routing_prefix = %config.bedrock_routing_prefix,
        "Auto-detected Bedrock routing prefix"
    );

    // Telemetry (created early so spend tracker can reference metrics)
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
    let (metrics, _meter_provider) = telemetry::Metrics::new(otlp_endpoint.as_deref())?;
    let metrics = Arc::new(metrics);

    let key_cache = auth::KeyCache::new();
    let rate_limiter = ratelimit::RateLimiter::new();
    let idp_validator = Arc::new(MultiIdpValidator::new());
    let mut virtual_keys_enabled = true;
    let mut admin_login_enabled = true;
    let mut session_token_ttl_hours: i64 = 24;
    let mut cache_version: i64 = 0;
    let mut session_signing_key = generate_signing_key();
    let mut idp_configs: Vec<IdpConfig> = Vec::new();

    // Connect to database (required)
    let pool = if config.rds_iam_auth && config.database_host.is_some() {
        let host = config.database_host.as_deref().unwrap();
        db::connect_iam(
            &aws_config,
            host,
            config.database_port,
            &config.database_name,
            &config.database_user,
        )
        .await?
    } else {
        db::connect(&config.database_url).await?
    };
    db::run_migrations(&pool).await?;

    // Seed env IDP to DB (idempotent — skips if issuer already exists)
    if let Some(env_idp) = IdpConfig::from_env() {
        seed_env_idp(&pool, &env_idp).await;
    }

    // Seed bootstrap admin + ADMIN_USERS to DB
    seed_admin_users(&pool, &config).await;

    // Load keys into memory
    let count = key_cache.load_from_db(&pool).await?;
    tracing::info!(count, "Loaded virtual keys into cache");

    // Load DB IDPs
    match db::idp::get_enabled_idps(&pool).await {
        Ok(db_idps) => {
            for row in &db_idps {
                idp_configs.push(IdpConfig::from_db_row(row));
            }
            tracing::info!(count = db_idps.len(), "Loaded IDPs from database");
        }
        Err(e) => tracing::warn!(%e, "Failed to load IDPs from database"),
    }

    // Load settings
    if let Ok(Some(vk)) = db::settings::get_setting(&pool, "virtual_keys_enabled").await {
        virtual_keys_enabled = vk == "true";
    }
    if let Ok(Some(al)) = db::settings::get_setting(&pool, "admin_login_enabled").await {
        admin_login_enabled = al == "true";
    }
    if let Ok(Some(ttl)) = db::settings::get_setting(&pool, "session_token_ttl_hours").await
        && let Ok(v) = ttl.parse::<i64>()
    {
        session_token_ttl_hours = v;
    }

    // Load or persist session signing key
    match db::settings::get_setting(&pool, "session_signing_key").await {
        Ok(Some(key)) => {
            tracing::info!("Loaded session signing key from database");
            session_signing_key = key;
        }
        Ok(None) => {
            // First startup with DB: persist the generated key so all instances share it
            if let Err(e) = persist_signing_key(&pool, &session_signing_key).await {
                tracing::warn!(%e, "Failed to persist session signing key to database");
            } else {
                tracing::info!("Generated and persisted new session signing key");
            }
        }
        Err(e) => {
            tracing::warn!(%e, "Failed to load session signing key from database")
        }
    }

    if let Ok(v) = db::settings::get_cache_version(&pool).await {
        cache_version = v;
    }

    // Load model mappings into cache
    match model_cache.load_from_db(&pool).await {
        Ok(count) => tracing::info!(count, "Loaded model mappings into cache"),
        Err(e) => tracing::warn!(%e, "Failed to load model mappings from database"),
    }

    // Wrap pool in Arc<RwLock> so background tasks (spend tracker, delivery loop)
    // always read the current pool after IAM auth refreshes it.
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool));

    // Start spend tracker (metrics passed for flush error tracking)
    let spend_tracker = Arc::new(spend::SpendTracker::new(
        Arc::clone(&db_pool),
        metrics.clone(),
    ));
    spend_tracker.start_flush_loop(10);

    // Save IAM config before config is moved into state
    let iam_auth_enabled = config.rds_iam_auth;
    let iam_db_host = config.database_host.clone();
    let iam_db_port = config.database_port;
    let iam_db_name = config.database_name.clone();
    let iam_db_user = config.database_user.clone();

    // Load IDPs into validator
    let idp_count = idp_configs.len();
    idp_validator.load_idps(idp_configs).await;
    if idp_count > 0 {
        tracing::info!(count = idp_count, "OIDC authentication enabled");
        idp_validator.start_refresh_loop();
    }

    // Budget spend cache (30s TTL)
    let budget_cache = Arc::new(budget::BudgetSpendCache::new(30));

    // SNS + EventBridge clients (for app notifications — destination configured at runtime)
    let sns_client = Some(aws_sdk_sns::Client::new(&aws_config));
    let eb_client = Some(aws_sdk_eventbridge::Client::new(&aws_config));

    let endpoint_pool = Arc::new(ccag::endpoint::EndpointPool::new());
    let endpoint_stats = Arc::new(ccag::endpoint::stats::EndpointStats::new());

    let state = Arc::new(proxy::GatewayState {
        bedrock_client,
        bedrock_control_client,
        model_cache,
        config,
        key_cache,
        rate_limiter,
        idp_validator,
        db_pool: Arc::clone(&db_pool),
        spend_tracker,
        metrics,
        virtual_keys_enabled: AtomicBool::new(virtual_keys_enabled),
        admin_login_enabled: AtomicBool::new(admin_login_enabled),
        cache_version: AtomicI64::new(cache_version),
        session_token_ttl_hours: AtomicI64::new(session_token_ttl_hours),
        session_signing_key,
        cli_sessions: api::cli_auth::new_session_store(),
        setup_tokens: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        http_client: reqwest::Client::new(),

        budget_cache: budget_cache.clone(),
        sns_client: sns_client.clone(),
        eb_client: eb_client.clone(),
        quota_cache: Some(quota_cache),
        bedrock_health: tokio::sync::RwLock::new(None),
        endpoint_pool: endpoint_pool.clone(),
        endpoint_stats,
        aws_config: aws_config.clone(),
        started_at: std::time::Instant::now(),
        login_attempts: tokio::sync::Mutex::new(Vec::new()),
    });

    // Load endpoints into pool
    {
        let pool = state.db().await;
        match ccag::db::endpoints::get_enabled_endpoints(&pool).await {
            Ok(endpoints) => {
                if endpoints.is_empty() {
                    // Auto-migrate: create default endpoint from current config
                    tracing::info!("No endpoints configured, creating default from gateway config");
                    match ccag::db::endpoints::create_endpoint(
                        &pool,
                        "Default",
                        None,
                        None,
                        None,
                        &aws_region,
                        &state.config.bedrock_routing_prefix,
                        0,
                    )
                    .await
                    {
                        Ok(ep) => {
                            tracing::info!(id = %ep.id, "Created default endpoint");
                            // Mark as default so unassigned teams route to it.
                            let _ = ccag::db::endpoints::set_default_endpoint(&pool, ep.id).await;
                            // Reload after marking default so is_default is reflected in pool.
                            match ccag::db::endpoints::get_enabled_endpoints(&pool).await {
                                Ok(eps) => {
                                    state.endpoint_pool.load_endpoints(eps, &aws_config).await
                                }
                                Err(_) => {
                                    state
                                        .endpoint_pool
                                        .load_endpoints(vec![ep], &aws_config)
                                        .await
                                }
                            }
                        }
                        Err(e) => tracing::warn!(%e, "Failed to create default endpoint"),
                    }
                } else {
                    let count = endpoints.len();
                    state
                        .endpoint_pool
                        .load_endpoints(endpoints, &aws_config)
                        .await;
                    tracing::info!(count, "Loaded endpoints into pool");
                }
            }
            Err(e) => tracing::warn!(%e, "Failed to load endpoints"),
        }
    }

    // Start cache version polling loop (5s interval)
    start_cache_poll_loop(Arc::clone(&state));

    // Start IAM token refresh loop (10 min interval) if IAM auth is enabled
    if let (true, Some(host)) = (iam_auth_enabled, iam_db_host) {
        db::start_iam_refresh_loop(
            Arc::clone(&state),
            aws_config.clone(),
            host,
            iam_db_port,
            iam_db_name,
            iam_db_user,
        );
        tracing::info!("IAM database token refresh loop started (10 min interval)");
    }

    // Start Bedrock health poll loop (60s interval) — checks gateway default + all endpoints
    {
        let state_for_health = Arc::clone(&state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            let mut was_healthy = true;
            loop {
                interval.tick().await;

                // Check gateway's default Bedrock client
                let ok = state_for_health
                    .bedrock_control_client
                    .list_inference_profiles()
                    .max_results(1)
                    .send()
                    .await
                    .is_ok();

                if ok != was_healthy {
                    if ok {
                        tracing::info!("Bedrock health restored");
                    } else {
                        tracing::warn!("Bedrock health check failed");
                    }
                    was_healthy = ok;
                }

                let mut cached = state_for_health.bedrock_health.write().await;
                *cached = Some((std::time::Instant::now(), ok));
                drop(cached);

                // Check all endpoint clients
                let clients = state_for_health.endpoint_pool.get_all_clients().await;
                for client in &clients {
                    let ep_ok = if let Some(arn) = &client.config.inference_profile_arn {
                        // For application inference profile endpoints, validate the specific ARN
                        client
                            .control_client
                            .get_inference_profile()
                            .inference_profile_identifier(arn)
                            .send()
                            .await
                            .is_ok()
                    } else {
                        // For standard CRI endpoints, check credentials/region reachability
                        client
                            .control_client
                            .list_inference_profiles()
                            .max_results(1)
                            .send()
                            .await
                            .is_ok()
                    };

                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    client
                        .last_health_check
                        .store(now_secs, std::sync::atomic::Ordering::Relaxed);

                    let was_ep_healthy = client.healthy.load(std::sync::atomic::Ordering::Relaxed);
                    if ep_ok != was_ep_healthy {
                        if ep_ok {
                            tracing::info!(endpoint = %client.config.name, "Endpoint health restored");
                            ccag::endpoint::EndpointPool::mark_healthy(client);
                            // Pre-warm quota cache in the background on first healthy check.
                            let client_arc = std::sync::Arc::clone(client);
                            tokio::spawn(async move {
                                tracing::debug!(endpoint = %client_arc.config.name, "Pre-warming quota cache");
                                if let Err(e) = client_arc.quota_cache.get_bedrock_quotas().await {
                                    tracing::warn!(endpoint = %client_arc.config.name, %e, "Quota cache pre-warm failed");
                                }
                            });
                        } else {
                            tracing::warn!(endpoint = %client.config.name, "Endpoint health check failed");
                            ccag::endpoint::EndpointPool::mark_unhealthy(client);
                        }
                    }
                }
            }
        });
    }

    // Start budget notification delivery loop
    {
        let notif_pool = state.db_pool.clone();
        let notif_http = budget::notifications::delivery_http_client();
        let notif_url = state.config.notification_url.clone();
        let notif_sns = state.sns_client.clone();
        let notif_eb = state.eb_client.clone();
        tokio::spawn(async move {
            budget::notifications::delivery_loop(
                notif_pool, notif_http, notif_url, notif_sns, notif_eb,
            )
            .await;
        });
        tracing::info!("Budget notification delivery loop started");
    }

    let app = api::router(state);

    // Start HTTPS listener alongside HTTP
    let tls_port = std::env::var("TLS_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok());
    let should_tls =
        tls_port.is_some() || std::env::var("TLS_CERT").is_ok() || addr.ip().is_loopback();

    if should_tls {
        // Default to 443 for local dev (OIDC redirect flows often require standard HTTPS port)
        let tls_port = tls_port.unwrap_or(443);
        let tls_addr = std::net::SocketAddr::new(addr.ip(), tls_port);

        match build_rustls_config() {
            Ok(rustls_config) => {
                let tls_app = app.clone();
                tokio::spawn(async move {
                    tracing::info!(%tls_addr, "HTTPS listener ready (self-signed)");
                    if let Err(e) = axum_server::bind_rustls(
                        tls_addr,
                        axum_server::tls_rustls::RustlsConfig::from_config(rustls_config),
                    )
                    .serve(tls_app.into_make_service())
                    .await
                    {
                        tracing::warn!(%e, %tls_addr, "HTTPS listener failed — port 443 requires sudo for local dev");
                    }
                });
            }
            Err(e) => tracing::warn!(%e, "Failed to configure TLS — HTTPS disabled"),
        }
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "CCAG listening");
    axum::serve(listener, app).await?;

    Ok(())
}

fn build_rustls_config() -> anyhow::Result<Arc<rustls::ServerConfig>> {
    // Check for user-provided cert/key
    if let (Ok(cert_path), Ok(key_path)) = (std::env::var("TLS_CERT"), std::env::var("TLS_KEY")) {
        let cert_pem = std::fs::read(&cert_path)?;
        let key_pem = std::fs::read(&key_path)?;
        let certs: Vec<_> =
            rustls_pemfile::certs(&mut &cert_pem[..]).collect::<Result<Vec<_>, _>>()?;
        let key = rustls_pemfile::private_key(&mut &key_pem[..])?
            .ok_or_else(|| anyhow::anyhow!("No private key found in {}", key_path))?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        return Ok(Arc::new(config));
    }

    // Generate self-signed cert for local dev (cached to disk so keychain trust persists)
    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("ccag");
    let cert_path = data_dir.join("dev-cert.pem");
    let key_path = data_dir.join("dev-key.pem");

    // Reuse existing cert if it exists
    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            "Using cached self-signed TLS certificate from {}",
            cert_path.display()
        );
        // Ensure cert is trusted in keychain (idempotent, no-op if already trusted)
        trust_cert_in_keychain(&cert_path);
        let cert_pem = std::fs::read(&cert_path)?;
        let key_pem = std::fs::read(&key_path)?;
        let certs: Vec<rustls::pki_types::CertificateDer> =
            rustls_pemfile::certs(&mut &cert_pem[..])
                .filter_map(|c| c.ok())
                .collect();
        let key = rustls_pemfile::private_key(&mut &key_pem[..])?
            .ok_or_else(|| anyhow::anyhow!("No private key in dev-key.pem"))?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        return Ok(Arc::new(config));
    }

    tracing::info!("Generating self-signed TLS certificate for local dev");
    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let key_pair = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(subject_alt_names)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Claude Code AWS Gateway (dev)");
    let cert = params.self_signed(&key_pair)?;

    // Save to disk for reuse
    std::fs::create_dir_all(&data_dir)?;
    std::fs::write(&cert_path, cert.pem())?;
    std::fs::write(&key_path, key_pair.serialize_pem())?;
    tracing::info!("Saved dev certificate to {}", cert_path.display());

    // Auto-trust in macOS keychain (requires sudo, which we already need for port 443)
    trust_cert_in_keychain(&cert_path);

    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der())
        .map_err(|e| anyhow::anyhow!("Failed to serialize key: {}", e))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    Ok(Arc::new(config))
}

/// Trust a self-signed certificate in the macOS keychain for SSL.
/// Idempotent — uses a marker file to skip re-trusting unchanged certs.
fn trust_cert_in_keychain(cert_path: &std::path::Path) {
    if cfg!(not(target_os = "macos")) {
        return;
    }

    // Use a marker file to track if we've already trusted this cert.
    // security verify-cert doesn't work for self-signed certs, so we
    // compare cert modification time against our marker instead.
    let marker_path = cert_path.with_extension("trusted");
    if marker_path.exists()
        && let (Ok(cert_meta), Ok(marker_meta)) = (
            std::fs::metadata(cert_path),
            std::fs::metadata(&marker_path),
        )
        && let (Ok(cert_mod), Ok(marker_mod)) = (cert_meta.modified(), marker_meta.modified())
        && marker_mod >= cert_mod
    {
        tracing::debug!("Dev certificate already trusted (marker file present)");
        return;
    }

    // Try user login keychain first (no sudo needed), with explicit SSL policy
    tracing::info!("Trusting dev certificate for SSL in login keychain...");
    let result = std::process::Command::new("security")
        .args([
            "add-trusted-cert",
            "-p",
            "ssl",
            "-r",
            "trustRoot",
            &cert_path.to_string_lossy(),
        ])
        .status();

    match result {
        Ok(status) if status.success() => {
            tracing::info!(
                "Dev certificate trusted in login keychain (curl/browsers will accept it)"
            );
            let _ = std::fs::write(&marker_path, "");
        }
        Ok(_) => {
            // Fall back to system keychain (requires sudo)
            tracing::info!("Trying system keychain (requires sudo)...");
            let result = std::process::Command::new("security")
                .args([
                    "add-trusted-cert",
                    "-d",
                    "-p",
                    "ssl",
                    "-r",
                    "trustRoot",
                    "-k",
                    "/Library/Keychains/System.keychain",
                    &cert_path.to_string_lossy(),
                ])
                .status();

            match result {
                Ok(status) if status.success() => {
                    tracing::info!("Dev certificate trusted in system keychain");
                    let _ = std::fs::write(&marker_path, "");
                }
                _ => {
                    tracing::warn!(
                        "Could not auto-trust dev cert. Run manually:\n  \
                         security add-trusted-cert -p ssl -r trustRoot \"{}\"",
                        cert_path.display()
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(%e, "Failed to run security command");
        }
    }
}

/// Generate a random 256-bit signing key (hex-encoded).
fn generate_signing_key() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    hex::encode(bytes)
}

/// Persist signing key to DB without bumping cache version (internal, not user-facing).
async fn persist_signing_key(pool: &sqlx::PgPool, key: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO proxy_settings (key, value, updated_at)
           VALUES ('session_signing_key', $1, now())
           ON CONFLICT (key) DO UPDATE SET value = $1, updated_at = now()"#,
    )
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Seed env IDP (OIDC_ISSUER) to DB. Idempotent — skips if issuer already exists.
/// Once seeded, the IDP is fully editable via the admin portal.
async fn seed_env_idp(pool: &sqlx::PgPool, env_idp: &auth::oidc::IdpConfig) {
    // Check if any IDP with this issuer already exists
    let existing = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM identity_providers WHERE issuer_url = $1",
    )
    .bind(&env_idp.issuer)
    .fetch_one(pool)
    .await;

    if let Ok((count,)) = existing
        && count > 0
    {
        tracing::debug!(issuer = %env_idp.issuer, "Env IDP already seeded to DB, skipping");
        return;
    }

    match db::idp::create_idp(
        pool,
        &env_idp.name,
        &env_idp.issuer,
        None,
        env_idp.audience.as_deref(),
        env_idp.jwks_url.as_deref(),
        "implicit",
        env_idp.auto_provision,
        &env_idp.default_role,
        None,
        env_idp.user_claim.as_deref(),
        env_idp.scopes.as_deref(),
    )
    .await
    {
        Ok(idp) => {
            tracing::info!(id = %idp.id, name = %idp.name, issuer = %idp.issuer_url, "Seeded env IDP to database")
        }
        Err(e) => tracing::warn!(%e, "Failed to seed env IDP to database"),
    }
}

/// Seed bootstrap admin and ADMIN_USERS to DB. Idempotent — skips existing users.
async fn seed_admin_users(pool: &sqlx::PgPool, config: &config::GatewayConfig) {
    // Seed bootstrap admin
    if let Ok(None) = db::users::get_user_by_email(pool, &config.admin_username).await {
        match db::users::create_user(pool, &config.admin_username, None, "admin").await {
            Ok(_) => tracing::info!(user = %config.admin_username, "Seeded bootstrap admin user"),
            Err(e) => tracing::warn!(%e, "Failed to seed bootstrap admin user"),
        }
    }

    // Seed ADMIN_USERS
    for admin_sub in &config.admin_users {
        if admin_sub == &config.admin_username {
            continue; // Already seeded above
        }
        if let Ok(None) = db::users::get_user_by_email(pool, admin_sub).await {
            match db::users::create_user(pool, admin_sub, None, "admin").await {
                Ok(_) => tracing::info!(user = %admin_sub, "Seeded admin user from ADMIN_USERS"),
                Err(e) => tracing::warn!(%e, user = %admin_sub, "Failed to seed admin user"),
            }
        }
    }
}

/// Poll the cache_version table every 5 seconds. If it changed, reload caches.
fn start_cache_poll_loop(state: Arc<proxy::GatewayState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let pool = state.db().await;
            let pool = &pool;

            let new_version = match db::settings::get_cache_version(pool).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(%e, "Failed to poll cache version");
                    continue;
                }
            };

            let current = state.cache_version.load(Ordering::Relaxed);
            if new_version > current {
                tracing::info!(
                    old = current,
                    new = new_version,
                    "Cache version changed, reloading"
                );
                state.cache_version.store(new_version, Ordering::Relaxed);

                // Reload virtual keys
                match state.key_cache.load_from_db(pool).await {
                    Ok(count) => tracing::debug!(count, "Reloaded key cache"),
                    Err(e) => tracing::warn!(%e, "Failed to reload key cache"),
                }

                // Reload IDPs (all from DB — env IDP was seeded at startup)
                let mut idp_configs: Vec<IdpConfig> = Vec::new();
                if let Ok(db_idps) = db::idp::get_enabled_idps(pool).await {
                    for row in &db_idps {
                        idp_configs.push(IdpConfig::from_db_row(row));
                    }
                }
                state.idp_validator.load_idps(idp_configs).await;

                // Reload model mappings
                match state.model_cache.load_from_db(pool).await {
                    Ok(count) => tracing::debug!(count, "Reloaded model mappings"),
                    Err(e) => tracing::warn!(%e, "Failed to reload model mappings"),
                }

                // Reload endpoints
                match db::endpoints::get_enabled_endpoints(pool).await {
                    Ok(endpoints) => {
                        let count = endpoints.len();
                        state
                            .endpoint_pool
                            .load_endpoints(endpoints, &state.aws_config)
                            .await;
                        tracing::debug!(count, "Reloaded endpoints");
                    }
                    Err(e) => tracing::warn!(%e, "Failed to reload endpoints"),
                }

                // Reload settings
                if let Ok(Some(vk)) = db::settings::get_setting(pool, "virtual_keys_enabled").await
                {
                    state.set_virtual_keys_enabled(vk == "true");
                }
                if let Ok(Some(al)) = db::settings::get_setting(pool, "admin_login_enabled").await {
                    state.set_admin_login_enabled(al == "true");
                }
                if let Ok(Some(ttl)) =
                    db::settings::get_setting(pool, "session_token_ttl_hours").await
                    && let Ok(v) = ttl.parse::<i64>()
                {
                    state.session_token_ttl_hours.store(v, Ordering::Relaxed);
                }
            }

            // Rate limiter cleanup (every poll cycle, regardless of cache version)
            state.rate_limiter.cleanup().await;

            // Endpoint affinity cleanup
            state.endpoint_pool.cleanup_affinity().await;

            // Endpoint stats cleanup (evict >1h buckets)
            state.endpoint_stats.cleanup().await;
        }
    });
}
