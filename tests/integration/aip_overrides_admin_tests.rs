/// Integration tests for the three new admin REST endpoints:
///
///   GET    /admin/endpoints/{id}/aip-overrides
///   POST   /admin/endpoints/{id}/aip-overrides
///   DELETE /admin/endpoints/{id}/aip-overrides/{model_id}
///
/// Contract points covered (numbered per Task 6 spec):
///   1. Auth:    all three endpoints without admin auth → 401/403
///   2. 404:     nonexistent endpoint id for all three verbs
///   3. POST happy path + round-trip via GET
///   4. POST conflict:  same (endpoint_id, model_id) twice → 409
///   5. DELETE happy path + subsequent GET no longer shows the row
///   6. DELETE 404: model_id not present on this endpoint
///   7a. POST validation: missing model_id → 400
///   7b. POST validation: malformed ARN (doesn't start with `arn:aws:bedrock:`) → 400
///   8. cache_version bump on each successful write
///
/// Run with: make test-integration (requires Docker Postgres on port 5433)
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

use crate::helpers;
use ccag::budget::BudgetSpendCache;

// ── shared test app factory ────────────────────────────────────────────────────

/// Build a minimal test router backed by a real DB pool. Returns the router
/// and a valid admin session token.
///
/// Mirrors the pattern in `admin_tests.rs::test_app`.
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
        pricing_refresh_enabled: false,
    };

    let signing_key = "test-signing-key-aip-overrides";

    // Seed the admin user so `resolve_oidc_role` resolves "admin".
    let _ = ccag::db::users::create_user(pool, "admin", None, "admin").await;

    let identity = ccag::auth::oidc::OidcIdentity {
        sub: "admin".to_string(),
        email: None,
        idp_name: "Local".to_string(),
    };
    let admin_token = ccag::auth::session::issue(signing_key, &identity, 24);

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let (metrics, _provider) = ccag::telemetry::Metrics::new(None).unwrap();
    let metrics = Arc::new(metrics);
    let db_pool = Arc::new(tokio::sync::RwLock::new(pool.clone()));

    let state = Arc::new(ccag::proxy::GatewayState {
        bedrock_client: aws_sdk_bedrockruntime::Client::new(&aws_config),
        bedrock_control_client: aws_sdk_bedrock::Client::new(&aws_config),
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
        pricing_client: Arc::new(aws_sdk_pricing::Client::new(&aws_config)),
    });

    (ccag::api::router(state), admin_token)
}

/// Create an endpoint via the DB directly and return its UUID string.
async fn create_endpoint(pool: &sqlx::PgPool, name: &str) -> String {
    ccag::db::endpoints::create_endpoint(pool, name, None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap_or_else(|e| panic!("create_endpoint({name}) failed: {e}"))
        .id
        .to_string()
}

/// Read the current `cache_version` counter from the DB.
async fn get_cache_version(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM cache_version")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Parse a response body as JSON.
async fn parse_body(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("failed to read response body");
    serde_json::from_slice(&bytes).expect("response body is not valid JSON")
}

/// A valid AIP ARN used throughout the tests.
const VALID_ARN: &str =
    "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged";

/// A valid AIP ARN for a second model (Opus).
const VALID_ARN_OPUS: &str =
    "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-tagged";

// ── Contract point 1: Auth ────────────────────────────────────────────────────

/// GET /admin/endpoints/{id}/aip-overrides without auth → 401
#[tokio::test]
async fn aip_overrides_get_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "auth-test-get").await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "GET without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// POST /admin/endpoints/{id}/aip-overrides without auth → 401
#[tokio::test]
async fn aip_overrides_post_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "auth-test-post").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"claude-sonnet-4-5","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "POST without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// DELETE /admin/endpoints/{id}/aip-overrides/{model_id} without auth → 401
#[tokio::test]
async fn aip_overrides_delete_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "auth-test-delete").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/admin/endpoints/{ep_id}/aip-overrides/claude-sonnet-4-5"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "DELETE without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// A non-admin (member) token should also be rejected on all three verbs.
#[tokio::test]
async fn aip_overrides_member_token_rejected() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "member-auth").await;

    // Create a member user and issue their token
    let _ = ccag::db::users::create_user(&pool, "member@test.com", None, "member").await;
    let member_identity = ccag::auth::oidc::OidcIdentity {
        sub: "member@test.com".to_string(),
        email: None,
        idp_name: "Local".to_string(),
    };
    let member_token =
        ccag::auth::session::issue("test-signing-key-aip-overrides", &member_identity, 24);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {member_token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"claude-sonnet-4-5","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "member token must be rejected with 401/403, got {}",
        resp.status()
    );
}

// ── Contract point 2: 404 on nonexistent endpoint id ─────────────────────────

/// GET /admin/endpoints/{nonexistent}/aip-overrides → 404
#[tokio::test]
async fn aip_overrides_get_nonexistent_endpoint_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let fake_id = Uuid::new_v4();

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{fake_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "GET on nonexistent endpoint id must return 404"
    );
}

/// POST /admin/endpoints/{nonexistent}/aip-overrides → 404
#[tokio::test]
async fn aip_overrides_post_nonexistent_endpoint_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let fake_id = Uuid::new_v4();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{fake_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"claude-sonnet-4-5","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "POST on nonexistent endpoint id must return 404"
    );
}

/// DELETE /admin/endpoints/{nonexistent}/aip-overrides/{model_id} → 404
#[tokio::test]
async fn aip_overrides_delete_nonexistent_endpoint_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let fake_id = Uuid::new_v4();

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/admin/endpoints/{fake_id}/aip-overrides/claude-sonnet-4-5"
                ))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "DELETE on nonexistent endpoint id must return 404"
    );
}

// ── Contract point 3: POST happy path ─────────────────────────────────────────

/// POST inserts a row; response echoes it; subsequent GET shows it.
#[tokio::test]
async fn aip_overrides_post_happy_path() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-post-happy").await;

    // POST a new override
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"claude-sonnet-4-5","aip_arn":"{VALID_ARN}","reason":"test reason"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "POST happy path must return 200 or 201, got {status}"
    );

    let body = parse_body(resp).await;
    // Response must echo the inserted row's key fields
    assert_eq!(
        body["model_id"].as_str().unwrap_or(""),
        "claude-sonnet-4-5",
        "response must echo model_id"
    );
    assert_eq!(
        body["aip_arn"].as_str().unwrap_or(""),
        VALID_ARN,
        "response must echo aip_arn"
    );

    // GET must show the inserted row
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = parse_body(resp).await;
    let overrides = body["overrides"]
        .as_array()
        .expect("GET response must have 'overrides' array");
    assert_eq!(
        overrides.len(),
        1,
        "GET must return exactly one override after POST"
    );
    assert_eq!(
        overrides[0]["model_id"].as_str().unwrap_or(""),
        "claude-sonnet-4-5"
    );
    assert_eq!(overrides[0]["aip_arn"].as_str().unwrap_or(""), VALID_ARN);
}

// ── Contract point 4: POST conflict → 409 ────────────────────────────────────

/// POSTing the same (endpoint_id, model_id) twice must return 409.
///
/// This covers the `DbError::Conflict` path (PK violation, Postgres code 23505)
/// that Task 1's CRUD layer surfaces and Task 6's handler must map to HTTP 409.
#[tokio::test]
async fn aip_overrides_post_conflict_returns_409() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-conflict").await;

    let body_str = format!(r#"{{"model_id":"claude-sonnet-4-5","aip_arn":"{VALID_ARN}"}}"#);

    // First insert succeeds
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body_str.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    let first_status = resp.status();
    assert!(
        first_status == StatusCode::CREATED || first_status == StatusCode::OK,
        "first POST must succeed, got {first_status}"
    );

    // Second insert with the same (endpoint_id, model_id) must return 409
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body_str))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "duplicate (endpoint_id, model_id) must return 409 Conflict"
    );

    // The 409 response should carry a structured error body
    let body = parse_body(resp).await;
    assert!(
        body.get("error").is_some() || body.get("type").is_some(),
        "409 response must carry a structured error body, got: {body}"
    );
}

// ── Contract point 5: DELETE happy path ──────────────────────────────────────

/// DELETE removes the override; subsequent GET no longer shows it.
#[tokio::test]
async fn aip_overrides_delete_happy_path() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-delete-happy").await;

    // Insert two overrides (Sonnet + Opus)
    for (model, arn) in [
        ("claude-sonnet-4-5", VALID_ARN),
        ("claude-opus-4-7", VALID_ARN_OPUS),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model_id":"{model}","aip_arn":"{arn}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status() == StatusCode::CREATED || resp.status() == StatusCode::OK,
            "setup POST for {model} must succeed"
        );
    }

    // DELETE the Sonnet override
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/admin/endpoints/{ep_id}/aip-overrides/claude-sonnet-4-5"
                ))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let del_status = resp.status();
    assert!(
        del_status == StatusCode::OK || del_status == StatusCode::NO_CONTENT,
        "DELETE must return 200 or 204, got {del_status}"
    );

    // GET must show only Opus (Sonnet gone)
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = parse_body(resp).await;
    let overrides = body["overrides"]
        .as_array()
        .expect("GET response must have 'overrides' array");

    assert_eq!(
        overrides.len(),
        1,
        "only one override should remain after DELETE"
    );
    assert_eq!(
        overrides[0]["model_id"].as_str().unwrap_or(""),
        "claude-opus-4-7",
        "remaining override must be Opus, not the deleted Sonnet"
    );
}

// ── Contract point 6: DELETE 404 on missing model_id ─────────────────────────

/// DELETE /admin/endpoints/{id}/aip-overrides/{model_id} when model_id is not
/// present on this endpoint → 404.
#[tokio::test]
async fn aip_overrides_delete_missing_model_returns_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-delete-missing").await;

    // No overrides inserted — DELETE of any model must be 404
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/admin/endpoints/{ep_id}/aip-overrides/claude-sonnet-4-5"
                ))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "DELETE of a model_id not present on this endpoint must return 404"
    );
}

// ── Contract point 7a: POST validation — missing model_id → 400 ──────────────

/// POST with body that omits `model_id` must return 400.
#[tokio::test]
async fn aip_overrides_post_missing_model_id_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-validation-no-model").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"aip_arn":"{VALID_ARN}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "POST without model_id must return 400"
    );
}

// ── Contract point 7b: POST validation — malformed ARN → 400 ─────────────────

/// POST with an ARN that doesn't start with `arn:aws:bedrock:` must return 400.
///
/// This is a best-effort syntactic check. Deep validation (ARN reachable,
/// AIP resolves) happens at health-loop time, not at write time.
#[tokio::test]
async fn aip_overrides_post_malformed_arn_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-validation-bad-arn").await;

    for bad_arn in [
        "not-an-arn-at-all",
        "arn:aws:iam::123456789012:role/some-role", // wrong service
        "arn:aws:bedrock",                          // too short / incomplete
        "",                                         // empty
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model_id":"claude-sonnet-4-5","aip_arn":"{bad_arn}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "POST with malformed ARN '{bad_arn}' must return 400"
        );
    }
}

// ── Contract point 8: cache_version bump on successful writes ─────────────────

/// Each successful POST bumps cache_version; each successful DELETE bumps it too.
///
/// Mirrors the pattern used by other admin write endpoints (e.g. IDP SCIM
/// admin-group update, `set_scim_admin_groups` in admin.rs which calls
/// `db::settings::bump_cache_version`).
#[tokio::test]
async fn aip_overrides_writes_bump_cache_version() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-cache-bump").await;

    let v0 = get_cache_version(&pool).await;

    // POST → cache_version must increase
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"claude-sonnet-4-5","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let post_status = resp.status();
    assert!(
        post_status == StatusCode::CREATED || post_status == StatusCode::OK,
        "POST must succeed, got {post_status}"
    );

    let v1 = get_cache_version(&pool).await;
    assert!(
        v1 > v0,
        "cache_version must be bumped after a successful POST (before={v0}, after={v1})"
    );

    // DELETE → cache_version must increase again
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/admin/endpoints/{ep_id}/aip-overrides/claude-sonnet-4-5"
                ))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let del_status = resp.status();
    assert!(
        del_status == StatusCode::OK || del_status == StatusCode::NO_CONTENT,
        "DELETE must succeed, got {del_status}"
    );

    let v2 = get_cache_version(&pool).await;
    assert!(
        v2 > v1,
        "cache_version must be bumped after a successful DELETE (before={v1}, after={v2})"
    );
}

// ── Bonus: GET returns all rows for an endpoint, multiple overrides ────────────

/// GET lists all override rows for the endpoint, including multiple models.
#[tokio::test]
async fn aip_overrides_get_lists_all_rows() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-get-all").await;

    // Insert Sonnet + Opus
    for (model, arn) in [
        ("claude-sonnet-4-5", VALID_ARN),
        ("claude-opus-4-7", VALID_ARN_OPUS),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model_id":"{model}","aip_arn":"{arn}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status() == StatusCode::CREATED || resp.status() == StatusCode::OK,
            "setup POST for {model} must succeed"
        );
    }

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = parse_body(resp).await;
    let overrides = body["overrides"]
        .as_array()
        .expect("GET response must have 'overrides' array");

    assert_eq!(
        overrides.len(),
        2,
        "GET must return both inserted overrides"
    );
    let model_ids: Vec<&str> = overrides
        .iter()
        .map(|o| o["model_id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        model_ids.contains(&"claude-sonnet-4-5"),
        "Sonnet must be in the list"
    );
    assert!(
        model_ids.contains(&"claude-opus-4-7"),
        "Opus must be in the list"
    );
}

/// GET for an endpoint with no overrides returns an empty array (not 404 or error).
#[tokio::test]
async fn aip_overrides_get_empty_is_ok() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-get-empty").await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = parse_body(resp).await;
    let overrides = body["overrides"]
        .as_array()
        .expect("GET response must have 'overrides' array");
    assert!(
        overrides.is_empty(),
        "GET for an endpoint with no overrides must return empty array"
    );
}
