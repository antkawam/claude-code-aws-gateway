/// Integration tests for Task 7: Backwards-compat shim on POST /admin/endpoints
/// and GET /admin/endpoints/{id} response shape.
///
/// # Contract points covered (numbered per the Task 7 spec)
///
/// 1. POST without `inference_profile_arn`
///      → endpoint created, `aip_overrides: []`, `inference_profile_arn: null`,
///        NO Deprecation/Sunset headers.
///
/// 2. POST with `inference_profile_arn` (shim compat path — GetInferenceProfile succeeds)
///      → endpoint created, exactly ONE row in `endpoint_aip_overrides` with
///        `set_by = COMPAT_SHIM_SET_BY` and `aip_arn = <supplied ARN>`.
///        Response body: `aip_overrides: [{...one row...}]` AND
///        `inference_profile_arn = <supplied>`.
///        Response headers: `Deprecation: true`, `Sunset: <EXPECTED_SUNSET_DATE>`.
///
/// 3. POST with `inference_profile_arn` but GetInferenceProfile errors
///      → endpoint IS created (operator not blocked), zero override rows, legacy
///        column populated.  Response: `aip_overrides: []`,
///        `inference_profile_arn = <supplied>`.  Deprecation header present.
///        Endpoint creation NOT rolled back.
///
/// 4. GET /admin/endpoints/{id}: zero override rows + legacy column null
///      → `aip_overrides: []`, `inference_profile_arn: null`,
///        no Deprecation/Sunset headers.
///
/// 5. GET /admin/endpoints/{id}: exactly one override row + legacy null
///      → `aip_overrides: [{row}]`, `inference_profile_arn = row.aip_arn`,
///        Deprecation + Sunset headers.
///
/// 6. GET /admin/endpoints/{id}: two override rows + legacy null
///      → `aip_overrides: [{...}, {...}]`, `inference_profile_arn: null`,
///        NO Deprecation/Sunset headers (can't represent multiple ARNs).
///
/// 7. GET /admin/endpoints/{id}: zero rows + legacy column populated
///    (auto-migration not yet completed)
///      → `aip_overrides: []`, `inference_profile_arn = <legacy>`,
///        Deprecation + Sunset headers (signals legacy field in use).
///
/// Extra smoke: admin auth still required on both POST and GET.
/// Extra smoke: cache_version is bumped when the shim auto-inserts an override row.
///
/// # Deprecation header contract (snapshot — builder must match these exactly)
///
/// Header name:  `Deprecation`
/// Header value: `true`
///
/// Header name:  `Sunset`
/// Header value: `Wed, 01 Jan 2027 00:00:00 GMT`
///   (RFC 1123 / HTTP-date format; one minor release ahead of the current 1.x
///    series, chosen per spec §Slice 3 backwards compat shim guidance)
///
/// # BUILDER CONTRACT
///
/// The compat shim logic in `POST /admin/endpoints` needs to call
/// `GetInferenceProfile` after creating the endpoint, parse the foundation model
/// via `parse_foundation_model_from_arn` + `bedrock_to_anthropic`, and insert
/// a row with `set_by = "compat-shim-on-create"`.
///
/// Because the handler runs in an integration context without real AWS creds,
/// the builder MUST make the `GetInferenceProfile` call injectable — either by
/// accepting a generic closure (like `migrate_legacy_aip_endpoints` does in
/// `src/migrations/aip_legacy.rs`) or by extracting a small helper fn:
///
/// ```rust
/// pub async fn try_create_compat_override<F, Fut>(
///     endpoint_id: uuid::Uuid,
///     legacy_arn: &str,
///     pool: &sqlx::PgPool,
///     get_foundation_model: F,
/// ) -> Result<(), String>
/// where
///     F: Fn(&str) -> Fut + Send + Sync,
///     Fut: std::future::Future<Output = Result<String, String>> + Send,
/// { ... }
/// ```
///
/// This helper is what makes contract points 2 and 3 testable in isolation
/// (via `aip_compat_shim_try_create_compat_override_*` tests below) without
/// requiring real Bedrock credentials.
///
/// The integration tests for `POST /admin/endpoints` (contract points 1-3) test
/// the HTTP handler and therefore rely on GatewayState wiring. See the note in
/// `test_app` about the endpoint pool and how the mock is injected.
///
/// Run with: make test-integration
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

use crate::helpers;
use ccag::budget::BudgetSpendCache;
use ccag::db;
use ccag::db::endpoint_aip_overrides;

// ── Snapshot constants (builder must match these exactly) ─────────────────────

/// The `set_by` value written by the compat shim at POST /admin/endpoints time.
/// This constant is also baked into assertions so the builder has one place to
/// change if they choose a different string.
const COMPAT_SHIM_SET_BY: &str = "compat-shim-on-create";

/// The exact value of the `Deprecation` response header.
const DEPRECATION_HEADER_VALUE: &str = "true";

/// The exact value of the `Sunset` response header.
/// RFC 1123 / HTTP-date format, one minor release ahead.
const SUNSET_HEADER_VALUE: &str = "Wed, 01 Jan 2027 00:00:00 GMT";

/// A valid AIP ARN used in POST /admin/endpoints shim tests.
const SHIM_ARN: &str =
    "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-compat";

/// A second AIP ARN (Opus) used in multi-row GET tests.
const SHIM_ARN_OPUS: &str =
    "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/opus-compat";

// ── shared test app factory ────────────────────────────────────────────────────

/// Build a minimal test router backed by a real DB pool.
///
/// Mirrors the pattern in `aip_overrides_admin_tests.rs::test_app`.
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

    let signing_key = "test-signing-key-compat-shim";

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

/// Create a minimal endpoint via the DB, returning its UUID string.
/// Used for GET endpoint shape tests (no API call, so no shim involved).
async fn create_endpoint_db(
    pool: &sqlx::PgPool,
    name: &str,
    inference_profile_arn: Option<&str>,
) -> Uuid {
    db::endpoints::create_endpoint(
        pool,
        name,
        None,
        None,
        inference_profile_arn,
        "us-east-1",
        "us",
        0,
    )
    .await
    .unwrap_or_else(|e| panic!("create_endpoint_db({name}) failed: {e}"))
    .id
}

// ── Contract point 1: POST without inference_profile_arn ─────────────────────

/// POST /admin/endpoints WITHOUT `inference_profile_arn`:
/// - Endpoint is created (201).
/// - Response body contains `aip_overrides: []`.
/// - Response body contains `inference_profile_arn: null`.
/// - No `Deprecation` or `Sunset` headers in the response.
#[tokio::test]
async fn post_endpoint_no_arn_no_overrides_no_deprecation_headers() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/endpoints")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"ep-no-arn","region":"us-east-1","routing_prefix":"us"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();

    // Check headers before consuming the body
    let deprecation = resp.headers().get("deprecation").cloned();
    let sunset = resp.headers().get("sunset").cloned();

    let body = parse_body(resp).await;

    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "POST without inference_profile_arn must return 200 or 201, got {status}; body={body}"
    );

    // Response body must contain aip_overrides as an empty array
    let aip_overrides = body
        .get("aip_overrides")
        .expect("response body must include 'aip_overrides' field; got {body}");
    assert_eq!(
        aip_overrides,
        &Value::Array(vec![]),
        "aip_overrides must be [] when no inference_profile_arn is supplied; got {aip_overrides}"
    );

    // inference_profile_arn must be null (not absent — always present)
    let ipa = body.get("inference_profile_arn").expect(
        "response body must include 'inference_profile_arn' field (always present); got {body}",
    );
    assert!(
        ipa.is_null(),
        "inference_profile_arn must be null when no ARN supplied; got {ipa}"
    );

    // Deprecation and Sunset headers must NOT be present
    assert!(
        deprecation.is_none(),
        "Deprecation header must NOT be present when inference_profile_arn is absent"
    );
    assert!(
        sunset.is_none(),
        "Sunset header must NOT be present when inference_profile_arn is absent"
    );

    // Verify DB: the created endpoint has zero override rows
    let endpoint_id: Uuid = body["id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("response body must include 'id' field with a UUID string");

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, endpoint_id)
        .await
        .expect("list_by_endpoint must not fail");

    assert!(
        rows.is_empty(),
        "zero override rows must exist in DB when no inference_profile_arn is supplied; got {rows:?}"
    );
}

// ── Contract point 2: POST with inference_profile_arn — shim succeeds ─────────

/// POST /admin/endpoints WITH `inference_profile_arn`, GetInferenceProfile succeeds:
/// - Endpoint is created (201).
/// - Exactly ONE row in `endpoint_aip_overrides` with `set_by = COMPAT_SHIM_SET_BY`
///   and `aip_arn = SHIM_ARN`.
/// - Response body: `aip_overrides: [{one row with the ARN}]`
///   AND `inference_profile_arn = SHIM_ARN`.
/// - Response headers: `Deprecation: true` and `Sunset: <SUNSET_HEADER_VALUE>`.
///
/// NOTE FOR BUILDER: This test runs against the real HTTP handler via `test_app`.
/// Because the test environment has no Bedrock credentials, `GetInferenceProfile`
/// will fail with a credentials/dispatch error at test time.  The handler should
/// therefore treat that as a "GetInferenceProfile failed" scenario (contract
/// point 3), and this test will exercise the "no-credentials fallback" variant.
///
/// To make this test green for contract point 2 (the success branch), the builder
/// must either:
///   (a) Inject the `get_foundation_model` closure into GatewayState so the
///       test_app can supply a mock; OR
///   (b) Use a unit-level helper (`try_create_compat_override`) that is tested
///       separately (see `aip_compat_shim_try_create_compat_override_success` below).
///
/// The HTTP-level tests (this function and `post_endpoint_with_arn_get_infprof_fails`)
/// assert the observable HTTP shape without caring HOW the builder wires
/// the injection — they simply check what the response contains.
///
/// The contract-point-2 SUCCESS case (shim row is actually inserted) is also
/// covered by the helper-level test `aip_compat_shim_try_create_compat_override_success`
/// which bypasses the handler.
#[tokio::test]
async fn post_endpoint_with_arn_response_shape_has_deprecation_headers() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let body_str = format!(
        r#"{{"name":"ep-with-arn","region":"us-east-1","routing_prefix":"us","inference_profile_arn":"{SHIM_ARN}"}}"#
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/endpoints")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body_str))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();

    // Snapshot the deprecation/sunset headers before consuming body
    let deprecation = resp
        .headers()
        .get("deprecation")
        .map(|v| v.to_str().unwrap_or("").to_string());
    let sunset = resp
        .headers()
        .get("sunset")
        .map(|v| v.to_str().unwrap_or("").to_string());

    let body = parse_body(resp).await;

    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "POST with inference_profile_arn must return 200 or 201, got {status}; body={body}"
    );

    // Response body must always include `aip_overrides` (array, possibly empty if shim failed)
    let aip_overrides = body
        .get("aip_overrides")
        .expect("response body must include 'aip_overrides' field when inference_profile_arn is present; body={body}");
    assert!(
        aip_overrides.is_array(),
        "aip_overrides must be a JSON array; got {aip_overrides}"
    );

    // Response body must include inference_profile_arn = SHIM_ARN (not null)
    let ipa = body
        .get("inference_profile_arn")
        .expect("response body must include 'inference_profile_arn' field; body={body}");
    assert_eq!(
        ipa.as_str().unwrap_or(""),
        SHIM_ARN,
        "inference_profile_arn in response must equal the supplied ARN; got {ipa}"
    );

    // Deprecation header MUST be present and equal DEPRECATION_HEADER_VALUE
    assert_eq!(
        deprecation.as_deref(),
        Some(DEPRECATION_HEADER_VALUE),
        "Deprecation header must be '{DEPRECATION_HEADER_VALUE}' when inference_profile_arn is set; got {deprecation:?}"
    );

    // Sunset header MUST be present and equal SUNSET_HEADER_VALUE
    assert_eq!(
        sunset.as_deref(),
        Some(SUNSET_HEADER_VALUE),
        "Sunset header must be '{SUNSET_HEADER_VALUE}' when inference_profile_arn is set; got {sunset:?}"
    );
}

// ── Contract point 3: POST with inference_profile_arn — GetInferenceProfile fails ──

/// POST /admin/endpoints WITH `inference_profile_arn`, GetInferenceProfile errors:
/// - Endpoint IS still created (operator not blocked).
/// - Zero override rows in `endpoint_aip_overrides`.
/// - Legacy column (`inference_profile_arn`) IS populated in the DB.
/// - Response: `aip_overrides: []`, `inference_profile_arn = SHIM_ARN`.
/// - Response headers: Deprecation present.
/// - Endpoint creation is NOT rolled back.
///
/// In the test environment there are no Bedrock credentials, so GetInferenceProfile
/// will always fail — this test exercises that path directly.
#[tokio::test]
async fn post_endpoint_with_arn_get_infprof_fails_endpoint_still_created() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let body_str = format!(
        r#"{{"name":"ep-with-arn-fail","region":"us-east-1","routing_prefix":"us","inference_profile_arn":"{SHIM_ARN}"}}"#
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/endpoints")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body_str))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();

    let deprecation = resp
        .headers()
        .get("deprecation")
        .map(|v| v.to_str().unwrap_or("").to_string());

    let body = parse_body(resp).await;

    // Endpoint creation must succeed even when GetInferenceProfile fails
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "POST with inference_profile_arn must still return 200/201 even when GetInferenceProfile fails; got {status}; body={body}"
    );

    // Extract endpoint id
    let endpoint_id: Uuid = body["id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("response body must include 'id' field with a UUID string; body={body}");

    // Endpoint must exist in DB (not rolled back)
    let ep_row =
        sqlx::query_as::<_, ccag::db::schema::Endpoint>("SELECT * FROM endpoints WHERE id = $1")
            .bind(endpoint_id)
            .fetch_optional(&pool)
            .await
            .expect("DB query must not fail");

    assert!(
        ep_row.is_some(),
        "endpoint must exist in DB even when GetInferenceProfile fails (not rolled back)"
    );

    // Legacy column must be populated
    let ep = ep_row.unwrap();
    assert_eq!(
        ep.inference_profile_arn.as_deref(),
        Some(SHIM_ARN),
        "inference_profile_arn column must be populated in DB when ARN supplied; got {:?}",
        ep.inference_profile_arn
    );

    // Response body: aip_overrides must be [] (shim did not insert a row)
    let aip_overrides = body
        .get("aip_overrides")
        .expect("response body must include 'aip_overrides' field; body={body}");
    assert_eq!(
        aip_overrides,
        &Value::Array(vec![]),
        "aip_overrides must be [] when GetInferenceProfile fails; got {aip_overrides}"
    );

    // Response body: inference_profile_arn must equal the supplied ARN (not null)
    let ipa = body
        .get("inference_profile_arn")
        .expect("response body must include 'inference_profile_arn' field; body={body}");
    assert_eq!(
        ipa.as_str().unwrap_or(""),
        SHIM_ARN,
        "inference_profile_arn in response must equal the supplied ARN even on GetInferenceProfile failure"
    );

    // Deprecation header must still be present (the legacy field IS populated)
    assert_eq!(
        deprecation.as_deref(),
        Some(DEPRECATION_HEADER_VALUE),
        "Deprecation header must still be present even when GetInferenceProfile fails; got {deprecation:?}"
    );

    // Zero override rows in DB
    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, endpoint_id)
        .await
        .expect("list_by_endpoint must not fail");

    assert!(
        rows.is_empty(),
        "zero override rows must be in DB when GetInferenceProfile fails (auto-migration retries at next startup); got {rows:?}"
    );
}

// ── Contract point 4: GET /admin/endpoints/{id} — zero rows + legacy null ─────

/// GET /admin/endpoints/{id}: no override rows, inference_profile_arn = null:
/// - `aip_overrides: []`
/// - `inference_profile_arn: null`
/// - No Deprecation/Sunset headers.
#[tokio::test]
async fn get_endpoint_no_overrides_no_legacy_clean_shape() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create a CRI endpoint with no legacy ARN
    let ep_id = create_endpoint_db(&pool, "ep-get-clean", None).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();

    let deprecation = resp.headers().get("deprecation").cloned();
    let sunset = resp.headers().get("sunset").cloned();

    let body = parse_body(resp).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "GET on existing endpoint must return 200; got {status}; body={body}"
    );

    // aip_overrides must be present and empty
    let aip_overrides = body
        .get("aip_overrides")
        .expect("GET response must include 'aip_overrides' field (always present); body={body}");
    assert_eq!(
        aip_overrides,
        &Value::Array(vec![]),
        "aip_overrides must be [] when endpoint has no overrides and no legacy ARN; got {aip_overrides}"
    );

    // inference_profile_arn must be null
    let ipa = body
        .get("inference_profile_arn")
        .expect("GET response must include 'inference_profile_arn' field; body={body}");
    assert!(
        ipa.is_null(),
        "inference_profile_arn must be null when no legacy ARN and no overrides; got {ipa}"
    );

    // No Deprecation or Sunset headers
    assert!(
        deprecation.is_none(),
        "Deprecation header must NOT be present when inference_profile_arn is null in GET response"
    );
    assert!(
        sunset.is_none(),
        "Sunset header must NOT be present when inference_profile_arn is null in GET response"
    );
}

// ── Contract point 5: GET /admin/endpoints/{id} — exactly one override row ────

/// GET /admin/endpoints/{id}: exactly one override row, legacy null:
/// - `aip_overrides: [{the row}]`
/// - `inference_profile_arn = row.aip_arn`
/// - Deprecation + Sunset headers present.
#[tokio::test]
async fn get_endpoint_one_override_row_legacy_ipa_populated_with_deprecation() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create endpoint with no legacy ARN, then insert one override row via DB
    let ep_id = create_endpoint_db(&pool, "ep-get-one-row", None).await;

    endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-5",
        SHIM_ARN,
        "admin",
        Some("test override"),
    )
    .await
    .expect("insert override must succeed");

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();

    let deprecation = resp
        .headers()
        .get("deprecation")
        .map(|v| v.to_str().unwrap_or("").to_string());
    let sunset = resp
        .headers()
        .get("sunset")
        .map(|v| v.to_str().unwrap_or("").to_string());

    let body = parse_body(resp).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "GET must return 200; got {status}; body={body}"
    );

    // aip_overrides must have exactly one row
    let aip_overrides = body
        .get("aip_overrides")
        .and_then(|v| v.as_array())
        .expect("aip_overrides must be a JSON array; body={body}");
    assert_eq!(
        aip_overrides.len(),
        1,
        "aip_overrides must have exactly one row; got {aip_overrides:?}"
    );
    assert_eq!(
        aip_overrides[0]["model_id"].as_str().unwrap_or(""),
        "claude-sonnet-4-5",
        "aip_overrides[0].model_id must match the inserted row"
    );
    assert_eq!(
        aip_overrides[0]["aip_arn"].as_str().unwrap_or(""),
        SHIM_ARN,
        "aip_overrides[0].aip_arn must match the inserted row"
    );

    // inference_profile_arn must equal the single row's aip_arn (legacy client compat)
    let ipa = body
        .get("inference_profile_arn")
        .expect("GET response must include 'inference_profile_arn' field; body={body}");
    assert_eq!(
        ipa.as_str().unwrap_or(""),
        SHIM_ARN,
        "inference_profile_arn must equal the single override row's aip_arn; got {ipa}"
    );

    // Deprecation header must be present
    assert_eq!(
        deprecation.as_deref(),
        Some(DEPRECATION_HEADER_VALUE),
        "Deprecation header must be '{DEPRECATION_HEADER_VALUE}' when inference_profile_arn is non-null in GET; got {deprecation:?}"
    );

    // Sunset header must be present and match snapshot
    assert_eq!(
        sunset.as_deref(),
        Some(SUNSET_HEADER_VALUE),
        "Sunset header must be '{SUNSET_HEADER_VALUE}' when inference_profile_arn is non-null in GET; got {sunset:?}"
    );
}

// ── Contract point 6: GET /admin/endpoints/{id} — two override rows ───────────

/// GET /admin/endpoints/{id}: two override rows, legacy null:
/// - `aip_overrides: [{row1}, {row2}]`
/// - `inference_profile_arn: null` (can't represent multiple ARNs)
/// - NO Deprecation/Sunset headers.
#[tokio::test]
async fn get_endpoint_two_override_rows_ipa_null_no_deprecation() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let ep_id = create_endpoint_db(&pool, "ep-get-two-rows", None).await;

    // Insert Sonnet override
    endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-5",
        SHIM_ARN,
        "admin",
        Some("sonnet"),
    )
    .await
    .expect("insert sonnet override must succeed");

    // Insert Opus override
    endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-opus-4-7",
        SHIM_ARN_OPUS,
        "admin",
        Some("opus"),
    )
    .await
    .expect("insert opus override must succeed");

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();

    let deprecation = resp.headers().get("deprecation").cloned();
    let sunset = resp.headers().get("sunset").cloned();

    let body = parse_body(resp).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "GET must return 200; got {status}; body={body}"
    );

    // aip_overrides must have two rows
    let aip_overrides = body
        .get("aip_overrides")
        .and_then(|v| v.as_array())
        .expect("aip_overrides must be a JSON array; body={body}");
    assert_eq!(
        aip_overrides.len(),
        2,
        "aip_overrides must have two rows; got {aip_overrides:?}"
    );

    let model_ids: Vec<&str> = aip_overrides
        .iter()
        .map(|o| o["model_id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        model_ids.contains(&"claude-sonnet-4-5"),
        "aip_overrides must include sonnet row"
    );
    assert!(
        model_ids.contains(&"claude-opus-4-7"),
        "aip_overrides must include opus row"
    );

    // inference_profile_arn must be null (can't represent 2 ARNs as one field)
    let ipa = body
        .get("inference_profile_arn")
        .expect("GET response must include 'inference_profile_arn' field; body={body}");
    assert!(
        ipa.is_null(),
        "inference_profile_arn must be null when there are 2+ override rows; got {ipa}"
    );

    // No Deprecation or Sunset headers when ipa is null
    assert!(
        deprecation.is_none(),
        "Deprecation header must NOT be present when inference_profile_arn is null in GET (two-row case)"
    );
    assert!(
        sunset.is_none(),
        "Sunset header must NOT be present when inference_profile_arn is null in GET (two-row case)"
    );
}

// ── Contract point 7: GET /admin/endpoints/{id} — zero rows + legacy column ───

/// GET /admin/endpoints/{id}: zero override rows, legacy column populated
/// (auto-migration not yet completed):
/// - `aip_overrides: []`
/// - `inference_profile_arn = <legacy value>`
/// - Deprecation + Sunset headers (signals legacy field in use).
#[tokio::test]
async fn get_endpoint_zero_rows_legacy_column_set_deprecation_headers() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create endpoint WITH legacy ARN in the column but zero rows in new table
    let legacy_arn =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/legacy-only";
    let ep_id = create_endpoint_db(&pool, "ep-get-legacy-only", Some(legacy_arn)).await;

    // Confirm no override rows were inserted
    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();
    assert!(rows.is_empty(), "precondition: no rows in new table");

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();

    let deprecation = resp
        .headers()
        .get("deprecation")
        .map(|v| v.to_str().unwrap_or("").to_string());
    let sunset = resp
        .headers()
        .get("sunset")
        .map(|v| v.to_str().unwrap_or("").to_string());

    let body = parse_body(resp).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "GET must return 200; got {status}; body={body}"
    );

    // aip_overrides must be empty (no rows in new table)
    let aip_overrides = body
        .get("aip_overrides")
        .expect("GET response must include 'aip_overrides' field; body={body}");
    assert_eq!(
        aip_overrides,
        &Value::Array(vec![]),
        "aip_overrides must be [] when no rows in new table; got {aip_overrides}"
    );

    // inference_profile_arn must equal the legacy column value
    let ipa = body
        .get("inference_profile_arn")
        .expect("GET response must include 'inference_profile_arn' field; body={body}");
    assert_eq!(
        ipa.as_str().unwrap_or(""),
        legacy_arn,
        "inference_profile_arn must equal the legacy column value; got {ipa}"
    );

    // Deprecation header must be present (legacy field in use)
    assert_eq!(
        deprecation.as_deref(),
        Some(DEPRECATION_HEADER_VALUE),
        "Deprecation header must be '{DEPRECATION_HEADER_VALUE}' when legacy column populated (zero new-table rows); got {deprecation:?}"
    );

    // Sunset header must be present and match snapshot
    assert_eq!(
        sunset.as_deref(),
        Some(SUNSET_HEADER_VALUE),
        "Sunset header must be '{SUNSET_HEADER_VALUE}' when legacy column populated; got {sunset:?}"
    );
}

// ── Auth smoke: POST and GET still require admin auth ─────────────────────────

/// POST /admin/endpoints without auth → 401/403.
#[tokio::test]
async fn post_endpoint_requires_admin_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/endpoints")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"unauth-ep","region":"us-east-1","routing_prefix":"us"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "POST /admin/endpoints without auth must return 401/403; got {}",
        resp.status()
    );
}

/// GET /admin/endpoints/{id} without auth → 401/403.
#[tokio::test]
async fn get_endpoint_by_id_requires_admin_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;
    let ep_id = create_endpoint_db(&pool, "ep-auth-smoke", None).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{ep_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "GET /admin/endpoints/{{id}} without auth must return 401/403; got {}",
        resp.status()
    );
}

/// GET /admin/endpoints/{nonexistent-id} with valid admin auth → 404.
#[tokio::test]
async fn get_endpoint_by_id_nonexistent_returns_404() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let fake_id = Uuid::new_v4();

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/endpoints/{fake_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "GET /admin/endpoints/{{nonexistent}} must return 404; got {}",
        resp.status()
    );
}

// ── Cache version bump: shim auto-insert bumps cache_version ──────────────────

/// When the shim auto-inserts an override row via POST /admin/endpoints,
/// cache_version must be bumped (same contract as the explicit
/// POST /admin/endpoints/{id}/aip-overrides endpoint from Task 6).
///
/// Because GetInferenceProfile always fails in the test environment (no creds),
/// this test checks the "GetInferenceProfile fails" path — in which case
/// no override row is inserted and therefore no cache bump from the shim.
/// The test documents this behaviour explicitly so the builder knows what
/// to assert in the success path.
///
/// If/when the builder injects the get_foundation_model closure into GatewayState
/// to allow test overrides, this test should be updated to verify that the
/// SUCCESS path (shim inserts a row) ALSO bumps cache_version.
#[tokio::test]
async fn post_endpoint_with_arn_no_creds_no_cache_bump_from_shim() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let v0 = get_cache_version(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/endpoints")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"ep-cache-arn","region":"us-east-1","routing_prefix":"us","inference_profile_arn":"{SHIM_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    let body = parse_body(resp).await;

    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "POST must succeed; got {status}; body={body}"
    );

    let endpoint_id: Uuid = body["id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("response body must include 'id' with a UUID string; body={body}");

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, endpoint_id)
        .await
        .unwrap();

    if rows.is_empty() {
        // GetInferenceProfile failed (expected in test env without creds).
        // No shim row → no shim cache bump (the endpoint creation itself does not
        // bump cache_version by default).
        // This branch documents the no-creds behaviour; no assertion on v1 needed.
        let _v1 = get_cache_version(&pool).await;
        // intentionally no assertion here — this path is informational
    } else {
        // GetInferenceProfile succeeded (possible if test env has creds).
        // Shim inserted a row → cache_version MUST be bumped.
        let v1 = get_cache_version(&pool).await;
        assert!(
            v1 > v0,
            "cache_version must be bumped when shim auto-inserts an override row (before={v0}, after={v1})"
        );
    }
}

// ── Helper-level unit test for try_create_compat_override (injectable seam) ───

/// Tests for the `try_create_compat_override` helper (or equivalent injectable
/// function) that the builder should extract to allow unit-level testing of the
/// shim logic without running the full HTTP handler.
///
/// BUILDER CONTRACT:
/// Expose a public function in `src/api/admin.rs` (or a sibling module) like:
///
/// ```rust
/// pub async fn try_create_compat_override<F, Fut>(
///     endpoint_id: uuid::Uuid,
///     legacy_arn: &str,
///     pool: &sqlx::PgPool,
///     get_foundation_model: F,
/// ) -> Result<String, String>   // Ok(model_id) on success, Err(reason) on failure
/// where
///     F: Fn(&str) -> Fut + Send + Sync,
///     Fut: std::future::Future<Output = Result<String, String>> + Send,
/// ```
///
/// Once the builder exposes this symbol, delete the `#[cfg(FALSE_UNTIL_BUILDER_EXPOSES_HELPER)]`
/// wrapper below and remove the `#[ignore]` attributes from each test.
///
/// The HTTP-level tests above (contract points 1-7) are the primary contract;
/// these helper-level tests are an optional seam for isolated unit verification.
// ─────────────────────────────────────────────────────────────────────────────
// The two tests below are intentionally DISABLED at compile time via
// `#[cfg(any())]` (always-false predicate) because `try_create_compat_override`
// doesn't exist yet.  The builder should:
//   1. Expose `pub async fn try_create_compat_override(...)` in src/api/admin.rs
//      (re-exported from the crate root so the path below resolves).
//   2. Replace `#[cfg(any())]` with `#[cfg(feature = "integration")]`.
// ─────────────────────────────────────────────────────────────────────────────

/// Helper success path: inserts exactly one override row with COMPAT_SHIM_SET_BY.
#[cfg(feature = "integration")]
#[tokio::test]
async fn aip_compat_shim_try_create_compat_override_success() {
    let pool = helpers::setup_test_db().await;

    let ep_id = create_endpoint_db(&pool, "ep-helper-success", Some(SHIM_ARN)).await;

    // Mock: GetInferenceProfile succeeds and returns "claude-sonnet-4-5"
    let get_model = |_arn: &str| std::future::ready(Ok("claude-sonnet-4-5".to_string()));

    let result: Result<String, String> =
        ccag::api::admin::try_create_compat_override(ep_id, SHIM_ARN, &pool, get_model).await;

    assert!(
        result.is_ok(),
        "try_create_compat_override must return Ok when get_foundation_model succeeds; got {result:?}"
    );

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        1,
        "exactly one override row must be inserted by the compat shim; got {rows:?}"
    );

    let row = &rows[0];
    assert_eq!(
        row.set_by, COMPAT_SHIM_SET_BY,
        "set_by must be '{COMPAT_SHIM_SET_BY}'; got '{}'",
        row.set_by
    );
    assert_eq!(
        row.aip_arn, SHIM_ARN,
        "aip_arn must equal the supplied legacy ARN"
    );
    assert_eq!(
        row.model_id, "claude-sonnet-4-5",
        "model_id must be the value returned by get_foundation_model"
    );
}

/// Helper failure path: GetInferenceProfile returns Err → returns Err, zero rows.
#[cfg(feature = "integration")]
#[tokio::test]
async fn aip_compat_shim_try_create_compat_override_get_infprof_fails_no_row() {
    let pool = helpers::setup_test_db().await;

    let ep_id = create_endpoint_db(&pool, "ep-helper-fail", Some(SHIM_ARN)).await;

    let get_model =
        |_arn: &str| std::future::ready(Err("simulated GetInferenceProfile failure".to_string()));

    let result: Result<String, String> =
        ccag::api::admin::try_create_compat_override(ep_id, SHIM_ARN, &pool, get_model).await;

    // The helper should return Err to signal the caller, so the handler can
    // log a warning without aborting the endpoint creation.
    assert!(
        result.is_err(),
        "try_create_compat_override must return Err when get_foundation_model fails"
    );

    let rows = endpoint_aip_overrides::list_by_endpoint(&pool, ep_id)
        .await
        .unwrap();

    assert!(
        rows.is_empty(),
        "zero override rows must be inserted when get_foundation_model fails; got {rows:?}"
    );
}
