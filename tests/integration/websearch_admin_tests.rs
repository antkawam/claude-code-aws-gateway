use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::helpers;
use ccag::budget::BudgetSpendCache;

/// Build a test router with a real DB pool and session token auth.
async fn test_app(pool: &sqlx::PgPool) -> (axum::Router, String) {
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
        pricing_refresh_enabled: true,
    };

    let signing_key = "test-signing-key-for-integration-tests";

    // Create admin user in DB so role resolution works
    let _ = ccag::db::users::create_user(pool, "admin", None, "admin").await;

    // Issue a session token for the admin
    let identity = ccag::auth::oidc::OidcIdentity {
        sub: "admin".to_string(),
        email: None,
        idp_name: "Local".to_string(),
    };
    let admin_token = ccag::auth::session::issue(signing_key, &identity, 24);

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let bedrock_client = aws_sdk_bedrockruntime::Client::new(&aws_config);
    let bedrock_control_client = aws_sdk_bedrock::Client::new(&aws_config);

    let (metrics, _provider) = ccag::telemetry::Metrics::new(None).unwrap();
    let metrics = Arc::new(metrics);

    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));
    let state = Arc::new(ccag::proxy::GatewayState {
        bedrock_client,
        bedrock_control_client,
        model_cache: ccag::translate::models::ModelCache::new(),
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
        setup_tokens: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        http_client: reqwest::Client::new(),

        budget_cache: Arc::new(BudgetSpendCache::new(30)),
        sns_client: None,
        eb_client: None,
        quota_cache: None,
        aws_config: aws_config.clone(),
        bedrock_health: tokio::sync::RwLock::new(None),
        endpoint_pool: Arc::new(ccag::endpoint::EndpointPool::new()),
        endpoint_stats: Arc::new(ccag::endpoint::stats::EndpointStats::new()),
        started_at: std::time::Instant::now(),
        login_attempts: tokio::sync::Mutex::new(Vec::new()),
        pricing_client: std::sync::Arc::new(aws_sdk_pricing::Client::new(&aws_config)),
    });

    let router = ccag::api::router(state);
    (router, admin_token)
}

// ============================================================
// Websearch Admin Mode — GET default
// ============================================================

#[tokio::test]
async fn test_get_websearch_mode_default() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // GET /admin/websearch-mode should return the default mode ("enabled")
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // The endpoint doesn't exist yet, so we expect 404.
    // When implemented, this should return 200 with {"mode": "enabled"}.
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /admin/websearch-mode should return 200 with default mode"
    );

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["mode"], "enabled",
        "Default websearch mode should be 'enabled'"
    );
}

// ============================================================
// Websearch Admin Mode — SET disabled
// ============================================================

#[tokio::test]
async fn test_set_websearch_mode_disabled() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // PUT /admin/websearch-mode with {"mode": "disabled"}
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode": "disabled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PUT /admin/websearch-mode with 'disabled' should return 200"
    );

    // GET should now confirm the mode is "disabled"
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["mode"], "disabled",
        "Websearch mode should be 'disabled' after update"
    );
}

// ============================================================
// Websearch Admin Mode — SET global with provider config
// ============================================================

#[tokio::test]
async fn test_set_websearch_mode_global() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // PUT /admin/websearch-mode with global mode and provider config
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "mode": "global",
                        "provider": {
                            "provider_type": "tavily",
                            "api_key": "test-key",
                            "max_results": 5
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PUT /admin/websearch-mode with 'global' and provider should return 200"
    );

    // GET should confirm mode is "global" with provider info
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["mode"], "global",
        "Websearch mode should be 'global' after update"
    );
    assert_eq!(
        json["provider"]["provider_type"], "tavily",
        "Provider type should be 'tavily'"
    );
    assert_eq!(
        json["provider"]["max_results"], 5,
        "Provider max_results should be 5"
    );
}

// ============================================================
// Websearch Admin Mode — Invalid mode returns 400
// ============================================================

#[tokio::test]
async fn test_set_websearch_mode_invalid() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // PUT /admin/websearch-mode with an invalid mode value
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode": "bogus"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "PUT /admin/websearch-mode with invalid mode 'bogus' should return 400"
    );
}

// ============================================================
// Setup script includes WebSearch deny when mode is disabled
// ============================================================

#[tokio::test]
async fn test_setup_script_deny_when_disabled() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // First, set websearch mode to "disabled" via the admin API
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode": "disabled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    // Will be 404 until implemented, but the test should assert 200 (TDD red phase)
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Setting websearch mode to disabled should succeed"
    );

    // Now GET /auth/setup and check the script includes WebSearch in deny permissions
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/auth/setup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);

    // The setup script should include permissions.deny with WebSearch
    assert!(
        body_str.contains("WebSearch"),
        "Setup script should contain 'WebSearch' in deny permissions when mode is disabled. Got: {}",
        &body_str[..body_str.len().min(500)]
    );
}

// ============================================================
// Setup script does NOT deny WebSearch when mode is enabled
// ============================================================

#[tokio::test]
async fn test_setup_script_no_deny_when_enabled() {
    let pool = helpers::setup_test_db().await;
    let (app, _token) = test_app(&pool).await;

    // With default mode ("enabled"), GET /auth/setup should NOT deny WebSearch
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/auth/setup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);

    // The setup script should NOT contain permissions deny for WebSearch
    // (We check that "permissions" + "deny" + "WebSearch" are not all present together)
    let has_websearch_deny = body_str.contains("WebSearch")
        && (body_str.contains("permissions.deny") || body_str.contains("\"deny\""));

    assert!(
        !has_websearch_deny,
        "Setup script should NOT contain WebSearch deny permissions when mode is enabled (default). Got: {}",
        &body_str[..body_str.len().min(500)]
    );
}

// ============================================================
// Global provider API key is masked in GET response
// ============================================================

#[tokio::test]
async fn test_global_provider_api_key_masked() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // PUT /admin/websearch-mode with global mode and a real API key
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "mode": "global",
                        "provider": {
                            "provider_type": "tavily",
                            "api_key": "tvly-secret-key-12345",
                            "max_results": 5
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PUT global with api_key should return 200"
    );

    // GET should NOT expose the raw API key
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // The raw key should never appear in the GET response
    assert!(
        !body_str.contains("tvly-secret-key-12345"),
        "GET response must NOT contain the raw API key. Got: {}",
        body_str
    );

    // Instead, expect a has_api_key boolean indicator
    assert_eq!(
        json["provider"]["has_api_key"], true,
        "GET response should indicate has_api_key: true when a key is configured"
    );
}

// ============================================================
// Switching from global to enabled clears provider config
// ============================================================

#[tokio::test]
async fn test_mode_change_to_enabled_clears_global_provider() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Step 1: Set mode to "global" with provider config
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "mode": "global",
                        "provider": {
                            "provider_type": "tavily",
                            "api_key": "tvly-key-abc",
                            "max_results": 3
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Setting global mode with provider should succeed"
    );

    // Step 2: Switch mode to "enabled" (no provider needed)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode": "enabled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Switching to enabled mode should succeed"
    );

    // Step 3: GET should show enabled mode with NO provider field
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        json["mode"], "enabled",
        "Mode should be 'enabled' after switching from global"
    );
    assert!(
        json.get("provider").is_none() || json["provider"].is_null(),
        "Provider config should be cleared when switching from global to enabled. Got: {}",
        json
    );
}

// ============================================================
// Round 3: Wiring websearch mode into handler + global provider
// ============================================================

// Test A: Websearch mode is stored in a way the handler can consume
// (verifies the DB → settings → handler data path works end-to-end)
#[tokio::test]
async fn test_websearch_mode_available_in_state() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Set mode to "disabled"
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode": "disabled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify the mode is readable from the DB settings table directly.
    // This is the same path the handler would use to read the mode.
    let mode = ccag::db::settings::get_setting(&pool, "websearch_mode")
        .await
        .unwrap()
        .unwrap_or_else(|| "enabled".to_string());
    assert_eq!(
        mode, "disabled",
        "Handler should be able to read websearch_mode='disabled' from settings"
    );

    // Now set mode to "global" and verify the handler can read both mode and provider
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "mode": "global",
                        "provider": {
                            "provider_type": "tavily",
                            "api_key": "tvly-test-key",
                            "max_results": 5
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mode = ccag::db::settings::get_setting(&pool, "websearch_mode")
        .await
        .unwrap()
        .unwrap_or_else(|| "enabled".to_string());
    assert_eq!(mode, "global");

    let provider_json = ccag::db::settings::get_setting(&pool, "websearch_global_provider")
        .await
        .unwrap()
        .expect("Global provider config should be stored in settings");
    let provider: serde_json::Value = serde_json::from_str(&provider_json).unwrap();
    assert_eq!(provider["provider_type"], "tavily");
    assert_eq!(provider["api_key"], "tvly-test-key");
    assert_eq!(provider["max_results"], 5);
}

// Test B: Global provider config is complete and usable for SearchProvider construction
#[tokio::test]
async fn test_global_provider_config_complete() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // PUT global mode with full provider config (type, api_key, api_url, max_results)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "mode": "global",
                        "provider": {
                            "provider_type": "tavily",
                            "api_key": "tvly-full-key-99",
                            "api_url": "https://custom.tavily.com/search",
                            "max_results": 8
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PUT global with full provider config should succeed"
    );

    // GET it back and verify ALL fields needed to construct a SearchProvider are present
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Verify all fields needed for provider construction are present
    let provider = &json["provider"];
    assert_eq!(
        provider["provider_type"], "tavily",
        "provider_type must be present"
    );
    assert_eq!(
        provider["has_api_key"], true,
        "has_api_key must be true (api_key was provided)"
    );
    assert_eq!(
        provider["api_url"], "https://custom.tavily.com/search",
        "api_url must be preserved"
    );
    assert_eq!(provider["max_results"], 8, "max_results must be preserved");

    // Also verify the raw DB value can construct a SearchProvider.
    // This tests the new from_global_config method that doesn't exist yet.
    let provider_json = ccag::db::settings::get_setting(&pool, "websearch_global_provider")
        .await
        .unwrap()
        .expect("Global provider config should be in settings");
    let config_value: serde_json::Value = serde_json::from_str(&provider_json).unwrap();

    // This call will fail to compile until SearchProvider::from_global_config is implemented
    let provider = ccag::websearch::SearchProvider::from_global_config(&config_value)
        .expect("Should construct SearchProvider from stored global config");
    assert_eq!(provider.provider_name(), "tavily");
}

// ============================================================
// Round 4: Wiring websearch mode into settings endpoint + portal visibility
// ============================================================

// Test: GET /admin/settings should include websearch_mode so the portal can
// decide whether to show/hide the Web Search nav item. Currently get_settings()
// only returns virtual_keys_enabled, admin_login_enabled, and session_token_ttl_hours.
#[tokio::test]
async fn test_settings_includes_websearch_mode() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // GET /admin/settings — the endpoint the portal calls on load
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/settings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // The settings response should include websearch_mode so the portal knows
    // whether to render the Web Search nav item. Default should be "enabled".
    assert!(
        json.get("websearch_mode").is_some(),
        "GET /admin/settings must include 'websearch_mode' field for portal visibility. Got: {json}"
    );
    assert_eq!(
        json["websearch_mode"], "enabled",
        "Default websearch_mode in settings should be 'enabled'"
    );
}

// Test: After changing websearch mode, GET /admin/settings should reflect the new value.
#[tokio::test]
async fn test_settings_websearch_mode_updates() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Set websearch mode to "disabled"
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode": "disabled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PUT /admin/websearch-mode should succeed"
    );

    // GET /admin/settings should now show websearch_mode = "disabled"
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/settings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        json["websearch_mode"], "disabled",
        "GET /admin/settings should reflect updated websearch_mode='disabled'. Got: {json}"
    );
}

// Test: Websearch mode should be accessible from GatewayState's cached settings,
// not just via a DB query, so the hot request path in handlers.rs can read it
// without a DB round-trip on every request.
#[tokio::test]
async fn test_websearch_mode_cached_in_state() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Set mode to "disabled" via admin API
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"mode": "disabled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The settings endpoint is the proxy for what the handler sees.
    // If it returns websearch_mode, the state has it cached (or can read it).
    // This tests the data path: PUT admin API -> DB -> GET settings -> handler
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/settings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        json["websearch_mode"], "disabled",
        "Settings endpoint (state cache proxy) should return 'disabled' after PUT. Got: {json}"
    );

    // Now switch to "global" and verify it updates
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"mode": "global", "provider": {"provider_type": "tavily", "api_key": "k"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/settings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        json["websearch_mode"], "global",
        "Settings endpoint should return 'global' after second PUT. Got: {json}"
    );
}

// Test C: Global mode requires provider_type in the provider object (validation)
#[tokio::test]
async fn test_global_mode_requires_provider_type() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // PUT global mode with provider object that has NO provider_type field
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/websearch-mode")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "mode": "global",
                        "provider": {
                            "api_key": "some-key",
                            "max_results": 5
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Currently the API just stores whatever JSON you give it without validating
    // provider_type. This test expects 400 to enforce that the provider object
    // must have at minimum a provider_type field.
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "PUT global mode without provider_type should return 400. \
         The API must validate that provider.provider_type is present."
    );
}
