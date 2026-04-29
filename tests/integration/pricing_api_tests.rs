/// Integration tests for the admin pricing API endpoints and related dashboard
/// field additions.
///
/// Tests exercise a full HTTP stack (tower::ServiceExt::oneshot) with a real
/// test database.  The `test_app` helper is copied from admin_tests.rs and kept
/// local so this file is self-contained.
///
/// # What is tested
///
/// Endpoints (not yet implemented — all route-level tests will fail with 404
/// until @builder lands the handlers):
///   - GET  /admin/pricing
///   - PUT  /admin/pricing/:prefix
///   - DELETE /admin/pricing/:prefix
///   - POST /admin/pricing/refresh
///
/// Field additions (tests will fail with missing-key assertions until @builder
/// adds the fields):
///   - GET /admin/analytics  → `unpriced_rows`, `unpriced_models`
///   - GET /admin/budget/status → `unpriced_tokens`
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use uuid::Uuid;

use crate::helpers;
use ccag::budget::BudgetSpendCache;

// ---------------------------------------------------------------------------
// Local test-app factory (mirrors admin_tests.rs::test_app)
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

// ---------------------------------------------------------------------------
// Helper: read response body as JSON
// ---------------------------------------------------------------------------

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "Response body is not valid JSON: {e}\nbody: {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

// ---------------------------------------------------------------------------
// Test 1: Auth — all four pricing endpoints return 401 without credentials
// ---------------------------------------------------------------------------

/// All four pricing endpoints must reject unauthenticated requests with 401.
/// This test will pass as soon as the endpoints exist; it does NOT require
/// correct business logic.
#[tokio::test]
async fn pricing_endpoints_require_admin_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let endpoints: &[(&str, &str)] = &[
        ("GET", "/admin/pricing"),
        ("PUT", "/admin/pricing/claude-opus-4-7"),
        ("DELETE", "/admin/pricing/claude-opus-4-7"),
        ("POST", "/admin/pricing/refresh"),
    ];

    for (method, uri) in endpoints {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(*method)
                    .uri(*uri)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must return 401 without auth, got {}",
            resp.status()
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: GET /admin/pricing returns seed rows
// ---------------------------------------------------------------------------

/// Authenticated GET /admin/pricing must return an array containing at least
/// the 7 seed rows seeded by migration 010, sorted by model_prefix ASC.
#[tokio::test]
async fn get_pricing_returns_seed_rows() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/pricing")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /admin/pricing must return 200"
    );

    let json = body_json(resp).await;

    // Response is an array (not wrapped in an object key)
    let rows = json
        .as_array()
        .expect("GET /admin/pricing must return a JSON array");

    assert!(
        rows.len() >= 7,
        "Must return at least 7 seed rows, got {}",
        rows.len()
    );

    // Verify ascending sort by model_prefix
    let prefixes: Vec<&str> = rows
        .iter()
        .map(|r| {
            r["model_prefix"]
                .as_str()
                .expect("model_prefix must be a string")
        })
        .collect();

    for i in 0..prefixes.len() - 1 {
        assert!(
            prefixes[i] <= prefixes[i + 1],
            "Rows must be sorted ASC by model_prefix: '{}' > '{}'",
            prefixes[i],
            prefixes[i + 1]
        );
    }

    // All seed rows must be present
    for expected_prefix in &[
        "claude-haiku-4-5",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
    ] {
        assert!(
            prefixes.contains(expected_prefix),
            "Seed prefix '{}' must appear in GET /admin/pricing response",
            expected_prefix
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: GET /admin/pricing — JSON shape of each row
// ---------------------------------------------------------------------------

/// Each row in the GET /admin/pricing response must contain the expected keys
/// with the correct value types.
#[tokio::test]
async fn get_pricing_json_shape() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/pricing")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let rows = json.as_array().expect("Must be an array");
    assert!(
        !rows.is_empty(),
        "Must have at least one row to check shape"
    );

    // Use the first seed row for shape verification
    let row = &rows[0];

    assert!(
        row["model_prefix"].is_string(),
        "model_prefix must be a string; got: {:?}",
        row["model_prefix"]
    );
    assert!(
        row["input_rate"].is_number(),
        "input_rate must be a number; got: {:?}",
        row["input_rate"]
    );
    assert!(
        row["output_rate"].is_number(),
        "output_rate must be a number; got: {:?}",
        row["output_rate"]
    );
    assert!(
        row["cache_read_rate"].is_number(),
        "cache_read_rate must be a number; got: {:?}",
        row["cache_read_rate"]
    );
    assert!(
        row["cache_write_rate"].is_number(),
        "cache_write_rate must be a number; got: {:?}",
        row["cache_write_rate"]
    );
    assert!(
        row["source"].is_string(),
        "source must be a string; got: {:?}",
        row["source"]
    );
    // aws_sku must be present (even if null)
    assert!(
        row.get("aws_sku").is_some(),
        "aws_sku key must be present in each row"
    );
    // Seed rows have no SKU
    assert!(
        row["aws_sku"].is_null(),
        "Seed rows must have null aws_sku; got: {:?}",
        row["aws_sku"]
    );
    assert!(
        row["updated_at"].is_string(),
        "updated_at must be an ISO-8601 string; got: {:?}",
        row["updated_at"]
    );
}

// ---------------------------------------------------------------------------
// Test 4: PUT /admin/pricing/:prefix — override seed row
// ---------------------------------------------------------------------------

/// PUT /admin/pricing/claude-opus-4-7 with custom rates must:
///   - Return 200 with the upserted row
///   - Set source='admin_manual' regardless of what was in the DB
///   - Subsequent GET must reflect the new rates
#[tokio::test]
async fn put_pricing_overrides_seed() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let body = serde_json::json!({
        "input_rate": 7.77,
        "output_rate": 38.85,
        "cache_read_rate": 0.777,
        "cache_write_rate": 9.7125
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/pricing/claude-opus-4-7")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PUT /admin/pricing/claude-opus-4-7 must return 200"
    );

    let put_json = body_json(resp).await;
    assert_eq!(
        put_json["source"], "admin_manual",
        "PUT must force source='admin_manual', got: {:?}",
        put_json["source"]
    );
    assert_eq!(
        put_json["model_prefix"], "claude-opus-4-7",
        "PUT must echo back the model_prefix"
    );

    // Verify via GET /admin/pricing that the override persists
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/pricing")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let get_json = body_json(resp).await;
    let rows = get_json.as_array().expect("Must be an array");
    let overridden = rows
        .iter()
        .find(|r| r["model_prefix"] == "claude-opus-4-7")
        .expect("claude-opus-4-7 must still appear in the list after override");

    assert_eq!(
        overridden["source"], "admin_manual",
        "GET must reflect source='admin_manual' after PUT override"
    );
    let stored_input = overridden["input_rate"].as_f64().unwrap_or(-1.0);
    assert!(
        (stored_input - 7.77).abs() < 1e-6,
        "input_rate must be 7.77 after PUT, got {stored_input}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: PUT /admin/pricing/:prefix — create new prefix
// ---------------------------------------------------------------------------

/// PUT on a prefix that does not exist in the DB must create the row and return
/// 200 with source='admin_manual'.
#[tokio::test]
async fn put_pricing_creates_new_prefix() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let unique_prefix = format!("claude-test-new-{}", Uuid::new_v4().simple());
    let body = serde_json::json!({
        "input_rate": 1.23,
        "output_rate": 6.15,
        "cache_read_rate": 0.123,
        "cache_write_rate": 1.5375
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/pricing/{unique_prefix}"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "PUT /admin/pricing/{unique_prefix} must return 200"
    );

    let put_json = body_json(resp).await;
    assert_eq!(
        put_json["source"], "admin_manual",
        "New row must have source='admin_manual'"
    );
    assert_eq!(
        put_json["model_prefix"],
        unique_prefix.as_str(),
        "Response must echo back the new prefix"
    );

    // Verify it appears in GET
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/pricing")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let get_json = body_json(resp).await;
    let rows = get_json.as_array().expect("Must be an array");
    assert!(
        rows.iter()
            .any(|r| r["model_prefix"] == unique_prefix.as_str()),
        "New prefix '{unique_prefix}' must appear in GET /admin/pricing"
    );

    // Cleanup: delete the test row so we don't pollute other tests
    // (each test has its own isolated DB, but be explicit)
    sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&unique_prefix)
        .execute(&pool)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Test 6: PUT /admin/pricing/:prefix — missing required rate fields returns 400
// ---------------------------------------------------------------------------

/// PUT with missing required rate fields (e.g. no input_rate) must return 400.
#[tokio::test]
async fn put_pricing_requires_all_rate_fields() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Missing input_rate
    let body = serde_json::json!({
        "output_rate": 6.15,
        "cache_read_rate": 0.123,
        "cache_write_rate": 1.5375
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/pricing/claude-test-incomplete")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "PUT with missing input_rate must return 400"
    );
}

// ---------------------------------------------------------------------------
// Test 7: DELETE /admin/pricing/:prefix — removes existing row
// ---------------------------------------------------------------------------

/// DELETE on an existing prefix must return 200 `{"deleted": true}`.
/// A subsequent GET must not include the deleted prefix.
#[tokio::test]
async fn delete_pricing_removes_row() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Insert a row to delete so we don't clobber the seeds
    let prefix = format!("claude-delete-me-{}", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO model_pricing \
         (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source) \
         VALUES ($1, 1.0, 5.0, 0.1, 1.25, 'admin_manual')",
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("setup: insert row to delete must succeed");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/admin/pricing/{prefix}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "DELETE /admin/pricing/{prefix} must return 200"
    );

    let del_json = body_json(resp).await;
    assert_eq!(
        del_json["deleted"], true,
        "DELETE response must be {{\"deleted\": true}}, got: {del_json}"
    );

    // Verify the row is gone from GET
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/pricing")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let get_json = body_json(resp).await;
    let rows = get_json.as_array().expect("Must be an array");
    assert!(
        !rows.iter().any(|r| r["model_prefix"] == prefix.as_str()),
        "Deleted prefix '{prefix}' must not appear in GET /admin/pricing"
    );
}

// ---------------------------------------------------------------------------
// Test 8: DELETE /admin/pricing/:prefix — unknown prefix returns 404
// ---------------------------------------------------------------------------

/// DELETE on a prefix that does not exist must return 404.
///
/// Convention choice: 404 is more REST-ful than 200 `{"deleted": false}`.
/// The implementation must return 404 for missing rows.
///
/// NOTE: Before the DELETE route is registered this test passes "by accident"
/// because axum returns 404 for any unknown route.  After @builder wires up
/// the route the 404 must come from the handler checking the DB (rows_affected=0),
/// not from routing.  The assertion is correct in both cases.
#[tokio::test]
async fn delete_pricing_unknown_returns_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let nonexistent = format!("claude-does-not-exist-{}", Uuid::new_v4().simple());

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/admin/pricing/{nonexistent}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "DELETE of nonexistent prefix must return 404, got {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// Test 9: POST /admin/pricing/refresh — requires admin auth
// ---------------------------------------------------------------------------

/// POST /admin/pricing/refresh must reject unauthenticated requests with 401.
#[tokio::test]
async fn refresh_endpoint_admin_auth_required() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/pricing/refresh")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /admin/pricing/refresh must return 401 without auth"
    );
}

// ---------------------------------------------------------------------------
// Test 10: POST /admin/pricing/refresh — returns RefreshReport shape
// ---------------------------------------------------------------------------

/// POST /admin/pricing/refresh with valid admin auth must return 200 and a
/// JSON body with the RefreshReport shape:
///   { inserted: u32, updated: u32, unchanged: u32, skipped_manual: u32, errors: [...] }
///
/// In offline CI (no real AWS), the endpoint will either:
///   (a) return a report with `errors` populated (graceful failure), or
///   (b) return zeros if the implementation is mocked.
///
/// Either outcome is acceptable — what matters is:
///   1. Status is 200 (never 500)
///   2. The response has the expected JSON keys with numeric values
///
/// NOTE: This test is marked #[ignore] because it attempts AWS I/O.
/// Run it with `cargo test -- --ignored` against a mock or real AWS environment.
/// @builder: add a mock hook to GatewayState (e.g. a function pointer or trait
/// object) so this can be tested without live AWS.
#[tokio::test]
#[ignore = "hits AWS pricing API; run with --ignored in an environment with AWS credentials or a mock"]
async fn refresh_endpoint_returns_report_json() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/pricing/refresh")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /admin/pricing/refresh must never return 500; errors go in the report body"
    );

    let json = body_json(resp).await;

    // Verify all expected fields exist and have the right types
    assert!(
        json["inserted"].is_number(),
        "RefreshReport must have 'inserted' as a number; got: {:?}",
        json["inserted"]
    );
    assert!(
        json["updated"].is_number(),
        "RefreshReport must have 'updated' as a number; got: {:?}",
        json["updated"]
    );
    assert!(
        json["unchanged"].is_number(),
        "RefreshReport must have 'unchanged' as a number; got: {:?}",
        json["unchanged"]
    );
    assert!(
        json["skipped_manual"].is_number(),
        "RefreshReport must have 'skipped_manual' as a number; got: {:?}",
        json["skipped_manual"]
    );
    assert!(
        json["errors"].is_array(),
        "RefreshReport must have 'errors' as an array; got: {:?}",
        json["errors"]
    );
}

// ---------------------------------------------------------------------------
// Test 11: GET /admin/analytics — unpriced_rows=0 when all models are known
// ---------------------------------------------------------------------------

/// With no spend_log entries, the analytics endpoint must return unpriced_rows=0
/// and unpriced_models=[].
#[tokio::test]
async fn dashboard_shows_zero_unpriced_when_all_known() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // No spend_log entries — everything is clean
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/analytics?days=7")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /admin/analytics must return 200"
    );

    let json = body_json(resp).await;

    assert!(
        json.get("unpriced_rows").is_some(),
        "GET /admin/analytics must include 'unpriced_rows' field; got keys: {:?}",
        json.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
    assert_eq!(
        json["unpriced_rows"].as_i64().unwrap_or(-1),
        0,
        "unpriced_rows must be 0 when spend_log is empty"
    );

    assert!(
        json.get("unpriced_models").is_some(),
        "GET /admin/analytics must include 'unpriced_models' field"
    );
    let models = json["unpriced_models"]
        .as_array()
        .expect("unpriced_models must be an array");
    assert!(
        models.is_empty(),
        "unpriced_models must be empty when spend_log is empty; got: {models:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 12: GET /admin/analytics — unpriced_rows > 0 for unknown model
// ---------------------------------------------------------------------------

/// With spend_log entries for an unknown model within the last 7 days,
/// unpriced_rows must be > 0 and unpriced_models must contain the unknown model.
#[tokio::test]
async fn dashboard_shows_unpriced_rows_and_models() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let unknown_model = "claude-future-model-9999";

    // Insert 2 spend_log entries for the unknown model
    let entry1 = helpers::make_spend_entry(unknown_model, Some("tester@test.com"));
    let entry2 = helpers::make_spend_entry(unknown_model, Some("tester@test.com"));
    ccag::db::spend::insert_batch(&pool, &[entry1, entry2])
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/analytics?days=7")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;

    assert!(
        json.get("unpriced_rows").is_some(),
        "GET /admin/analytics must include 'unpriced_rows'"
    );
    let unpriced_rows = json["unpriced_rows"].as_i64().unwrap_or(-1);
    assert!(
        unpriced_rows >= 2,
        "unpriced_rows must be >= 2 (we inserted 2 rows with unknown model), got {unpriced_rows}"
    );

    assert!(
        json.get("unpriced_models").is_some(),
        "GET /admin/analytics must include 'unpriced_models'"
    );
    let models = json["unpriced_models"]
        .as_array()
        .expect("unpriced_models must be an array");
    let model_strings: Vec<&str> = models.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        model_strings.contains(&unknown_model),
        "unpriced_models must contain '{unknown_model}'; got: {model_strings:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 13: GET /admin/budget/status — unpriced_tokens=0 with known models
// ---------------------------------------------------------------------------

/// With no spend_log entries for the current user, unpriced_tokens must be 0.
#[tokio::test]
async fn budget_response_includes_unpriced_tokens_zero() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Insert a spend entry for a known model (should not count as unpriced)
    let known_entry = helpers::make_spend_entry("claude-opus-4-7", Some("admin"));
    ccag::db::spend::insert_batch(&pool, &[known_entry])
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/budget/status")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /admin/budget/status must return 200"
    );

    let json = body_json(resp).await;

    assert!(
        json.get("unpriced_tokens").is_some(),
        "GET /admin/budget/status must include 'unpriced_tokens' field; got keys: {:?}",
        json.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );

    let unpriced_tokens = json["unpriced_tokens"].as_i64().unwrap_or(-1);
    assert_eq!(
        unpriced_tokens, 0,
        "unpriced_tokens must be 0 when all spend_log entries have known models; got {unpriced_tokens}"
    );
}

// ---------------------------------------------------------------------------
// Test 14: GET /admin/budget/status — unpriced_tokens > 0 for unknown model
// ---------------------------------------------------------------------------

/// With spend_log entries for an unknown model in the budget period,
/// unpriced_tokens must equal the sum of all 4 token dimensions for those rows.
#[tokio::test]
async fn budget_response_includes_unpriced_tokens_nonzero() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let unknown_model = "claude-future-model-unpriced-99";

    // make_spend_entry creates entries with:
    //   input_tokens=500, output_tokens=200, cache_read_tokens=0, cache_write_tokens=0
    // Total per entry: 700 tokens
    let entry1 = helpers::make_spend_entry(unknown_model, Some("admin"));
    let entry2 = helpers::make_spend_entry(unknown_model, Some("admin"));
    ccag::db::spend::insert_batch(&pool, &[entry1, entry2])
        .await
        .unwrap();

    // Expected: 2 entries * (500 + 200 + 0 + 0) = 1400 tokens
    let expected_unpriced_tokens: i64 = 2 * (500 + 200);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/budget/status")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;

    assert!(
        json.get("unpriced_tokens").is_some(),
        "GET /admin/budget/status must include 'unpriced_tokens'"
    );

    let unpriced_tokens = json["unpriced_tokens"].as_i64().unwrap_or(-1);
    assert_eq!(
        unpriced_tokens, expected_unpriced_tokens,
        "unpriced_tokens must be {expected_unpriced_tokens} \
         (2 entries × 700 tokens for unknown model '{unknown_model}'); \
         got {unpriced_tokens}"
    );
}
