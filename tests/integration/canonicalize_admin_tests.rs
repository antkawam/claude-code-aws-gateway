/// Integration tests for Task 5: AIP override canonicalization + admin-add validation.
///
/// # What is tested
///
/// AC5.5 — `POST /admin/endpoints/{id}/aip-overrides` with a non-canonical
///          `model_id` that is NOT present in `model_mappings` returns 400
///          `invalid_request_error` with a body message containing the canonical form.
///
/// AC5.6 — Same endpoint with `model_id="claude-sonnet-4-6"` (canonical fixed point)
///          returns 200 or 201.
///
/// AC5.7 — Startup-time normalization pass: given a DB row in
///          `endpoint_aip_overrides` whose `model_id` is non-canonical
///          (e.g. `claude-sonnet-4-6-20250514`), the gateway's normalization scan
///          emits exactly one `tracing::info!` log line flagging the non-canonical
///          key. The row is preserved (count unchanged).
///
/// # BUILDER CONTRACT
///
/// ## AC5.5 / AC5.6
///
/// The Builder must update the `POST /admin/endpoints/{id}/aip-overrides` handler
/// in `src/api/admin.rs` to reject non-canonical `model_id` values with:
///
///   HTTP 400
///   {
///     "type": "error",
///     "error": {
///       "type": "invalid_request_error",
///       "message": "<human text> ... canonical form is `<canonical>`"
///     }
///   }
///
/// The check is: `canonicalize_model_id(model_id)` returns `Some(canonical)` AND
/// `canonical != model_id` AND no row with `model_id` exists in `model_mappings`.
/// If `model_id` IS a canonical fixed-point (`canonical == model_id`), accept it.
///
/// NOTE: the existing tests in `aip_overrides_admin_tests.rs` use `model_id =
/// "claude-sonnet-4-5"`, which the canonicalizer would also produce unchanged
/// (it is already canonical). After Task 5 those tests must continue to pass.
///
/// ## AC5.7
///
/// The Builder must expose a callable seam for the startup normalization scan.
/// Two acceptable forms:
///
///   A) `EndpointPool::scan_non_canonical_aip_overrides(pool: &sqlx::PgPool) -> ()`
///      (standalone async method on the pool struct, callable without AWS clients).
///
///   B) The scan runs inside `load_endpoints_inner` / `load_endpoints_with_db`
///      automatically; the test calls `load_endpoints_with_db` with a minimal
///      endpoint slice and observes the log output.
///
/// This test file implements Option A (preferred — no AWS SDK setup needed).
/// If the Builder chooses Option B, the `test_ac5_7_*` tests below will need
/// to be updated to call `load_endpoints_with_db` instead; the log-capture
/// scaffolding remains identical.
///
/// # Tracing capture
///
/// `tracing-test` is NOT yet a dev-dependency. Log capture is implemented by
/// installing a custom `tracing_subscriber::fmt` layer that writes to a
/// `Arc<Mutex<Vec<u8>>>` buffer for the duration of the test. The buffer is
/// inspected after the call under test completes.
///
/// # Run
///
/// ```
/// make test-integration
/// ```
/// (requires Docker Postgres on port 5433)
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::helpers;

// ── Shared test app factory (mirrors aip_overrides_admin_tests.rs) ─────────────

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

    let signing_key = "test-signing-key-canonicalize-admin";

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

/// Parse a response body as JSON.
async fn parse_body(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("failed to read response body");
    serde_json::from_slice(&bytes).expect("response body is not valid JSON")
}

const VALID_ARN: &str =
    "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/sonnet-tagged";

// ── AC5.5 — POST non-canonical model_id returns 400 ──────────────────────────

/// AC5.5: `POST /admin/endpoints/{id}/aip-overrides` with
/// `model_id="claude-sonnet-4-6-20250514"` (non-canonical, not in
/// `model_mappings`) must return 400 `invalid_request_error` whose body message
/// contains the canonical form `claude-sonnet-4-6`.
///
/// PRE-IMPLEMENTATION FAILURE MODE: assertion-fail. The current handler accepts
/// any `model_id` string without canonical validation and returns 200/201.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_5_post_non_canonical_model_id_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-ac5-5-non-canonical").await;

    // "claude-sonnet-4-6-20250514" is a dated variant: canonical form is
    // "claude-sonnet-4-6". It is not in model_mappings (no admin alias row).
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"claude-sonnet-4-6-20250514","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "POST with non-canonical model_id 'claude-sonnet-4-6-20250514' must return 400"
    );

    let body = parse_body(resp).await;

    // Error type must be "invalid_request_error"
    let error_type = body
        .pointer("/error/type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        error_type, "invalid_request_error",
        "error.type must be 'invalid_request_error', got body: {body}"
    );

    // Error message must name the canonical form
    let message = body
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        message.contains("claude-sonnet-4-6"),
        "error message must contain the canonical form 'claude-sonnet-4-6', \
         got message: {message:?}"
    );
}

/// AC5.5 (variant): `model_id="opus-4-6"` (auto-prefix form, non-canonical)
/// must also return 400 naming the canonical form `claude-opus-4-6`.
///
/// PRE-IMPLEMENTATION FAILURE MODE: assertion-fail.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_5_post_auto_prefix_form_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-ac5-5-auto-prefix").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"opus-4-6","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "POST with non-canonical model_id 'opus-4-6' must return 400"
    );

    let body = parse_body(resp).await;
    let message = body
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        message.contains("claude-opus-4-6"),
        "error message must contain the canonical form 'claude-opus-4-6', \
         got message: {message:?}"
    );
}

/// AC5.5 (variant): `model_id="Sonnet 4.7"` (canonicalizer rejects entirely)
/// must return 400. The canonicalizer returns `None` for this input, so the
/// handler has no canonical form to suggest; it should still reject with 400.
///
/// PRE-IMPLEMENTATION FAILURE MODE: assertion-fail.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_5_post_unrecognized_model_id_returns_400() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-ac5-5-unrecognized").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"Sonnet 4.7","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "POST with unrecognizable model_id 'Sonnet 4.7' must return 400"
    );

    let body = parse_body(resp).await;
    let error_type = body
        .pointer("/error/type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        error_type, "invalid_request_error",
        "error.type must be 'invalid_request_error', got body: {body}"
    );
}

// ── AC5.6 — POST canonical model_id returns 200/201 ──────────────────────────

/// AC5.6: `POST /admin/endpoints/{id}/aip-overrides` with
/// `model_id="claude-sonnet-4-6"` (canonical fixed point) returns 200 or 201.
///
/// `canonicalize_model_id("claude-sonnet-4-6")` returns
/// `Some("claude-sonnet-4-6")` — the fixed-point check passes because
/// `canonical == model_id`.
///
/// PRE-IMPLEMENTATION FAILURE MODE: this test PASSES against the current
/// (pre-Task-5) handler because it has no canonical check. It is a regression
/// guard ensuring the canonical-validation logic added in Task 5 correctly
/// identifies already-canonical inputs as valid.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_6_post_canonical_model_id_returns_200() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-ac5-6-canonical").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model_id":"claude-sonnet-4-6","aip_arn":"{VALID_ARN}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "POST with canonical model_id 'claude-sonnet-4-6' must return 200 or 201, \
         got {status}"
    );
}

/// AC5.6 (additional canonical forms): other canonical fixed-points must be
/// accepted.
///
/// PRE-IMPLEMENTATION FAILURE MODE: these pass pre-Task-5 and are regression guards.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_6_other_canonical_forms_accepted() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let ep_id = create_endpoint(&pool, "ep-ac5-6-other-canonical").await;

    let canonical_models = ["claude-opus-4-6", "claude-haiku-4-5", "claude-sonnet-4-5"];

    for (i, model_id) in canonical_models.iter().enumerate() {
        // Use different ARN suffix per row to avoid PK conflict on reuse
        let arn = format!(
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/model-{i}"
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/endpoints/{ep_id}/aip-overrides"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"model_id":"{model_id}","aip_arn":"{arn}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = resp.status();
        assert!(
            status == StatusCode::OK || status == StatusCode::CREATED,
            "POST with canonical model_id '{model_id}' must return 200 or 201, got {status}"
        );
    }
}

// ── AC5.7 — Startup normalization pass ────────────────────────────────────────

/// AC5.7: Given a DB containing one `endpoint_aip_overrides` row whose
/// `model_id` is non-canonical (e.g. `claude-sonnet-4-6-20250514`), the
/// startup normalization scan emits exactly one `tracing::info!` log line
/// flagging the non-canonical key. The row is preserved (count unchanged).
///
/// # BUILDER CONTRACT
///
/// The Builder must expose:
///
/// ```rust
/// impl ccag::endpoint::EndpointPool {
///     /// Scan all rows in `endpoint_aip_overrides`. For each row whose
///     /// `model_id` is not a canonical fixed-point, emit either
///     /// `tracing::warn!` (if a canonical-keyed sibling row already exists
///     /// for the same endpoint_id) or `tracing::info!` (otherwise).
///     /// Rows are never modified or deleted.
///     pub async fn scan_non_canonical_aip_overrides(pool: &sqlx::PgPool);
/// }
/// ```
///
/// The log line emitted for the non-canonical-key / no-sibling case MUST:
/// - Be at `INFO` level (not WARN — WARN is reserved for the duplicate-sibling case).
/// - Contain the string `"non-canonical"` OR the non-canonical model_id
///   (`claude-sonnet-4-6-20250514`) in the message.
///
/// # Tracing capture
///
/// `tracing-test` is not a dev-dependency. This test installs a custom
/// `tracing_subscriber::fmt` subscriber that writes to an in-memory buffer
/// for the duration of the call, then inspects the captured output.
///
/// PRE-IMPLEMENTATION FAILURE MODE: compile-fail because
/// `EndpointPool::scan_non_canonical_aip_overrides` does not yet exist.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_7_startup_scan_logs_non_canonical_key() {
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;

    // ── Shared write-buffer ───────────────────────────────────────────────────
    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = BufWriter(Arc::clone(&buf));

    // ── Install a test-scoped tracing subscriber ──────────────────────────────
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .finish();

    let pool = helpers::setup_test_db().await;

    // ── Insert fixture data ───────────────────────────────────────────────────
    // Create an endpoint so FK constraints are satisfied.
    let ep_id = ccag::db::endpoints::create_endpoint(
        &pool,
        "ep-ac5-7-startup",
        None,
        None,
        None,
        "us-east-1",
        "us",
        0,
    )
    .await
    .expect("create_endpoint failed")
    .id;

    // Insert a non-canonical override row directly (bypassing the handler's
    // canonical check, since that check is what Task 5 will add — this simulates
    // a pre-existing row from before the upgrade).
    ccag::db::endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-6-20250514", // non-canonical
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/legacy",
        "test-fixture",
        None,
    )
    .await
    .expect("insert non-canonical override failed");

    // Verify the row count before the scan.
    let count_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM endpoint_aip_overrides WHERE endpoint_id = $1")
            .bind(ep_id)
            .fetch_one(&pool)
            .await
            .expect("count query failed");
    assert_eq!(
        count_before, 1,
        "fixture: exactly one override row must exist before scan"
    );

    // ── Run the scan under the test subscriber ────────────────────────────────
    let _guard = tracing::subscriber::set_default(subscriber);
    ccag::endpoint::EndpointPool::scan_non_canonical_aip_overrides(&pool).await;
    drop(_guard);

    // ── Verify log output ─────────────────────────────────────────────────────
    let output = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();

    // The scan must emit at least one INFO line mentioning the non-canonical key.
    let has_info_line = output.lines().any(|line| {
        line.contains("INFO")
            && (line.contains("non-canonical") || line.contains("claude-sonnet-4-6-20250514"))
    });
    assert!(
        has_info_line,
        "startup scan must emit an INFO log line flagging the non-canonical key \
         'claude-sonnet-4-6-20250514'; captured output:\n{output}"
    );

    // ── Verify the row is preserved ───────────────────────────────────────────
    let count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM endpoint_aip_overrides WHERE endpoint_id = $1")
            .bind(ep_id)
            .fetch_one(&pool)
            .await
            .expect("count query failed");
    assert_eq!(
        count_after, count_before,
        "startup scan must NOT delete or modify rows: count changed from \
         {count_before} to {count_after}"
    );
}

/// AC5.7 (warn variant): given a DB with BOTH a non-canonical row AND a
/// canonical-keyed sibling row for the same endpoint, the scan emits WARN
/// (not INFO) for the non-canonical key.
///
/// PRE-IMPLEMENTATION FAILURE MODE: compile-fail (same as AC5.7 above).
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_7_startup_scan_warns_when_canonical_sibling_exists() {
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = BufWriter(Arc::clone(&buf));

    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();

    let pool = helpers::setup_test_db().await;

    let ep_id = ccag::db::endpoints::create_endpoint(
        &pool,
        "ep-ac5-7-warn",
        None,
        None,
        None,
        "us-east-1",
        "us",
        0,
    )
    .await
    .expect("create_endpoint failed")
    .id;

    // Insert both the non-canonical row and the canonical-keyed sibling.
    ccag::db::endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-6-20250514", // non-canonical
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/dated",
        "test-fixture",
        None,
    )
    .await
    .expect("insert non-canonical override failed");

    ccag::db::endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-6", // canonical sibling
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/canonical",
        "test-fixture",
        None,
    )
    .await
    .expect("insert canonical override failed");

    let _guard = tracing::subscriber::set_default(subscriber);
    ccag::endpoint::EndpointPool::scan_non_canonical_aip_overrides(&pool).await;
    drop(_guard);

    let output = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();

    // With a canonical sibling present, the scan must emit WARN (not just INFO).
    let has_warn_line = output.lines().any(|line| {
        line.contains("WARN")
            && (line.contains("claude-sonnet-4-6-20250514")
                || line.contains("non-canonical")
                || line.contains("conflict"))
    });
    assert!(
        has_warn_line,
        "startup scan must emit a WARN log line when a canonical-keyed sibling \
         already exists for the same endpoint; captured output:\n{output}"
    );

    // Both rows must still be present.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM endpoint_aip_overrides WHERE endpoint_id = $1")
            .bind(ep_id)
            .fetch_one(&pool)
            .await
            .expect("count query failed");
    assert_eq!(count, 2, "scan must preserve both rows, got {count}");
}

/// AC5.7 (all-canonical baseline): given a DB where all override rows are
/// already canonical, the scan emits NO info/warn log lines about non-canonical
/// keys (nothing to flag).
///
/// PRE-IMPLEMENTATION FAILURE MODE: compile-fail.
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_ac5_7_startup_scan_silent_for_all_canonical_rows() {
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = BufWriter(Arc::clone(&buf));

    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .finish();

    let pool = helpers::setup_test_db().await;

    let ep_id = ccag::db::endpoints::create_endpoint(
        &pool,
        "ep-ac5-7-all-canonical",
        None,
        None,
        None,
        "us-east-1",
        "us",
        0,
    )
    .await
    .expect("create_endpoint failed")
    .id;

    // Insert only canonical rows.
    ccag::db::endpoint_aip_overrides::insert(
        &pool,
        ep_id,
        "claude-sonnet-4-6",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/canonical-sonnet",
        "test-fixture",
        None,
    )
    .await
    .expect("insert canonical override failed");

    let _guard = tracing::subscriber::set_default(subscriber);
    ccag::endpoint::EndpointPool::scan_non_canonical_aip_overrides(&pool).await;
    drop(_guard);

    let output = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();

    // The scan must NOT emit any non-canonical-flagging lines.
    let has_flagging_line = output
        .lines()
        .any(|line| line.contains("non-canonical") || line.contains("claude-sonnet-4-6-20250514"));
    assert!(
        !has_flagging_line,
        "startup scan must emit no non-canonical-flagging log lines when all \
         rows are already canonical; captured output:\n{output}"
    );
}
