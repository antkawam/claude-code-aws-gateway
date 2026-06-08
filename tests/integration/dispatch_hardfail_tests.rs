/// Integration tests for Section 6: Hard-fail Case 2 and select_endpoint
/// empty-fallback eradication.
///
/// Spec: .claude/specs/unified-model-canonicalization.md §6, lines 279-308
/// Task: Task 6 of unified-model-canonicalization-tasks.md
///
/// # BUILDER CONTRACT
///
/// This file requires the following public surface to be exposed by the builder.
/// Tests will produce compile errors until the builder implements these changes.
///
/// ## 1. `ModelUnavailableError` in `src/api/handlers.rs`
///
///    `pub struct ModelUnavailableError` (or `pub enum`) must be added to
///    `src/api/handlers.rs` and must implement:
///
///    ```rust
///    impl ModelUnavailableError {
///        /// Return the model name that was requested but had no override configured.
///        pub fn model_name(&self) -> &str { ... }
///    }
///    ```
///
///    The natural variant name is `NoOverrideForCanonicalModel`.  Exact name is the
///    builder's choice.
///
/// ## 2. `resolve_dispatch_target` return type change
///
///    ```rust
///    pub fn resolve_dispatch_target(
///        cri_bedrock_model: &str,
///        original_model: &str,
///        aip_override: Option<&str>,
///        has_any_aip_overrides: bool,
///        legacy_inference_profile_arn: Option<&str>,
///    ) -> Result<String, ModelUnavailableError>
///    ```
///
///    Case 2 (`has_any_aip_overrides=true && aip_override.is_none()`) must return
///    `Err(ModelUnavailableError { model: original_model.to_string() })` (or
///    equivalent).  Cases 1, 3, 4 return `Ok(arn)` as before.
///
/// ## 3. Handler-level 400 for Case 2
///
///    The call-site in `src/api/handlers.rs::messages_handler` (around line 746)
///    must map the `Err` from `resolve_dispatch_target` to a 400 response:
///
///    ```rust
///    let resolved = match resolve_dispatch_target(...) {
///        Ok(target) => target,
///        Err(e) => {
///            return build_model_unavailable_on_endpoint_error(
///                e.model_name(),
///                selected_endpoint.as_ref().map(|ep| ep.config.name.as_str()),
///            );
///        }
///    };
///    ```
///
///    The error message must contain BOTH the model name AND the endpoint name.
///    `build_model_unavailable_on_endpoint_error` may be a new helper alongside
///    the existing `build_model_unavailable_error`, or that helper may be extended
///    with an optional endpoint parameter — builder's choice.
///
/// ## 4. `select_endpoint` returns None + non-empty pool → 400
///
///    After `select_endpoint` returns `None`, the handler must check
///    `state.endpoint_pool.is_empty().await`:
///
///    - Empty pool → bootstrap path: fall through to `state.bedrock_client`
///      (existing behaviour, no change).
///    - Non-empty pool → misconfiguration: return 400 `invalid_request_error`
///      with message "no routable endpoint for your team".
///
///    `EndpointPool::is_empty()` already exists (src/endpoint/mod.rs line 698).
///
/// ## 5. `EndpointPool::insert_client_for_testing` (test-only)
///
///    To allow the integration tests below to inject a pre-built `EndpointClient`
///    into the pool without going through `load_endpoints` (which calls
///    `aws_config::from_env()` and needs real credentials to actually work in
///    the expected way for these tests), the builder must expose:
///
///    ```rust
///    #[cfg(test)]
///    pub async fn insert_client_for_testing(&self, client: crate::endpoint::EndpointClient) {
///        let mut clients = self.clients.write().await;
///        clients.insert(client.config.id, std::sync::Arc::new(client));
///    }
///    ```
///
///    This mirrors the existing private `insert_client` test helper in
///    `src/endpoint/mod.rs` but is exposed `pub` under `#[cfg(test)]` so
///    the integration test file can call it on a `&EndpointPool`.
///
/// ## 6. Response message contract for AC6.4
///
///    The 400 response body for Case 2 (has overrides, model not covered) must
///    include BOTH:
///    - The requested model name (e.g. "claude-haiku-4-5")
///    - The endpoint name (e.g. "test-ep-haiku-hardfail")
///
///    Example message:
///    "Model 'claude-haiku-4-5' is not available on endpoint 'test-ep-haiku-hardfail'; \
///     the endpoint has AIP overrides configured but none match this model."
///
/// # Test layout
///
///   AC6.4 — handler returns 400 when endpoint has overrides for Sonnet/Opus but not Haiku
///   AC6.5 — handler returns 400 when select_endpoint returns None AND pool is non-empty
///   AC6.6 — handler does NOT return 400 when pool is empty (bootstrap path preserved)
///
/// Run with: make test-integration
#[cfg(feature = "integration")]
mod dispatch_hardfail_integration {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::helpers;
    use ccag::auth::CachedKey;
    use ccag::budget::BudgetSpendCache;
    use ccag::db::schema::Endpoint;
    use ccag::endpoint::{EndpointClient, EndpointPool};
    use ccag::translate::models::{CachedMapping, ModelCache};

    // ── Constants ──────────────────────────────────────────────────────────────

    /// Canonical Haiku model ID used across tests.
    const HAIKU_MODEL: &str = "claude-haiku-4-5";
    /// Canonical Sonnet model ID used across tests.
    const SONNET_MODEL: &str = "claude-sonnet-4-5";
    /// CRI suffix for Haiku used in available_models.
    const HAIKU_SUFFIX: &str = "anthropic.claude-haiku-4-5-20251001-v1:0";
    /// CRI suffix for Sonnet used in available_models.
    const SONNET_SUFFIX: &str = "anthropic.claude-sonnet-4-5-20250929-v1:0";
    /// Valid AIP ARN for Sonnet.
    const SONNET_AIP_ARN: &str =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-test";
    /// Valid AIP ARN for Opus.
    const OPUS_AIP_ARN: &str =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-test";

    // ── Shared setup helpers ───────────────────────────────────────────────────

    /// Build a minimal GatewayState backed by the given pool.
    ///
    /// - `virtual_keys_enabled`: true so the `x-api-key` path is active.
    /// - `model_cache`: pre-seeded with Haiku and Sonnet so `accept_model` never
    ///   falls through to the async `discover_fn` (which would need real AWS).
    /// - `endpoint_pool`: empty by default; tests inject clients directly via
    ///   `EndpointPool::insert_client_for_testing` (BUILDER CONTRACT §5).
    async fn test_app_with_pool(
        pool: &sqlx::PgPool,
    ) -> (axum::Router, String, Arc<ccag::proxy::GatewayState>) {
        let config = ccag::config::GatewayConfig {
            host: "127.0.0.1".to_string(),
            port: 9999,
            admin_username: "admin".to_string(),
            admin_password: "admin".to_string(),
            bedrock_routing_prefix: "us".to_string(),
            database_url: "postgres://test@localhost/test".to_string(),
            admin_users: vec![],
            notification_url: None,
            rds_iam_auth: false,
            database_host: None,
            database_port: 5432,
            database_name: "test".to_string(),
            database_user: "test".to_string(),
            pricing_refresh_interval: 86400,
            pricing_refresh_enabled: false,
        };

        let signing_key = "test-signing-key-dispatch-hardfail";

        // Seed admin user so admin token works.
        let _ = ccag::db::users::create_user(pool, "admin", None, "admin").await;

        let identity = ccag::auth::oidc::OidcIdentity {
            sub: "admin".to_string(),
            email: None,
            idp_name: "Local".to_string(),
        };
        let admin_token = ccag::auth::session::issue(signing_key, &identity, 24);

        let aws_config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .build();

        let bedrock_client = aws_sdk_bedrockruntime::Client::from_conf(
            aws_sdk_bedrockruntime::Config::builder()
                .behavior_version(aws_sdk_bedrockruntime::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );
        let bedrock_control_client = aws_sdk_bedrock::Client::from_conf(
            aws_sdk_bedrock::Config::builder()
                .behavior_version(aws_sdk_bedrock::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        );

        let (metrics, _provider) = ccag::telemetry::Metrics::new(None).unwrap();
        let metrics = Arc::new(metrics);
        let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));

        // Pre-seed model cache so accept_model never calls Bedrock's ListInferenceProfiles.
        let model_cache = ModelCache::new();
        model_cache
            .insert(CachedMapping {
                anthropic_prefix: HAIKU_MODEL.to_string(),
                bedrock_suffix: HAIKU_SUFFIX.to_string(),
                anthropic_display: None,
            })
            .await;
        model_cache
            .insert(CachedMapping {
                anthropic_prefix: SONNET_MODEL.to_string(),
                bedrock_suffix: SONNET_SUFFIX.to_string(),
                anthropic_display: None,
            })
            .await;

        let pricing_client = Arc::new(aws_sdk_pricing::Client::from_conf(
            aws_sdk_pricing::Config::builder()
                .behavior_version(aws_sdk_pricing::config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .build(),
        ));

        let state = Arc::new(ccag::proxy::GatewayState {
            bedrock_client,
            bedrock_control_client,
            model_cache,
            config,
            key_cache: ccag::auth::KeyCache::new(),
            rate_limiter: ccag::ratelimit::RateLimiter::new(),
            idp_validator: Arc::new(ccag::auth::oidc::MultiIdpValidator::new()),
            db_pool: db_pool.clone(),
            spend_tracker: Arc::new(ccag::spend::SpendTracker::new(db_pool, metrics.clone())),
            metrics,
            virtual_keys_enabled: AtomicBool::new(true),
            admin_login_enabled: AtomicBool::new(true),
            cache_version: AtomicI64::new(1),
            session_token_ttl_hours: AtomicI64::new(24),
            session_signing_key: signing_key.to_string(),
            cli_sessions: ccag::api::cli_auth::new_session_store(),
            setup_tokens: tokio::sync::RwLock::new(HashMap::new()),
            http_client: reqwest::Client::new(),
            budget_cache: Arc::new(BudgetSpendCache::new(30)),
            sns_client: None,
            eb_client: None,
            quota_cache: None,
            aws_config: aws_config.clone(),
            bedrock_health: tokio::sync::RwLock::new(None),
            endpoint_pool: Arc::new(EndpointPool::new()),
            endpoint_stats: Arc::new(ccag::endpoint::stats::EndpointStats::new()),
            started_at: std::time::Instant::now(),
            login_attempts: tokio::sync::Mutex::new(Vec::new()),
            pricing_client,
        });

        let router = ccag::api::router(Arc::clone(&state));
        (router, admin_token, state)
    }

    /// Build a stub `EndpointClient` with the given config and AIP overrides.
    ///
    /// `available_models` is pre-seeded with `models_available` (full profile IDs
    /// like "us.anthropic.claude-sonnet-4-5-20250929-v1:0").  `healthy` is set to
    /// `true` so `select_endpoint` will choose this endpoint.
    ///
    /// BUILDER CONTRACT §5: `EndpointPool::insert_client_for_testing` must exist.
    async fn make_test_client(
        ep: Endpoint,
        aip_overrides: HashMap<String, String>,
        models_available: Vec<String>,
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
            quota_cache: ccag::quota::QuotaCache::new(quota_client),
            healthy: AtomicBool::new(true),
            last_health_check: AtomicI64::new(0),
            available_models: Arc::new(tokio::sync::RwLock::new(models_available)),
            beta_capabilities: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            aip_overrides,
            aip_derived_profile_ids: tokio::sync::RwLock::new(vec![]),
        }
    }

    /// Build a minimal `Endpoint` struct with the given ID and name.
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

    /// Seed a virtual key into the `KeyCache` so that `x-api-key` auth works
    /// without needing to load from DB.
    async fn seed_key(state: &ccag::proxy::GatewayState, raw_key: &str, team_id: Option<Uuid>) {
        // We need to store the hash that check_auth will compute.
        // check_auth calls db::keys::hash_key(raw_key) internally.
        let hash = ccag::db::keys::hash_key(raw_key);
        let cached = CachedKey {
            id: Uuid::new_v4(),
            name: Some("test-key".to_string()),
            user_id: None,
            user_email: None,
            team_id,
            rate_limit_rpm: None,
        };
        state.key_cache.insert(hash, cached).await;
    }

    /// Parse the response body as JSON, consuming the response.
    async fn parse_body(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("failed to read response body");
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw": std::str::from_utf8(&bytes).unwrap_or("<binary>")}))
    }

    /// Minimal valid messages request body for the given model.
    fn messages_body(model: &str) -> String {
        json!({
            "model": model,
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "ping"}]
        })
        .to_string()
    }

    // ── AC6.4: endpoint has Sonnet/Opus overrides but not Haiku → 400 ──────────
    //
    // Scenario:
    //   - Team T1 has endpoint E1 configured.
    //   - E1 has AIP overrides for claude-sonnet-4-5 and claude-opus-4-7 (not Haiku).
    //   - A key for team T1 sends `claude-haiku-4-5`.
    //   - accept_model hits cache → CanonicalHit (no Bedrock call).
    //   - filter_by_model: E1 has Haiku in available_models → team_endpoints non-empty.
    //   - select_endpoint returns E1.
    //   - resolve_dispatch_target: has_any=true, aip_override=None → Err (Case 2 hard-fail).
    //   - Handler must return 400 with invalid_request_error naming Haiku AND E1's name.
    //
    // BUILDER CONTRACT §3: the 400 message must contain both the model name and
    // the endpoint name.
    #[tokio::test]
    async fn test_ac6_4_case2_hardfail_returns_400_with_model_and_endpoint_name() {
        let pool = helpers::setup_test_db().await;
        let (app, _admin_token, state) = test_app_with_pool(&pool).await;

        let team = helpers::create_test_team(&pool, "team-haiku-hardfail").await;
        let (raw_key, _) =
            helpers::create_test_key(&pool, Some("haiku-hardfail-key"), None, Some(team.id)).await;

        // Create an endpoint in the DB and assign it to the team.
        let ep_id = Uuid::new_v4();
        let ep_name = "test-ep-haiku-hardfail";
        let ep = make_test_endpoint(ep_id, ep_name);

        // Build the EndpointClient with Sonnet+Opus overrides (NOT Haiku).
        let mut aip_overrides = HashMap::new();
        aip_overrides.insert(SONNET_MODEL.to_string(), SONNET_AIP_ARN.to_string());
        aip_overrides.insert("claude-opus-4-7".to_string(), OPUS_AIP_ARN.to_string());

        // available_models includes Haiku so filter_by_model does NOT filter it out.
        // (We want to reach resolve_dispatch_target, not get blocked by filter_by_model.)
        let available_models = vec![format!("us.{HAIKU_SUFFIX}"), format!("us.{SONNET_SUFFIX}")];
        let client = make_test_client(ep, aip_overrides, available_models).await;

        // Inject the client into the pool (BUILDER CONTRACT §5).
        // Also create the DB-side team_endpoints row so get_team_endpoints returns E1.
        state.endpoint_pool.insert_client_for_testing(client).await;

        // Seed DB: create endpoint row + team_endpoints assignment.
        // We use direct SQL since the pool client was injected manually.
        sqlx::query(
            "INSERT INTO endpoints (id, name, region, routing_prefix, priority) \
             VALUES ($1, $2, 'us-east-1', 'us', 0)",
        )
        .bind(ep_id)
        .bind(ep_name)
        .execute(&pool)
        .await
        .expect("insert endpoint row");

        sqlx::query(
            "INSERT INTO team_endpoints (team_id, endpoint_id, priority) VALUES ($1, $2, 0)",
        )
        .bind(team.id)
        .bind(ep_id)
        .execute(&pool)
        .await
        .expect("insert team_endpoints row");

        // Seed key into cache so auth works without DB round-trip.
        seed_key(&state, &raw_key, Some(team.id)).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", &raw_key)
                    .header("content-type", "application/json")
                    .body(Body::from(messages_body(HAIKU_MODEL)))
                    .unwrap(),
            )
            .await
            .expect("request failed");

        let status = resp.status();
        let body = parse_body(resp).await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "Case 2 hard-fail: endpoint has AIP overrides for Sonnet/Opus but not Haiku \
             must return 400. Body: {body}"
        );

        let error_type = body["error"]["type"].as_str().unwrap_or("");
        assert_eq!(
            error_type, "invalid_request_error",
            "400 response must use error type 'invalid_request_error'. Body: {body}"
        );

        let message = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.to_lowercase().contains("haiku"),
            "400 message must name the requested model ('haiku'). Message: {message}"
        );
        assert!(
            message.contains(ep_name),
            "400 message must name the endpoint ('{}') so admins can identify the misconfiguration. \
             Message: {message}",
            ep_name
        );
    }

    // ── AC6.5: team has endpoints but all are filtered out → 400 ─────────────
    //
    // Scenario:
    //   - Team T2 has endpoint E2 configured.
    //   - E2's available_models does NOT include Haiku (only Sonnet).
    //   - A key for T2 requests Haiku.
    //   - filter_by_model: E2 doesn't support Haiku → filtered list is empty.
    //   - Handler must return 400 (existing path at handlers.rs line 697-699).
    //
    // This test validates the EXISTING `filter_by_model` → empty branch, which
    // already returns `build_model_unavailable_error`. It's included here to:
    //   1. Confirm the path still returns 400 after Task 6 changes.
    //   2. Serve as a baseline distinguishing "model not in available_models"
    //      (AC6.5) from "model in available_models but no AIP override" (AC6.4).
    //
    // Note: if today the filter_by_model path already returns 400, this test will
    // pass immediately (it's a regression guard). If select_endpoint returns None
    // because the ONLY endpoint is filtered out, the select_endpoint→None path
    // with non-empty pool must ALSO return 400 (AC6.5 second variant below).
    #[tokio::test]
    async fn test_ac6_5_no_routable_endpoint_for_model_returns_400() {
        let pool = helpers::setup_test_db().await;
        let (app, _admin_token, state) = test_app_with_pool(&pool).await;

        let team = helpers::create_test_team(&pool, "team-no-haiku-endpoint").await;
        let (raw_key, _) =
            helpers::create_test_key(&pool, Some("no-haiku-ep-key"), None, Some(team.id)).await;

        // Endpoint E2: only supports Sonnet (not Haiku).
        let ep_id = Uuid::new_v4();
        let ep_name = "ep-sonnet-only";
        let ep = make_test_endpoint(ep_id, ep_name);

        let available_models = vec![format!("us.{SONNET_SUFFIX}")]; // no Haiku
        let client = make_test_client(ep, HashMap::new(), available_models).await;

        state.endpoint_pool.insert_client_for_testing(client).await;

        // Seed DB endpoint + team assignment.
        sqlx::query(
            "INSERT INTO endpoints (id, name, region, routing_prefix, priority) \
             VALUES ($1, $2, 'us-east-1', 'us', 0)",
        )
        .bind(ep_id)
        .bind(ep_name)
        .execute(&pool)
        .await
        .expect("insert endpoint row");

        sqlx::query(
            "INSERT INTO team_endpoints (team_id, endpoint_id, priority) VALUES ($1, $2, 0)",
        )
        .bind(team.id)
        .bind(ep_id)
        .execute(&pool)
        .await
        .expect("insert team_endpoints row");

        seed_key(&state, &raw_key, Some(team.id)).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", &raw_key)
                    .header("content-type", "application/json")
                    .body(Body::from(messages_body(HAIKU_MODEL)))
                    .unwrap(),
            )
            .await
            .expect("request failed");

        let status = resp.status();
        let body = parse_body(resp).await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "Team has endpoints configured but none support the requested model — \
             must return 400. Body: {body}"
        );

        let error_type = body["error"]["type"].as_str().unwrap_or("");
        assert_eq!(
            error_type, "invalid_request_error",
            "400 response must use error type 'invalid_request_error'. Body: {body}"
        );
    }

    // ── AC6.5 (variant): select_endpoint returns None + non-empty pool → 400 ──
    //
    // Scenario:
    //   - Team T3 has endpoint E3 configured.
    //   - E3 is in the team_endpoints assignment AND in the pool.
    //   - E3's available_models INCLUDES Haiku (passes filter_by_model).
    //   - E3 is UNHEALTHY (healthy=false), so select_endpoint returns None.
    //   - Pool is non-empty (E3 is loaded).
    //   - Handler must return 400 (new behaviour from Task 6: "no routable endpoint
    //     for your team"). This replaces the old silent bootstrap fallback.
    //
    // BUILDER CONTRACT §4: after select_endpoint returns None, the handler must
    // call `state.endpoint_pool.is_empty().await`. If false → 400.
    #[tokio::test]
    async fn test_ac6_5_select_endpoint_returns_none_nonempty_pool_returns_400() {
        let pool = helpers::setup_test_db().await;
        let (app, _admin_token, state) = test_app_with_pool(&pool).await;

        let team = helpers::create_test_team(&pool, "team-unhealthy-endpoint").await;
        let (raw_key, _) =
            helpers::create_test_key(&pool, Some("unhealthy-ep-key"), None, Some(team.id)).await;

        // Endpoint E3: supports Haiku but is UNHEALTHY.
        let ep_id = Uuid::new_v4();
        let ep_name = "ep-unhealthy";
        let ep = make_test_endpoint(ep_id, ep_name);

        let available_models = vec![format!("us.{HAIKU_SUFFIX}")];
        let mut client = make_test_client(ep, HashMap::new(), available_models).await;
        // Mark unhealthy so select_endpoint returns None.
        client
            .healthy
            .store(false, std::sync::atomic::Ordering::Relaxed);

        state.endpoint_pool.insert_client_for_testing(client).await;

        // Seed DB endpoint + team assignment.
        sqlx::query(
            "INSERT INTO endpoints (id, name, region, routing_prefix, priority) \
             VALUES ($1, $2, 'us-east-1', 'us', 0)",
        )
        .bind(ep_id)
        .bind(ep_name)
        .execute(&pool)
        .await
        .expect("insert endpoint row");

        sqlx::query(
            "INSERT INTO team_endpoints (team_id, endpoint_id, priority) VALUES ($1, $2, 0)",
        )
        .bind(team.id)
        .bind(ep_id)
        .execute(&pool)
        .await
        .expect("insert team_endpoints row");

        seed_key(&state, &raw_key, Some(team.id)).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", &raw_key)
                    .header("content-type", "application/json")
                    .body(Body::from(messages_body(HAIKU_MODEL)))
                    .unwrap(),
            )
            .await
            .expect("request failed");

        let status = resp.status();
        let body = parse_body(resp).await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "select_endpoint returned None but pool is non-empty (unhealthy endpoint) — \
             must return 400 'no routable endpoint'. Body: {body}"
        );

        let error_type = body["error"]["type"].as_str().unwrap_or("");
        assert_eq!(
            error_type, "invalid_request_error",
            "400 response must use error type 'invalid_request_error'. Body: {body}"
        );

        let message = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.to_lowercase().contains("endpoint"),
            "400 message must mention 'endpoint'. Message: {message}"
        );
    }

    // ── AC6.6: no-team key + empty pool → bootstrap path, NOT 400 ─────────────
    //
    // Scenario:
    //   - Key K4 has no team assignment.
    //   - The endpoint pool is empty (fresh gateway, zero endpoints configured).
    //   - Request for Sonnet.
    //   - team_endpoints = [] (no team → no team endpoints lookup).
    //   - select_endpoint returns None.
    //   - Pool is empty → bootstrap path: fall through to state.bedrock_client.
    //   - The Bedrock call will fail (no real AWS creds) but the failure is a
    //     non-400 error (500 or connection error from the Bedrock SDK), NOT the
    //     new "no routable endpoint" 400.
    //
    // This test asserts:
    //   1. Status is NOT 400 (the new guard must not fire on the empty-pool path).
    //   2. The error body is NOT the "no routable endpoint" message.
    //
    // The actual Bedrock failure is acceptable — it proves the code reached the
    // Bedrock call, meaning the bootstrap path was taken.
    #[tokio::test]
    async fn test_ac6_6_empty_pool_no_team_key_reaches_bedrock_not_400() {
        let pool = helpers::setup_test_db().await;
        let (app, _admin_token, state) = test_app_with_pool(&pool).await;

        // Pool is empty — no endpoints configured.
        assert!(
            state.endpoint_pool.is_empty().await,
            "Pool must be empty for this test to be valid"
        );

        // Key with NO team.
        let (raw_key, _) = helpers::create_test_key(&pool, Some("bootstrap-key"), None, None).await;
        seed_key(&state, &raw_key, None).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", &raw_key)
                    .header("content-type", "application/json")
                    .body(Body::from(messages_body(SONNET_MODEL)))
                    .unwrap(),
            )
            .await
            .expect("request failed");

        let status = resp.status();
        let body = parse_body(resp).await;

        // Must NOT be 400 invalid_request_error from the new hard-fail.
        // The actual failure will be some form of AWS/Bedrock error (not a 400
        // from our routing logic), which proves the bootstrap path was taken.
        assert_ne!(
            status,
            StatusCode::BAD_REQUEST,
            "Empty pool + no-team key must NOT hit the 'no routable endpoint' 400 — \
             it must fall through to the bootstrap Bedrock client path. Body: {body}"
        );

        // Extra guard: the 400 we want to avoid has a specific message.
        // Even if the test infrastructure somehow returns 400 for another reason,
        // ensure it's NOT the routing guard message.
        if status == StatusCode::BAD_REQUEST {
            let message = body["error"]["message"].as_str().unwrap_or("");
            assert!(
                !message.to_lowercase().contains("no routable endpoint"),
                "If a 400 is returned, it must NOT be the 'no routable endpoint' guard \
                 — that guard must only fire for non-empty pools. Message: {message}"
            );
        }
    }
}
