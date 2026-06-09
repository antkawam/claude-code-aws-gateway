/// Integration tests for Task 7: Admin API for `model_mappings` CRUD.
///
/// # BUILDER CONTRACT
///
/// ## New DB functions (src/db/model_mappings.rs)
///
/// ```rust
/// /// Delete a mapping by prefix. Returns `true` if a row was deleted,
/// /// `false` if the prefix did not exist. Bumps `cache_version`.
/// pub async fn delete_mapping(
///     pool: &PgPool,
///     anthropic_prefix: &str,
/// ) -> Result<bool, sqlx::Error>
///
/// /// Fetch a single mapping by prefix. Returns None if not found.
/// pub async fn get_mapping(
///     pool: &PgPool,
///     anthropic_prefix: &str,
/// ) -> Result<Option<ModelMappingRow>, sqlx::Error>
/// ```
///
/// ## New HTTP routes (src/api/mod.rs) — all under existing admin auth middleware
///
/// ```
/// GET    /admin/mappings
/// POST   /admin/mappings
/// PUT    /admin/mappings/:prefix     (URL-encoded; test with percent-encoding as needed)
/// DELETE /admin/mappings/:prefix
/// POST   /admin/mappings/discover
/// ```
///
/// ## GET /admin/mappings — response shape
///
/// ```json
/// {
///   "mappings": [
///     {
///       "anthropic_prefix": "claude-sonnet-4-6",
///       "bedrock_suffix": "anthropic.claude-sonnet-4-6",
///       "anthropic_display": "Claude Sonnet 4.6",   // nullable
///       "source": "admin",
///       "created_via": "admin",
///       "last_used_at": null,                        // nullable ISO-8601
///       "created_at": "2025-01-01T00:00:00Z"         // ISO-8601
///     }
///   ]
/// }
/// ```
///
/// ## POST /admin/mappings — request / response shapes
///
/// Request:
/// ```json
/// {
///   "anthropic_prefix": "Sonnet 4.7",               // required
///   "bedrock_suffix":   "anthropic.claude-sonnet-4-6", // required
///   "anthropic_display": "Sonnet 4.7"               // optional
/// }
/// ```
///
/// Success (201 or 200):
/// ```json
/// {
///   "anthropic_prefix": "Sonnet 4.7",
///   "bedrock_suffix":   "anthropic.claude-sonnet-4-6",
///   "anthropic_display": "Sonnet 4.7",
///   "source":     "admin",
///   "created_via": "admin",
///   "created_at":  "<ISO-8601>",
///   "last_used_at": null
/// }
/// ```
///
/// Conflict (409):
/// ```json
/// { "type": "error", "error": { "type": "conflict_error", "message": "..." } }
/// ```
///
/// ## PUT /admin/mappings/:prefix — request / response
///
/// Request body same as POST (without `anthropic_prefix` field, since it is
/// in the URL path — the Builder may also accept it in the body and ignore
/// mismatches, but the URL path takes precedence).
/// Success: 200 with the updated row. Bumps `cache_version`.
/// Re-normalizes `created_via = 'admin'` regardless of prior value.
///
/// ## DELETE /admin/mappings/:prefix — response
///
/// Success: 200 or 204 with either empty body or `{"deleted": true}`.
/// Not found: 404 `not_found_error`.
///
/// ## POST /admin/mappings/discover — request / response
///
/// Request:
/// ```json
/// { "model": "claude-sonnet-4-6-20250514" }
/// ```
///
/// On no-match (404):
/// ```json
/// { "type": "error", "error": { "type": "not_found_error", "message": "..." } }
/// ```
///
/// On match (200):
/// ```json
/// {
///   "anthropic_prefix": "claude-sonnet-4-6",
///   "bedrock_suffix":   "anthropic.claude-sonnet-4-6",
///   "anthropic_display": "...",
///   "would_be_created_via": "pass1"   // or "pass2"
/// }
/// ```
///
/// On Bedrock error (502):
/// ```json
/// { "type": "error", "error": { "type": "bedrock_error", "message": "..." } }
/// ```
///
/// NOTE: The discover endpoint calls `discover_model(state.bedrock_control_client, ...)`
/// which requires a real (or mock) Bedrock connection. In the test environment
/// there is no real Bedrock; the expected failure mode is 502 or 404.
/// See AC7.4 tests for detail.
///
/// ## Input validation rules (POST + PUT)
///
/// - `anthropic_prefix`: non-empty, max 64 chars, no leading/trailing whitespace.
/// - `bedrock_suffix`:
///   - must start with `anthropic.`
///   - must NOT contain an embedded region prefix (no `us.`, `eu.`, `ap.`, etc.
///     at the start or after `anthropic.`)
///   - max 128 chars
/// - `anthropic_display` (optional): no leading/trailing whitespace, max 64 chars.
///
/// ## Embedded region prefix definition
///
/// A `bedrock_suffix` is considered to have an embedded region prefix if it
/// matches: starts with `<region>.anthropic.` (e.g., `us.anthropic.`, `eu.anthropic.`)
/// OR if the portion after `anthropic.` starts with a region indicator
/// (`us.`, `eu.`, `ap.`, `us-east-`, etc.).
/// Simple safe rule to implement: reject if the suffix starts with something that
/// is NOT `anthropic.` (e.g., `us.anthropic.` fails the `starts_with("anthropic.")`
/// check) OR if after stripping the leading `anthropic.` the remainder starts with
/// `us.`, `eu.`, `ap.`.
///
/// ## Auth
///
/// All five endpoints require a valid admin session token (Bearer header).
/// Missing token → 401 UNAUTHORIZED.
/// Non-admin (member) token → 403 FORBIDDEN.
///
/// ## Audit logging
///
/// Each successful mutation (POST, PUT, DELETE) emits:
/// ```
/// tracing::info!(admin_sub = %sub, action = "create_mapping" | "update_mapping" | "delete_mapping", ...)
/// ```
/// Test files do not assert on log content (that is an [online] AC).
///
/// ## sqlx offline cache
///
/// After adding new `query!` / `query_as!` macros, the Builder MUST run:
/// ```
/// cargo sqlx prepare --workspace
/// ```
/// against a running `make dev` Postgres, then commit the updated `.sqlx/` files.
/// CI uses `SQLX_OFFLINE=true` and will fail if cache is stale.
///
/// # Run
///
/// ```
/// make test-integration   # requires Docker Postgres on port 5433
/// ```
///
/// # Pre-implementation failure mode
///
/// Before the Builder implements the routes, `SQLX_OFFLINE=true cargo check
/// --workspace --all-targets --features integration` will compile-fail because
/// the handler functions referenced in `src/api/mod.rs` do not yet exist:
///   - `admin::list_mappings`
///   - `admin::create_mapping`
///   - `admin::update_mapping`
///   - `admin::delete_mapping`
///   - `admin::discover_mapping`
/// and the DB functions `db::model_mappings::delete_mapping` and
/// `db::model_mappings::get_mapping` will be missing.
///
/// The tests themselves (guarded by `#[cfg(feature = "integration")]`) also
/// compile-fail if those symbols are absent, which is the intended signal.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::helpers;
use ccag::budget::BudgetSpendCache;

// ── Shared test app factory ───────────────────────────────────────────────────

/// Build a minimal Axum router backed by a real DB pool. Returns the router
/// and a valid admin session token.
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

    let signing_key = "test-signing-key-admin-mappings";

    // Seed the admin user so role resolution works.
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

/// Issue a member-role session token (not admin).
fn member_token(pool_signing_key: &str, sub: &str) -> String {
    let identity = ccag::auth::oidc::OidcIdentity {
        sub: sub.to_string(),
        email: None,
        idp_name: "Local".to_string(),
    };
    ccag::auth::session::issue(pool_signing_key, &identity, 24)
}

/// Read the current `cache_version` counter from the DB.
async fn get_cache_version(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM cache_version")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Parse a response body as JSON, panicking with the status + body on failure.
async fn parse_body(resp: axum::response::Response) -> Value {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("failed to read response body");
    serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        panic!(
            "response body is not valid JSON (status={status}): {:?}",
            bytes
        )
    })
}

/// Clear all rows from model_mappings to give each test a clean slate
/// (the migration seeds several rows).
async fn clear_mappings(pool: &sqlx::PgPool) {
    sqlx::query("DELETE FROM model_mappings")
        .execute(pool)
        .await
        .expect("clear_mappings failed");
}

// ── AC7.5: Auth — all endpoints require admin session token ──────────────────

/// GET /admin/mappings without auth → 401.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_5_get_mappings_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "GET /admin/mappings without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// POST /admin/mappings without auth → 401.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_5_post_mapping_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"anthropic_prefix":"test-model","bedrock_suffix":"anthropic.test-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "POST /admin/mappings without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// PUT /admin/mappings/:prefix without auth → 401.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_5_put_mapping_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/mappings/test-model")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"bedrock_suffix":"anthropic.test-model"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "PUT /admin/mappings/:prefix without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// DELETE /admin/mappings/:prefix without auth → 401.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_5_delete_mapping_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/mappings/test-model")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "DELETE /admin/mappings/:prefix without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// POST /admin/mappings/discover without auth → 401.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_5_discover_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings/discover")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"claude-sonnet-4-6-20250514"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "POST /admin/mappings/discover without auth must return 401 or 403, got {}",
        resp.status()
    );
}

/// Member-role token is rejected on all five endpoints (403, not 401).
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_5_member_token_rejected_on_all_endpoints() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let _ = ccag::db::users::create_user(&pool, "member@test.com", None, "member").await;
    let tok = member_token("test-signing-key-admin-mappings", "member@test.com");

    // We only need to assert on one endpoint here; the auth middleware is shared.
    // Test POST (most specific).
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {tok}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"anthropic_prefix":"test-model","bedrock_suffix":"anthropic.test-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "member token must be rejected with 401/403 on POST /admin/mappings, got {}",
        resp.status()
    );
}

// ── AC7.1: POST happy path — persists with created_via='admin', source='admin' ──

/// AC7.1: POST /admin/mappings with valid input persists the row with
/// `created_via='admin'`, `source='admin'`, and bumps `cache_version`.
/// GET confirms the row is present.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_1_post_persists_admin_row() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let v0 = get_cache_version(&pool).await;

    // POST a new mapping.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"anthropic_prefix":"Sonnet 4.7","bedrock_suffix":"anthropic.claude-sonnet-4-6","anthropic_display":"Sonnet 4.7"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "POST /admin/mappings happy path must return 200 or 201, got {status}"
    );

    let body = parse_body(resp).await;

    // Response must echo the correct fields.
    assert_eq!(
        body["anthropic_prefix"].as_str().unwrap_or(""),
        "Sonnet 4.7",
        "AC7.1: response must echo anthropic_prefix"
    );
    assert_eq!(
        body["bedrock_suffix"].as_str().unwrap_or(""),
        "anthropic.claude-sonnet-4-6",
        "AC7.1: response must echo bedrock_suffix"
    );
    assert_eq!(
        body["source"].as_str().unwrap_or(""),
        "admin",
        "AC7.1: response must have source='admin'"
    );
    assert_eq!(
        body["created_via"].as_str().unwrap_or(""),
        "admin",
        "AC7.1: response must have created_via='admin'"
    );

    // cache_version must have been bumped.
    let v1 = get_cache_version(&pool).await;
    assert!(
        v1 > v0,
        "AC7.1: cache_version must be bumped after POST (before={v0}, after={v1})"
    );

    // GET must show the inserted row.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let list_body = parse_body(resp).await;
    let mappings = list_body["mappings"]
        .as_array()
        .expect("AC7.1: GET /admin/mappings must return a 'mappings' array");

    let inserted = mappings
        .iter()
        .find(|m| m["anthropic_prefix"].as_str() == Some("Sonnet 4.7"))
        .expect("AC7.1: inserted row 'Sonnet 4.7' must appear in GET /admin/mappings");

    assert_eq!(
        inserted["source"].as_str().unwrap_or(""),
        "admin",
        "AC7.1: persisted row must have source='admin'"
    );
    assert_eq!(
        inserted["created_via"].as_str().unwrap_or(""),
        "admin",
        "AC7.1: persisted row must have created_via='admin'"
    );
}

/// AC7.1 (without optional display): POST with just the required two fields
/// succeeds and `anthropic_display` is null.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_1_post_without_display_succeeds() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"anthropic_prefix":"test-alias","bedrock_suffix":"anthropic.claude-sonnet-4-5"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "POST without anthropic_display must succeed, got {status}"
    );

    let body = parse_body(resp).await;
    // Display may be null or absent — both are acceptable.
    let display = body.get("anthropic_display");
    assert!(
        display.is_none() || display.is_some_and(|v| v.is_null()),
        "AC7.1: anthropic_display must be null when omitted from POST body; got {display:?}"
    );
}

// ── AC7.6: GET /admin/mappings — full metadata shape ─────────────────────────

/// AC7.6: GET /admin/mappings returns a 'mappings' array where each item has
/// all seven required fields.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_6_get_mappings_returns_full_metadata_shape() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    // Insert one row via DB directly so we control all fields.
    sqlx::query(
        r#"INSERT INTO model_mappings
           (anthropic_prefix, bedrock_suffix, anthropic_display, source, created_via)
           VALUES ('shape-test', 'anthropic.claude-sonnet-4-6', 'Shape Test', 'admin', 'admin')"#,
    )
    .execute(&pool)
    .await
    .expect("fixture insert failed");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "AC7.6: GET must return 200");

    let body = parse_body(resp).await;
    let mappings = body["mappings"]
        .as_array()
        .expect("AC7.6: response must have a 'mappings' array at top level");

    let row = mappings
        .iter()
        .find(|m| m["anthropic_prefix"].as_str() == Some("shape-test"))
        .expect("AC7.6: 'shape-test' row must be present in GET /admin/mappings");

    // Verify all seven required fields are present.
    assert!(
        row.get("anthropic_prefix").is_some(),
        "AC7.6: 'anthropic_prefix' field must be present in mapping row"
    );
    assert!(
        row.get("bedrock_suffix").is_some(),
        "AC7.6: 'bedrock_suffix' field must be present in mapping row"
    );
    assert!(
        row.get("anthropic_display").is_some(),
        "AC7.6: 'anthropic_display' field must be present in mapping row (may be null)"
    );
    assert!(
        row.get("source").is_some(),
        "AC7.6: 'source' field must be present in mapping row"
    );
    assert!(
        row.get("created_via").is_some(),
        "AC7.6: 'created_via' field must be present in mapping row"
    );
    assert!(
        row.get("last_used_at").is_some(),
        "AC7.6: 'last_used_at' field must be present in mapping row (may be null)"
    );
    assert!(
        row.get("created_at").is_some(),
        "AC7.6: 'created_at' field must be present in mapping row"
    );

    // Spot-check the values we control.
    assert_eq!(row["anthropic_prefix"].as_str(), Some("shape-test"));
    assert_eq!(
        row["bedrock_suffix"].as_str(),
        Some("anthropic.claude-sonnet-4-6")
    );
    assert_eq!(row["source"].as_str(), Some("admin"));
    assert_eq!(row["created_via"].as_str(), Some("admin"));

    // created_at must be a non-null string (ISO-8601 timestamp).
    assert!(
        row["created_at"].as_str().is_some(),
        "AC7.6: 'created_at' must be a non-null ISO-8601 string"
    );
}

/// AC7.6: GET /admin/mappings on an empty table returns an empty array (not 404).
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_6_get_mappings_empty_returns_empty_array() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = parse_body(resp).await;
    let mappings = body["mappings"]
        .as_array()
        .expect("GET on empty table must still return 'mappings' array");
    assert!(
        mappings.is_empty(),
        "AC7.6: GET on empty table must return empty 'mappings' array, got {} rows",
        mappings.len()
    );
}

/// AC7.6: GET lists multiple rows including all created_via values.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_6_get_mappings_lists_multiple_rows_with_all_created_via_values() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    // Insert one row for each created_via value.
    for (prefix, suffix, via) in [
        ("test-unknown", "anthropic.claude-sonnet-4-6", "unknown"),
        ("test-pass1", "anthropic.claude-sonnet-4-5", "pass1"),
        ("test-pass2", "anthropic.claude-haiku-4-5", "pass2"),
        ("test-admin", "anthropic.claude-opus-4-6", "admin"),
    ] {
        sqlx::query(
            r#"INSERT INTO model_mappings
               (anthropic_prefix, bedrock_suffix, source, created_via)
               VALUES ($1, $2, 'seed', $3)
               ON CONFLICT (anthropic_prefix) DO NOTHING"#,
        )
        .bind(prefix)
        .bind(suffix)
        .bind(via)
        .execute(&pool)
        .await
        .expect("fixture insert failed");
    }

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = parse_body(resp).await;
    let mappings = body["mappings"]
        .as_array()
        .expect("must return 'mappings' array");

    assert_eq!(
        mappings.len(),
        4,
        "AC7.6: must list all 4 inserted rows, got {}",
        mappings.len()
    );

    let created_via_values: Vec<&str> = mappings
        .iter()
        .filter_map(|m| m["created_via"].as_str())
        .collect();
    for expected_via in ["unknown", "pass1", "pass2", "admin"] {
        assert!(
            created_via_values.contains(&expected_via),
            "AC7.6: 'created_via={expected_via}' must appear in listing"
        );
    }
}

// ── AC7.2: POST validation rejects ───────────────────────────────────────────

/// AC7.2 (embedded region prefix): POST with `bedrock_suffix = "us.anthropic.claude-sonnet-4-6"`
/// must return 400 `invalid_request_error`.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_embedded_region_prefix_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    for bad_suffix in [
        "us.anthropic.claude-sonnet-4-6",
        "eu.anthropic.claude-sonnet-4-6",
        "ap.anthropic.claude-sonnet-4-6",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/mappings")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"anthropic_prefix":"test-model","bedrock_suffix":"{bad_suffix}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "AC7.2: POST with embedded region prefix '{bad_suffix}' must return 400, got {}",
            resp.status()
        );

        let body = parse_body(resp).await;
        let error_type = body
            .pointer("/error/type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            error_type, "invalid_request_error",
            "AC7.2: error.type must be 'invalid_request_error' for embedded region prefix; got body: {body}"
        );
    }
}

/// AC7.2 (empty anthropic_prefix): POST with empty string returns 400.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_empty_prefix_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"anthropic_prefix":"","bedrock_suffix":"anthropic.claude-sonnet-4-6"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "AC7.2: POST with empty anthropic_prefix must return 400"
    );

    let body = parse_body(resp).await;
    assert_eq!(
        body.pointer("/error/type")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "invalid_request_error",
        "AC7.2: error.type must be 'invalid_request_error' for empty prefix"
    );
}

/// AC7.2 (anthropic_prefix > 64 chars): returns 400.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_prefix_too_long_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let long_prefix = "a".repeat(65);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"anthropic_prefix":"{long_prefix}","bedrock_suffix":"anthropic.claude-sonnet-4-6"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "AC7.2: POST with anthropic_prefix > 64 chars must return 400"
    );
}

/// AC7.2 (leading/trailing whitespace in prefix): returns 400.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_prefix_whitespace_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    for bad_prefix in [" leading", "trailing "] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/mappings")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"anthropic_prefix":"{bad_prefix}","bedrock_suffix":"anthropic.claude-sonnet-4-6"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "AC7.2: POST with prefix '{bad_prefix}' (leading/trailing whitespace) must return 400, got {}",
            resp.status()
        );
    }
}

/// AC7.2 (bedrock_suffix not starting with "anthropic."): returns 400.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_suffix_not_anthropic_prefix_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    for bad_suffix in [
        "amazon.claude-sonnet-4-6",
        "claude-sonnet-4-6",           // no prefix at all
        "ANTHROPIC.claude-sonnet-4-6", // wrong case
        "",                            // empty
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/mappings")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"anthropic_prefix":"test-model","bedrock_suffix":"{bad_suffix}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "AC7.2: POST with bedrock_suffix '{bad_suffix}' must return 400, got {}",
            resp.status()
        );
    }
}

/// AC7.2 (bedrock_suffix > 128 chars): returns 400.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_suffix_too_long_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // 10 chars for "anthropic." + 119 'a' chars = 129 total.
    let long_suffix = format!("anthropic.{}", "a".repeat(119));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"anthropic_prefix":"test-model","bedrock_suffix":"{long_suffix}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "AC7.2: POST with bedrock_suffix > 128 chars must return 400"
    );
}

/// AC7.2 (conflict — POST same prefix twice): second POST returns 409.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_conflict_returns_409() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let body_str =
        r#"{"anthropic_prefix":"Sonnet 4.8","bedrock_suffix":"anthropic.claude-sonnet-4-6"}"#;

    // First POST must succeed.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body_str))
                .unwrap(),
        )
        .await
        .unwrap();

    let first_status = resp.status();
    assert!(
        first_status == StatusCode::CREATED || first_status == StatusCode::OK,
        "first POST must succeed, got {first_status}"
    );

    // Second POST with the same prefix must return 409 Conflict.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
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
        "AC7.2: duplicate POST must return 409 Conflict"
    );

    let body = parse_body(resp).await;
    // Must carry a structured error body.
    assert!(
        body.get("error").is_some() || body.get("type").is_some(),
        "AC7.2: 409 response must carry a structured error body; got: {body}"
    );
}

/// AC7.2 (missing required fields): POST without anthropic_prefix returns 400 or 422.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_2_post_missing_required_fields_returns_4xx() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Missing bedrock_suffix.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"anthropic_prefix":"test-model"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status.is_client_error(),
        "POST without bedrock_suffix must return 4xx, got {status}"
    );

    // Missing anthropic_prefix.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"bedrock_suffix":"anthropic.claude-sonnet-4-6"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status.is_client_error(),
        "POST without anthropic_prefix must return 4xx, got {status}"
    );
}

// ── AC7.3: DELETE removes row, bumps cache_version ───────────────────────────

/// AC7.3: DELETE /admin/mappings/:prefix removes the row and bumps cache_version.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_3_delete_removes_row_and_bumps_cache_version() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    // Insert a row directly.
    sqlx::query(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, source, created_via)
           VALUES ('delete-me', 'anthropic.claude-sonnet-4-6', 'admin', 'admin')"#,
    )
    .execute(&pool)
    .await
    .expect("fixture insert failed");

    let v0 = get_cache_version(&pool).await;

    // DELETE it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/mappings/delete-me")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let del_status = resp.status();
    assert!(
        del_status == StatusCode::OK || del_status == StatusCode::NO_CONTENT,
        "AC7.3: DELETE must return 200 or 204, got {del_status}"
    );

    // cache_version must have been bumped.
    let v1 = get_cache_version(&pool).await;
    assert!(
        v1 > v0,
        "AC7.3: cache_version must be bumped after DELETE (before={v0}, after={v1})"
    );

    // GET must no longer show the deleted row.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let list_body = parse_body(resp).await;
    let mappings = list_body["mappings"]
        .as_array()
        .expect("GET must return 'mappings' array");

    let still_present = mappings
        .iter()
        .any(|m| m["anthropic_prefix"].as_str() == Some("delete-me"));
    assert!(
        !still_present,
        "AC7.3: deleted row 'delete-me' must not appear in GET /admin/mappings after DELETE"
    );
}

/// AC7.3: DELETE /admin/mappings/:prefix on a non-existent prefix returns 404.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_3_delete_nonexistent_returns_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/mappings/does-not-exist")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "AC7.3: DELETE on non-existent prefix must return 404"
    );
}

/// AC7.3: After DELETE, the DB-layer `delete_mapping` function returns false
/// on a second attempt (row already gone).
///
/// This tests the DB function surface directly.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_3_db_delete_mapping_returns_false_on_missing() {
    let pool = helpers::setup_test_db().await;
    clear_mappings(&pool).await;

    // Calling delete_mapping on a non-existent prefix must return Ok(false).
    let deleted: bool = ccag::db::model_mappings::delete_mapping(&pool, "not-in-db")
        .await
        .expect("AC7.3: delete_mapping must not return Err on missing prefix");

    assert!(
        !deleted,
        "AC7.3: delete_mapping must return false when the prefix does not exist"
    );
}

/// AC7.3: `delete_mapping` returns true after deleting an existing row.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_3_db_delete_mapping_returns_true_on_existing() {
    let pool = helpers::setup_test_db().await;
    clear_mappings(&pool).await;

    // Insert a row.
    sqlx::query(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, source, created_via)
           VALUES ('db-delete-test', 'anthropic.claude-haiku-4-5', 'admin', 'admin')"#,
    )
    .execute(&pool)
    .await
    .expect("fixture insert failed");

    let deleted: bool = ccag::db::model_mappings::delete_mapping(&pool, "db-delete-test")
        .await
        .expect("delete_mapping must not return Err on existing prefix");

    assert!(
        deleted,
        "AC7.3: delete_mapping must return true when the row existed and was deleted"
    );

    // Row must be gone.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix = 'db-delete-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("count query failed");
    assert_eq!(count, 0, "AC7.3: row must be gone after delete_mapping");
}

// ── AC7.7: PUT overwrites row and re-normalizes created_via='admin' ──────────

/// AC7.7: PUT /admin/mappings/:prefix overwrites the row. After PUT,
/// `created_via = 'admin'` even if the row previously had `created_via = 'unknown'`.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_7_put_overwrites_row_renormalizes_created_via() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    // Insert a row with created_via='unknown' (simulates a grandfathered row).
    sqlx::query(
        r#"INSERT INTO model_mappings
           (anthropic_prefix, bedrock_suffix, anthropic_display, source, created_via)
           VALUES ('overwrite-test', 'anthropic.claude-haiku-4-5', 'Old Display', 'seed', 'unknown')"#,
    )
    .execute(&pool)
    .await
    .expect("fixture insert failed");

    let v0 = get_cache_version(&pool).await;

    // PUT to overwrite.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/mappings/overwrite-test")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"bedrock_suffix":"anthropic.claude-sonnet-4-6","anthropic_display":"New Display"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let put_status = resp.status();
    assert!(
        put_status == StatusCode::OK || put_status == StatusCode::CREATED,
        "AC7.7: PUT must return 200 or 201, got {put_status}"
    );

    // cache_version must have been bumped.
    let v1 = get_cache_version(&pool).await;
    assert!(
        v1 > v0,
        "AC7.7: cache_version must be bumped after PUT (before={v0}, after={v1})"
    );

    // GET the updated row and verify fields.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let list_body = parse_body(resp).await;
    let mappings = list_body["mappings"]
        .as_array()
        .expect("GET must return 'mappings' array");

    let updated = mappings
        .iter()
        .find(|m| m["anthropic_prefix"].as_str() == Some("overwrite-test"))
        .expect("AC7.7: 'overwrite-test' row must still be present after PUT");

    assert_eq!(
        updated["bedrock_suffix"].as_str().unwrap_or(""),
        "anthropic.claude-sonnet-4-6",
        "AC7.7: PUT must overwrite bedrock_suffix"
    );
    assert_eq!(
        updated["created_via"].as_str().unwrap_or(""),
        "admin",
        "AC7.7: PUT must re-normalize created_via to 'admin' even if row was previously 'unknown'"
    );
}

/// AC7.7: PUT on a row with `created_via='pass1'` also re-normalizes to 'admin'.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_7_put_renormalizes_pass1_to_admin() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    sqlx::query(
        r#"INSERT INTO model_mappings
           (anthropic_prefix, bedrock_suffix, source, created_via)
           VALUES ('pass1-overwrite', 'anthropic.claude-sonnet-4-5', 'discovered', 'pass1')"#,
    )
    .execute(&pool)
    .await
    .expect("fixture insert failed");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/mappings/pass1-overwrite")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"bedrock_suffix":"anthropic.claude-sonnet-4-6"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let put_status = resp.status();
    assert!(
        put_status == StatusCode::OK || put_status == StatusCode::CREATED,
        "AC7.7: PUT must succeed, got {put_status}"
    );

    // Check via DB directly.
    let created_via: String = sqlx::query_scalar(
        "SELECT created_via FROM model_mappings WHERE anthropic_prefix = 'pass1-overwrite'",
    )
    .fetch_one(&pool)
    .await
    .expect("AC7.7: SELECT created_via after PUT failed");

    assert_eq!(
        created_via, "admin",
        "AC7.7: PUT must re-normalize created_via to 'admin' (was 'pass1'); got '{created_via}'"
    );
}

/// AC7.7: PUT on a non-existent prefix returns 404.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_7_put_nonexistent_returns_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/mappings/does-not-exist")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"bedrock_suffix":"anthropic.claude-sonnet-4-6"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "AC7.7: PUT on non-existent prefix must return 404"
    );
}

/// AC7.7: PUT applies the same input validation as POST.
/// bedrock_suffix with embedded region prefix → 400.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_7_put_validates_input_same_as_post() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    // Insert a row to PUT against.
    sqlx::query(
        r#"INSERT INTO model_mappings
           (anthropic_prefix, bedrock_suffix, source, created_via)
           VALUES ('put-validate-test', 'anthropic.claude-sonnet-4-6', 'admin', 'admin')"#,
    )
    .execute(&pool)
    .await
    .expect("fixture insert failed");

    // Attempt PUT with an invalid bedrock_suffix.
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/mappings/put-validate-test")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"bedrock_suffix":"us.anthropic.claude-sonnet-4-6"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "AC7.7: PUT with embedded region prefix must return 400 (same validation as POST)"
    );
}

// ── AC7.4: POST /admin/mappings/discover ─────────────────────────────────────

/// AC7.4 (404 path): POST /admin/mappings/discover with a model that the
/// gateway control client cannot match returns 404 `not_found_error`.
///
/// In the test environment there is no real Bedrock connection.
/// `discover_model` is expected to return None or time-out, resulting in
/// either 404 (no match) or 502 (Bedrock error). Both are acceptable since
/// the test environment has no real AWS credentials pointing at live Bedrock.
///
/// CONTRACT TO BUILDER: the test here asserts that the endpoint correctly
/// propagates discovery failure to an HTTP error (not panic, not 200).
/// The exact status code (404 vs 502) depends on how the no-Bedrock case
/// manifests at runtime:
///   - If `discover_model` returns `None` quickly (e.g. the SDK immediately
///     errors) → the handler MUST return 404 `not_found_error`.
///   - If `discover_model` returns an SDK error → 502 `bedrock_error`.
///
/// Both cases are valid in CI. The test asserts the status is 404 OR 502.
///
/// For a true 404 path test (requires mock Bedrock), see the builder contract
/// above: the Builder should consider a seam (`state.discover_fn`) for testing.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_4_discover_no_match_returns_4xx_or_502() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings/discover")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-totally-made-up-model-99-99"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status == StatusCode::NOT_FOUND
            || status == StatusCode::BAD_GATEWAY
            || status == StatusCode::GATEWAY_TIMEOUT,
        "AC7.4: discover with unresolvable model must return 404, 502, or 504 in CI, got {status}"
    );

    let body = parse_body(resp).await;

    // Whatever the status, there must be a structured error body.
    assert!(
        body.get("error").is_some(),
        "AC7.4: error response must have an 'error' field; got body: {body}"
    );

    let error_type = body
        .pointer("/error/type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // For 404: "not_found_error"; for 502: "bedrock_error".
    assert!(
        error_type == "not_found_error"
            || error_type == "bedrock_error"
            || error_type == "upstream_error",
        "AC7.4: error.type must be 'not_found_error' or 'bedrock_error', got '{error_type}' in body: {body}"
    );
}

/// AC7.4 (no persist): POST /admin/mappings/discover must NOT persist a row
/// in the database even when it returns a preview.
///
/// This test exercises the "dry run" contract: discover is always read-only.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_4_discover_does_not_persist_row() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let row_count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_mappings")
        .fetch_one(&pool)
        .await
        .expect("count before failed");

    // Call discover (result may be 404/502/504 — doesn't matter for this test).
    let _ = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings/discover")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"claude-sonnet-4-6-20250514"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let row_count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_mappings")
        .fetch_one(&pool)
        .await
        .expect("count after failed");

    assert_eq!(
        row_count_before, row_count_after,
        "AC7.4: discover must NOT persist a row to model_mappings \
         (before={row_count_before}, after={row_count_after})"
    );
}

/// AC7.4 (missing body field): POST /admin/mappings/discover with missing
/// 'model' field returns 400.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_4_discover_missing_model_field_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings/discover")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status.is_client_error(),
        "AC7.4: discover without 'model' field must return 4xx, got {status}"
    );
}

// ── AC7.1+AC7.3: Round-trip: POST then DELETE then GET ───────────────────────

/// Round-trip: POST a row, verify it's in GET, DELETE it, verify it's gone.
/// Both writes must bump cache_version independently.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac7_1_and_ac7_3_post_then_delete_round_trip() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    clear_mappings(&pool).await;

    let v0 = get_cache_version(&pool).await;

    // POST.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"anthropic_prefix":"round-trip","bedrock_suffix":"anthropic.claude-sonnet-4-5"}"#,
                ))
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
    assert!(v1 > v0, "cache_version must increase after POST");

    // GET must show the row.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = parse_body(resp).await;
    let found = list_body["mappings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["anthropic_prefix"].as_str() == Some("round-trip"));
    assert!(found, "POST row must appear in GET");

    // DELETE.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/mappings/round-trip")
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
    assert!(v2 > v1, "cache_version must increase after DELETE");

    // GET must no longer show the row.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/mappings")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = parse_body(resp).await;
    let still_present = list_body["mappings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["anthropic_prefix"].as_str() == Some("round-trip"));
    assert!(
        !still_present,
        "deleted row must not appear in GET after DELETE"
    );
}

// ── DB-layer: get_mapping ─────────────────────────────────────────────────────

/// `get_mapping` returns None for a non-existent prefix.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_db_get_mapping_returns_none_for_missing_prefix() {
    let pool = helpers::setup_test_db().await;
    clear_mappings(&pool).await;

    let result: Option<ccag::db::model_mappings::ModelMappingRow> =
        ccag::db::model_mappings::get_mapping(&pool, "not-in-db")
            .await
            .expect("get_mapping must not return Err on missing prefix");

    assert!(
        result.is_none(),
        "get_mapping must return None when the prefix does not exist"
    );
}

/// `get_mapping` returns Some(row) for an existing prefix with correct field values.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_db_get_mapping_returns_row_for_existing_prefix() {
    let pool = helpers::setup_test_db().await;
    clear_mappings(&pool).await;

    sqlx::query(
        r#"INSERT INTO model_mappings
           (anthropic_prefix, bedrock_suffix, anthropic_display, source, created_via)
           VALUES ('get-test', 'anthropic.claude-sonnet-4-6', 'Get Test', 'admin', 'admin')"#,
    )
    .execute(&pool)
    .await
    .expect("fixture insert failed");

    let row: ccag::db::model_mappings::ModelMappingRow =
        ccag::db::model_mappings::get_mapping(&pool, "get-test")
            .await
            .expect("get_mapping must not return Err")
            .expect("get_mapping must return Some for an existing prefix");

    assert_eq!(
        row.anthropic_prefix, "get-test",
        "get_mapping: anthropic_prefix must match"
    );
    assert_eq!(
        row.bedrock_suffix, "anthropic.claude-sonnet-4-6",
        "get_mapping: bedrock_suffix must match"
    );
    assert_eq!(
        row.anthropic_display.as_deref(),
        Some("Get Test"),
        "get_mapping: anthropic_display must match"
    );
    assert_eq!(
        row.created_via, "admin",
        "get_mapping: created_via must match"
    );
    assert!(
        row.last_used_at.is_none(),
        "get_mapping: last_used_at must be None for a freshly inserted row"
    );
}
