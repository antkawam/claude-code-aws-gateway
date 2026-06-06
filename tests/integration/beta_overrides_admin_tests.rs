/// Integration tests for admin API handlers related to beta overrides.
///
/// The Builder must create:
///   - `src/db/beta_overrides.rs` — DB module (see beta_overrides_db_tests.rs)
///   - `src/api/admin.rs` — three new handlers:
///       `list_beta_overrides`, `upsert_beta_override`, `delete_beta_override`
///   - `src/api/mod.rs` — three new routes:
///       GET  /admin/beta-overrides
///       POST /admin/beta-overrides
///       DELETE /admin/beta-overrides/{endpoint_id}/{profile_id}/{beta_name}
///   - `src/endpoint/mod.rs` — new method `forget_capability(&self, profile, beta)` on
///     `EndpointClient` that removes the entry from `beta_capabilities`.
///
/// Design decisions documented here:
/// - DELETE a nonexistent override → **404** (not idempotent 200). Rationale:
///   the DELETE path encodes (endpoint_id, profile_id, beta_name) all in the URL,
///   making a 200 idempotent response a silent success that could mask operator
///   typos (e.g. wrong endpoint UUID). A 404 forces the caller to verify.
/// - Upsert with nonexistent endpoint_id → **404** (FK violation surfaced as
///   structured client error, not 500). The handler must catch the FK constraint
///   error from sqlx and return a 404 with `{"error": {...}}` body.
///
/// Test isolation: each test calls `helpers::setup_test_db()` which creates a
/// fresh `test_{uuid}` DB and runs all migrations. The `test_app` helper below
/// mirrors the pattern from `admin_tests.rs`.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use uuid::Uuid;

use ccag::db;
use ccag::endpoint::{EndpointClient, ProbeSource};

use crate::helpers;

// ---------------------------------------------------------------------------
// Test app bootstrap (mirrors admin_tests.rs::test_app)
// ---------------------------------------------------------------------------

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

    let _ = ccag::db::users::create_user(pool, "admin", None, "admin").await;

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
        budget_cache: Arc::new(ccag::budget::BudgetSpendCache::new(30)),
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

// ---------------------------------------------------------------------------
// Helper: create a DB endpoint and load it into the endpoint pool on `state`.
//
// Returns `(endpoint_id, Arc<EndpointClient>)` so tests can inspect the
// in-memory cache after admin API calls.
// ---------------------------------------------------------------------------

async fn create_fixture_endpoint(pool: &sqlx::PgPool, name: &str) -> Uuid {
    db::endpoints::create_endpoint(pool, name, None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap_or_else(|e| panic!("create_fixture_endpoint({name}): {e}"))
        .id
}

// ---------------------------------------------------------------------------
// Helper: extract the endpoint pool from the running app so tests can inspect
// the in-memory beta_capabilities cache directly.
//
// The `test_app` helper builds a GatewayState with an empty EndpointPool.
// To make the pool hold a real `EndpointClient` that the handler can look up,
// we call `load_endpoints` on it after the endpoint is in the DB.
// ---------------------------------------------------------------------------

async fn build_app_with_endpoint(
    pool: &sqlx::PgPool,
    ep_name: &str,
) -> (axum::Router, String, Uuid, Arc<ccag::proxy::GatewayState>) {
    use std::sync::atomic::AtomicI64;

    let ep_id = create_fixture_endpoint(pool, ep_name).await;

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
    let _ = ccag::db::users::create_user(pool, "admin", None, "admin").await;
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

    let endpoint_pool = Arc::new(ccag::endpoint::EndpointPool::new());

    // Load the endpoint into the in-memory pool so the handler can find it
    let endpoints = db::endpoints::get_enabled_endpoints(pool)
        .await
        .expect("get_enabled_endpoints");
    endpoint_pool.load_endpoints(endpoints, &aws_config).await;

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
        budget_cache: Arc::new(ccag::budget::BudgetSpendCache::new(30)),
        sns_client: None,
        eb_client: None,
        quota_cache: None,
        aws_config: aws_config.clone(),
        bedrock_health: tokio::sync::RwLock::new(None),
        endpoint_pool: endpoint_pool.clone(),
        endpoint_stats: Arc::new(ccag::endpoint::stats::EndpointStats::new()),
        started_at: std::time::Instant::now(),
        login_attempts: tokio::sync::Mutex::new(Vec::new()),
        pricing_client: std::sync::Arc::new(aws_sdk_pricing::Client::new(&aws_config)),
    });

    let router = ccag::api::router(state.clone());
    (router, admin_token, ep_id, state)
}

// ===========================================================================
// Test 9 — list_returns_all_overrides_admin_only
// ===========================================================================

#[tokio::test]
async fn list_returns_all_overrides_admin_only() {
    let pool = helpers::setup_test_db().await;
    let (app, admin_token) = test_app(&pool).await;

    let ep_id = create_fixture_endpoint(&pool, "ep-list-auth").await;

    // POST one override via the admin API
    let body = serde_json::json!({
        "endpoint_id": ep_id,
        "profile_id": "us.anthropic.claude-opus-4-7",
        "beta_name": "context-1m-2025-08-07",
        "supported": true,
        "reason": "integration test"
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/beta-overrides")
                .header("authorization", format!("Bearer {admin_token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /admin/beta-overrides should return 200"
    );

    // GET /admin/beta-overrides — should return the override
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/beta-overrides")
                .header("authorization", format!("Bearer {admin_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let resp_body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
    let overrides = json["overrides"]
        .as_array()
        .expect("response must have 'overrides' array");
    assert_eq!(overrides.len(), 1, "should return 1 override");
    assert_eq!(overrides[0]["profile_id"], "us.anthropic.claude-opus-4-7");
    assert_eq!(overrides[0]["beta_name"], "context-1m-2025-08-07");
    assert_eq!(overrides[0]["supported"], true);

    // Unauthenticated GET must return 401
    let resp_unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/beta-overrides")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_unauth.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated GET /admin/beta-overrides must be 401"
    );

    // Unauthenticated POST must return 401
    let resp_unauth_post = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/beta-overrides")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_unauth_post.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated POST /admin/beta-overrides must be 401"
    );
}

// ===========================================================================
// Test 10 — upsert_creates_db_row_and_caches_in_memory
// ===========================================================================

#[tokio::test]
async fn upsert_creates_db_row_and_caches_in_memory() {
    let pool = helpers::setup_test_db().await;
    let (app, admin_token, ep_id, state) =
        build_app_with_endpoint(&pool, "ep-upsert-cache-true").await;

    let profile = "us.anthropic.claude-opus-4-7";
    let beta = "context-1m-2025-08-07";

    let body = serde_json::json!({
        "endpoint_id": ep_id,
        "profile_id": profile,
        "beta_name": beta,
        "supported": true,
        "reason": "force-enable for test"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/beta-overrides")
                .header("authorization", format!("Bearer {admin_token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "upsert should return 200");

    // (a) DB row must exist
    let db_rows = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list_for_endpoint");
    assert_eq!(db_rows.len(), 1, "DB should have exactly one override row");
    assert!(db_rows[0].supported, "DB row must have supported=true");

    // (b) In-memory cache must reflect the override
    let client = state
        .endpoint_pool
        .get_client(ep_id)
        .await
        .expect("endpoint must be in pool");

    let cached = client.is_beta_supported(profile, beta).await;
    assert_eq!(
        cached,
        Some(true),
        "is_beta_supported must return Some(true) immediately after upsert API call"
    );

    // (c) The cache entry source must be AdminOverride
    // We verify via the public interface: AdminOverride entries must ignore TTL.
    // We can inspect source by reading beta_capabilities directly.
    {
        let map = client.beta_capabilities.read().await;
        let key = (profile.to_string(), beta.to_string());
        let entry = map
            .get(&key)
            .expect("cache entry must exist after admin upsert");
        assert_eq!(
            entry.source,
            ProbeSource::AdminOverride,
            "cache source must be AdminOverride"
        );
        assert!(entry.supported, "cache entry must be supported=true");
    }
}

// ===========================================================================
// Test 11 — upsert_with_supported_false_caches_in_memory_as_false
// ===========================================================================

#[tokio::test]
async fn upsert_with_supported_false_caches_in_memory_as_false() {
    let pool = helpers::setup_test_db().await;
    let (app, admin_token, ep_id, state) =
        build_app_with_endpoint(&pool, "ep-upsert-cache-false").await;

    let profile = "us.anthropic.claude-haiku-4-5";
    let beta = "context-1m-2025-08-07";

    let body = serde_json::json!({
        "endpoint_id": ep_id,
        "profile_id": profile,
        "beta_name": beta,
        "supported": false,
        "reason": "force-disable for test"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/beta-overrides")
                .header("authorization", format!("Bearer {admin_token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "upsert supported=false should return 200"
    );

    // DB must have supported=false
    let db_rows = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .expect("list_for_endpoint");
    assert_eq!(db_rows.len(), 1);
    assert!(!db_rows[0].supported, "DB row must have supported=false");

    // In-memory cache must return Some(false)
    let client = state
        .endpoint_pool
        .get_client(ep_id)
        .await
        .expect("endpoint must be in pool");

    let cached = client.is_beta_supported(profile, beta).await;
    assert_eq!(
        cached,
        Some(false),
        "is_beta_supported must return Some(false) for admin-forced unsupported"
    );
}

// ===========================================================================
// Test 12 — upsert_admin_override_wins_over_existing_seedprobe
// ===========================================================================

#[tokio::test]
async fn upsert_admin_override_wins_over_existing_seedprobe() {
    let pool = helpers::setup_test_db().await;
    let (app, admin_token, ep_id, state) = build_app_with_endpoint(&pool, "ep-override-wins").await;

    let profile = "us.anthropic.claude-opus-4-7";
    let beta = "context-1m-2025-08-07";

    // Pre-populate the in-memory cache with a SeedProbe entry saying supported=true
    let client = state
        .endpoint_pool
        .get_client(ep_id)
        .await
        .expect("endpoint must be in pool");
    client
        .mark_supported(profile, beta, ProbeSource::SeedProbe)
        .await;

    // Verify the seed-probe entry is there
    assert_eq!(
        client.is_beta_supported(profile, beta).await,
        Some(true),
        "sanity: SeedProbe entry should be Some(true)"
    );

    // Now POST an admin override with supported=false — override must win
    let body = serde_json::json!({
        "endpoint_id": ep_id,
        "profile_id": profile,
        "beta_name": beta,
        "supported": false,
        "reason": "override beats seed probe"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/beta-overrides")
                .header("authorization", format!("Bearer {admin_token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Admin override (supported=false) must win over the earlier SeedProbe (supported=true)
    let cached = client.is_beta_supported(profile, beta).await;
    assert_eq!(
        cached,
        Some(false),
        "AdminOverride(false) must supersede earlier SeedProbe(true)"
    );

    // Verify source is AdminOverride
    {
        let map = client.beta_capabilities.read().await;
        let key = (profile.to_string(), beta.to_string());
        let entry = map.get(&key).expect("cache entry must exist");
        assert_eq!(
            entry.source,
            ProbeSource::AdminOverride,
            "source must be AdminOverride after admin write"
        );
    }
}

// ===========================================================================
// Test 13 — delete_removes_db_row_and_in_memory_cache
// ===========================================================================

#[tokio::test]
async fn delete_removes_db_row_and_in_memory_cache() {
    let pool = helpers::setup_test_db().await;
    let (app, admin_token, ep_id, state) = build_app_with_endpoint(&pool, "ep-delete-cache").await;

    let profile = "us.anthropic.claude-opus-4-7";
    let beta = "context-1m-2025-08-07";

    // First: upsert via POST
    let upsert_body = serde_json::json!({
        "endpoint_id": ep_id,
        "profile_id": profile,
        "beta_name": beta,
        "supported": true,
        "reason": "to be deleted"
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/beta-overrides")
                .header("authorization", format!("Bearer {admin_token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&upsert_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "upsert before delete should succeed"
    );

    // Verify it's in DB and cache
    let rows = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "sanity: DB row exists before delete");

    let client = state
        .endpoint_pool
        .get_client(ep_id)
        .await
        .expect("endpoint must be in pool");
    assert_eq!(
        client.is_beta_supported(profile, beta).await,
        Some(true),
        "sanity: cache says Some(true) before delete"
    );

    // Now: DELETE via admin API
    // URL encoding: profile_id and beta_name are path segments; they may contain dots
    // (which are valid in URL paths) but the router must handle them as-is.
    let delete_uri = format!("/admin/beta-overrides/{}/{}/{}", ep_id, profile, beta);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&delete_uri)
                .header("authorization", format!("Bearer {admin_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "DELETE of existing override should return 200"
    );

    // DB row must be gone
    let rows_after = db::beta_overrides::list_for_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert_eq!(rows_after.len(), 0, "DB row must be deleted");

    // In-memory cache must return None (forget_capability removes the entry)
    let cached_after = client.is_beta_supported(profile, beta).await;
    assert_eq!(
        cached_after, None,
        "is_beta_supported must return None after the override is deleted"
    );
}

// ===========================================================================
// Test 14 — delete_404_for_nonexistent
// ===========================================================================
//
// Design decision: DELETE a nonexistent override → 404.
// See module-level doc comment for rationale.

#[tokio::test]
async fn delete_404_for_nonexistent() {
    let pool = helpers::setup_test_db().await;
    let (app, admin_token) = test_app(&pool).await;

    let ep_id = create_fixture_endpoint(&pool, "ep-delete-404").await;

    // Attempt to delete an override that was never created
    let delete_uri = format!(
        "/admin/beta-overrides/{}/us.anthropic.nonexistent/context-1m-2025-08-07",
        ep_id
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&delete_uri)
                .header("authorization", format!("Bearer {admin_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "DELETE of nonexistent override must return 404"
    );

    // Response body must be structured error JSON, not a plain string
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&body).expect("DELETE 404 response must be valid JSON");
    assert!(
        json["error"].is_object(),
        "DELETE 404 response must have 'error' object, got: {json}"
    );
}

// ===========================================================================
// Test 15 — upsert_validates_endpoint_exists
// ===========================================================================
//
// Design decision: POST with nonexistent endpoint_id → 404.
// The handler must catch the FK constraint violation from sqlx and convert it
// to a structured 404 response (not let a 500 bubble up).

#[tokio::test]
async fn upsert_validates_endpoint_exists() {
    let pool = helpers::setup_test_db().await;
    let (app, admin_token) = test_app(&pool).await;

    // Use a random UUID that has no corresponding endpoint row
    let nonexistent_ep_id = Uuid::new_v4();

    let body = serde_json::json!({
        "endpoint_id": nonexistent_ep_id,
        "profile_id": "us.anthropic.claude-opus-4-7",
        "beta_name": "context-1m-2025-08-07",
        "supported": true,
        "reason": "should fail"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/beta-overrides")
                .header("authorization", format!("Bearer {admin_token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Must NOT be 500 — FK violations must be surfaced as client errors
    assert_ne!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "FK violation on nonexistent endpoint_id must not produce a 500"
    );

    // Must be 404 (documented design decision above)
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "POST with nonexistent endpoint_id must return 404"
    );

    // Response body must be structured error JSON
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("404 response body must be valid JSON");
    assert!(
        json["error"].is_object(),
        "404 response must contain 'error' object, got: {json}"
    );
}

// ===========================================================================
// Test: forget_capability — EndpointClient method removes cache entry
//
// The Builder must add `forget_capability(&self, profile, beta)` to
// `EndpointClient`. This test validates that method directly (pure in-memory,
// no HTTP, no DB). It is kept in this file because it is a contract the admin
// DELETE handler depends on.
// ===========================================================================

#[tokio::test]
async fn forget_capability_removes_entry_from_cache() {
    use ccag::endpoint::EndpointPool;

    // Build a minimal EndpointPool with one client (no DB, no AWS calls)
    let pool = EndpointPool::new();
    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

    // We need a real Endpoint record to call create_client; use a DB-backed fixture.
    let db_pool = helpers::setup_test_db().await;
    let ep = db::endpoints::create_endpoint(
        &db_pool,
        "ep-forget",
        None,
        None,
        None,
        "us-east-1",
        "us",
        0,
    )
    .await
    .expect("create endpoint for forget_capability test");

    let endpoints = vec![ep.clone()];
    pool.load_endpoints(endpoints, &aws_config).await;

    let client = pool.get_client(ep.id).await.expect("client must be loaded");

    let profile = "us.anthropic.claude-opus-4-7";
    let beta = "context-1m-2025-08-07";

    // Pre-populate cache
    client
        .mark_supported(profile, beta, ProbeSource::AdminOverride)
        .await;
    assert_eq!(
        client.is_beta_supported(profile, beta).await,
        Some(true),
        "sanity: entry exists before forget_capability"
    );

    // forget_capability must remove the entry
    client.forget_capability(profile, beta).await;

    assert_eq!(
        client.is_beta_supported(profile, beta).await,
        None,
        "is_beta_supported must return None after forget_capability"
    );
}
