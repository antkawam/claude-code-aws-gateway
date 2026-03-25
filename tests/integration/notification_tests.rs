use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::helpers;
use ccag::budget::BudgetSpendCache;
use ccag::db;

// ============================================================
// DB CRUD: notification_config
// ============================================================

#[tokio::test]
async fn notification_config_upsert_draft() {
    let pool = helpers::setup_test_db().await;

    let cats = serde_json::json!(["budget"]);
    let draft =
        db::notification_config::upsert_draft(&pool, "webhook", "https://example.com/hook", &cats)
            .await
            .unwrap();

    assert_eq!(draft.slot, "draft");
    assert_eq!(draft.destination_type, "webhook");
    assert_eq!(draft.destination_value, "https://example.com/hook");

    // Verify get_draft returns it
    let fetched = db::notification_config::get_draft(&pool).await.unwrap();
    assert!(fetched.is_some());
    assert_eq!(fetched.unwrap().id, draft.id);

    // Verify get_active returns None
    let active = db::notification_config::get_active(&pool).await.unwrap();
    assert!(active.is_none());
}

#[tokio::test]
async fn notification_config_upsert_draft_overwrites() {
    let pool = helpers::setup_test_db().await;

    let cats = serde_json::json!(["budget"]);
    let draft1 = db::notification_config::upsert_draft(&pool, "webhook", "https://a.com", &cats)
        .await
        .unwrap();

    let draft2 = db::notification_config::upsert_draft(
        &pool,
        "sns",
        "arn:aws:sns:us-east-1:123456789012:topic",
        &cats,
    )
    .await
    .unwrap();

    // Same row ID (upsert)
    assert_eq!(draft1.id, draft2.id);
    assert_eq!(draft2.destination_type, "sns");
}

#[tokio::test]
async fn notification_config_activate() {
    let pool = helpers::setup_test_db().await;

    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com/hook", &cats)
        .await
        .unwrap();

    let active = db::notification_config::activate_draft(&pool)
        .await
        .unwrap();
    assert!(active.is_some());
    let active = active.unwrap();
    assert_eq!(active.slot, "active");
    assert_eq!(active.destination_type, "webhook");

    // Draft should be gone
    let draft = db::notification_config::get_draft(&pool).await.unwrap();
    assert!(draft.is_none());

    // Active should be fetchable
    let fetched = db::notification_config::get_active(&pool).await.unwrap();
    assert!(fetched.is_some());
}

#[tokio::test]
async fn notification_config_activate_replaces() {
    let pool = helpers::setup_test_db().await;
    let cats = serde_json::json!(["budget"]);

    // First cycle: draft → activate
    db::notification_config::upsert_draft(&pool, "webhook", "https://first.com", &cats)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();

    // Second cycle: new draft → activate (replaces previous active)
    db::notification_config::upsert_draft(
        &pool,
        "sns",
        "arn:aws:sns:us-east-1:123456789012:topic2",
        &cats,
    )
    .await
    .unwrap();
    let active = db::notification_config::activate_draft(&pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.destination_type, "sns");
    assert!(active.destination_value.contains("topic2"));
}

#[tokio::test]
async fn notification_config_delete_active() {
    let pool = helpers::setup_test_db().await;
    let cats = serde_json::json!(["budget"]);

    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com", &cats)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();

    let deleted = db::notification_config::delete_active(&pool).await.unwrap();
    assert!(deleted);

    let active = db::notification_config::get_active(&pool).await.unwrap();
    assert!(active.is_none());
}

#[tokio::test]
async fn notification_config_categories_update() {
    let pool = helpers::setup_test_db().await;
    let cats = serde_json::json!(["budget"]);

    // Create active config
    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com", &cats)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();

    // Update categories
    let new_cats = serde_json::json!(["budget", "rate_limit"]);
    let updated = db::notification_config::update_event_categories(&pool, &new_cats)
        .await
        .unwrap();
    assert!(updated);

    let active = db::notification_config::get_active(&pool)
        .await
        .unwrap()
        .unwrap();
    let categories = active.event_categories.as_array().unwrap();
    assert_eq!(categories.len(), 2);
}

#[tokio::test]
async fn notification_config_draft_does_not_affect_active() {
    let pool = helpers::setup_test_db().await;
    let cats = serde_json::json!(["budget"]);

    // Create active
    db::notification_config::upsert_draft(&pool, "webhook", "https://active.com", &cats)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();

    // Create new draft
    db::notification_config::upsert_draft(
        &pool,
        "sns",
        "arn:aws:sns:us-east-1:123456789012:new",
        &cats,
    )
    .await
    .unwrap();

    // Active unchanged
    let active = db::notification_config::get_active(&pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.destination_type, "webhook");
    assert_eq!(active.destination_value, "https://active.com");
}

#[tokio::test]
async fn notification_config_get_both() {
    let pool = helpers::setup_test_db().await;
    let cats = serde_json::json!(["budget"]);

    // Create active + draft
    db::notification_config::upsert_draft(&pool, "webhook", "https://active.com", &cats)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();
    db::notification_config::upsert_draft(
        &pool,
        "sns",
        "arn:aws:sns:us-east-1:123456789012:draft",
        &cats,
    )
    .await
    .unwrap();

    let (active, draft) = db::notification_config::get_both(&pool).await.unwrap();
    assert!(active.is_some());
    assert!(draft.is_some());
    assert_eq!(active.unwrap().destination_type, "webhook");
    assert_eq!(draft.unwrap().destination_type, "sns");
}

#[tokio::test]
async fn notification_config_test_result() {
    let pool = helpers::setup_test_db().await;
    let cats = serde_json::json!(["budget"]);

    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com", &cats)
        .await
        .unwrap();

    db::notification_config::update_test_result(&pool, "draft", true, None)
        .await
        .unwrap();

    let draft = db::notification_config::get_draft(&pool)
        .await
        .unwrap()
        .unwrap();
    assert!(draft.last_tested_at.is_some());
    assert_eq!(draft.last_test_success, Some(true));
    assert!(draft.last_test_error.is_none());
}

// ============================================================
// DB CRUD: delivery_log
// ============================================================

#[tokio::test]
async fn delivery_log_insert_and_query() {
    let pool = helpers::setup_test_db().await;

    let payload = serde_json::json!({"event_type": "budget_warning"});
    db::notification_config::log_delivery(
        &pool,
        None,
        "webhook",
        "https://example.com",
        "budget_warning",
        &payload,
        "success",
        None,
        150,
    )
    .await
    .unwrap();

    db::notification_config::log_delivery(
        &pool,
        None,
        "webhook",
        "https://example.com",
        "budget_blocked",
        &payload,
        "failure",
        Some("timeout"),
        10000,
    )
    .await
    .unwrap();

    let entries = db::notification_config::get_recent_deliveries(&pool, 10)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);

    // Most recent first
    assert_eq!(entries[0].event_type, "budget_blocked");
    assert_eq!(entries[0].status, "failure");
    assert_eq!(entries[0].error_message.as_deref(), Some("timeout"));
    assert_eq!(entries[1].event_type, "budget_warning");
    assert_eq!(entries[1].status, "success");
}

#[tokio::test]
async fn delivery_log_prune() {
    let pool = helpers::setup_test_db().await;

    let payload = serde_json::json!({"test": true});
    for i in 0..20 {
        db::notification_config::log_delivery(
            &pool,
            None,
            "webhook",
            "https://example.com",
            &format!("event_{i}"),
            &payload,
            "success",
            None,
            100,
        )
        .await
        .unwrap();
    }

    let before = db::notification_config::get_recent_deliveries(&pool, 100)
        .await
        .unwrap();
    assert_eq!(before.len(), 20);

    let pruned = db::notification_config::prune_delivery_log(&pool, 10)
        .await
        .unwrap();
    assert_eq!(pruned, 10);

    let after = db::notification_config::get_recent_deliveries(&pool, 100)
        .await
        .unwrap();
    assert_eq!(after.len(), 10);
}

// ============================================================
// Admin API: notification endpoints
// ============================================================

/// Build a test router (same pattern as admin_tests.rs)
async fn test_app(pool: &sqlx::PgPool) -> (axum::Router, String) {
    let config = ccag::config::GatewayConfig {
        host: "127.0.0.1".to_string(),
        port: 9999,
        admin_username: "admin".to_string(),
        admin_password: "admin".to_string(),
        bedrock_routing_prefix: "us".to_string(),
        database_url: "postgres://test@localhost/test".to_string(),
        admin_users: vec![],
        notification_url: Some("https://env-fallback.example.com".to_string()),
        rds_iam_auth: false,
        database_host: None,
        database_port: 5432,
        database_name: "test".to_string(),
        database_user: "test".to_string(),
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
    });

    let router = ccag::api::router(state);
    (router, admin_token)
}

fn member_token(sub: &str) -> String {
    let identity = ccag::auth::oidc::OidcIdentity {
        sub: sub.to_string(),
        email: None,
        idp_name: "Local".to_string(),
    };
    ccag::auth::session::issue("test-signing-key-for-integration-tests", &identity, 24)
}

async fn parse_body(resp: axum::http::Response<Body>) -> serde_json::Value {
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn admin_get_notification_config_empty() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert!(json["active"].is_null());
    assert!(json["draft"].is_null());
    assert_eq!(
        json["env_fallback"].as_str(),
        Some("https://env-fallback.example.com")
    );
    assert!(json["delivery_history"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn admin_put_notification_config_webhook() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"destination_type":"webhook","destination_value":"https://hooks.example.com"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["slot"].as_str(), Some("draft"));
    assert_eq!(json["destination_type"].as_str(), Some("webhook"));
}

#[tokio::test]
async fn admin_put_notification_config_sns() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"destination_type":"sns","destination_value":"arn:aws:sns:us-east-1:123456789012:my-topic"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["destination_type"].as_str(), Some("sns"));
}

#[tokio::test]
async fn admin_put_notification_config_eventbridge() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"destination_type":"eventbridge","destination_value":"arn:aws:events:us-east-1:123456789012:event-bus/my-bus"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["destination_type"].as_str(), Some("eventbridge"));
}

#[tokio::test]
async fn admin_put_notification_config_invalid_type() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"destination_type":"email","destination_value":"foo@bar.com"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_put_notification_config_invalid_webhook_url() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"destination_type":"webhook","destination_value":"http://insecure.com"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_put_notification_config_invalid_sns_arn() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"destination_type":"sns","destination_value":"not-an-arn"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_test_webhook_with_wiremock() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let mock_server = MockServer::start().await;

    // Setup mock webhook
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    // Note: wiremock uses http, but our validation requires https.
    // We'll save the draft directly to DB to bypass URL validation.
    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(
        &pool,
        "webhook",
        &format!("{}/webhook", mock_server.uri()),
        &cats,
    )
    .await
    .unwrap();

    // Test the draft
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/notifications/test")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["success"].as_bool(), Some(true));
    assert!(json["duration_ms"].as_i64().is_some());
}

#[tokio::test]
async fn admin_test_webhook_failure() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;
    let mock_server = MockServer::start().await;

    // Mock returns 500
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&mock_server)
        .await;

    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(
        &pool,
        "webhook",
        &format!("{}/webhook", mock_server.uri()),
        &cats,
    )
    .await
    .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/notifications/test")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["success"].as_bool(), Some(false));
    assert!(json["error"].as_str().unwrap().contains("500"));
}

#[tokio::test]
async fn admin_activate_after_test() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Save draft via DB (bypass https validation for test)
    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com/hook", &cats)
        .await
        .unwrap();

    // Mark test as successful
    db::notification_config::update_test_result(&pool, "draft", true, None)
        .await
        .unwrap();

    // Activate
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/notifications/activate")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["slot"].as_str(), Some("active"));
}

#[tokio::test]
async fn admin_activate_without_test() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Save draft without testing
    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com/hook", &cats)
        .await
        .unwrap();

    // Try to activate — should fail
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/notifications/activate")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_delete_notification_config() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create and activate
    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com", &cats)
        .await
        .unwrap();
    db::notification_config::update_test_result(&pool, "draft", true, None)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();

    // DELETE (deactivate)
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["active_deleted"].as_bool(), Some(true));

    // Verify gone
    let active = db::notification_config::get_active(&pool).await.unwrap();
    assert!(active.is_none());
}

#[tokio::test]
async fn admin_notification_requires_admin_role() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    // Create member
    let _ = ccag::db::users::create_user(&pool, "member@test.com", None, "member").await;
    let token = member_token("member@test.com");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/notifications/config")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_update_categories() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create active config
    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(&pool, "webhook", "https://example.com", &cats)
        .await
        .unwrap();
    db::notification_config::update_test_result(&pool, "draft", true, None)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();

    // Update categories via API
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/notifications/categories")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"event_categories":["budget","rate_limit"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let active = db::notification_config::get_active(&pool)
        .await
        .unwrap()
        .unwrap();
    let categories: Vec<String> = active
        .event_categories
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(categories.contains(&"budget".to_string()));
    assert!(categories.contains(&"rate_limit".to_string()));
}

#[tokio::test]
async fn admin_discard_draft() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create active + draft
    let cats = serde_json::json!(["budget"]);
    db::notification_config::upsert_draft(&pool, "webhook", "https://active.com", &cats)
        .await
        .unwrap();
    db::notification_config::update_test_result(&pool, "draft", true, None)
        .await
        .unwrap();
    db::notification_config::activate_draft(&pool)
        .await
        .unwrap();
    db::notification_config::upsert_draft(
        &pool,
        "sns",
        "arn:aws:sns:us-east-1:123456789012:draft",
        &cats,
    )
    .await
    .unwrap();

    // Discard draft
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/notifications/draft")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = parse_body(resp).await;
    assert_eq!(json["draft_deleted"].as_bool(), Some(true));

    // Draft gone, active still exists
    let draft = db::notification_config::get_draft(&pool).await.unwrap();
    assert!(draft.is_none());
    let active = db::notification_config::get_active(&pool).await.unwrap();
    assert!(active.is_some());
    assert_eq!(active.unwrap().destination_value, "https://active.com");
}
