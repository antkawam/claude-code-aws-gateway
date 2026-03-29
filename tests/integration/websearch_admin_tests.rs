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
