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

/// Issue a member-role session token.
fn member_token(signing_key: &str, sub: &str) -> String {
    let identity = ccag::auth::oidc::OidcIdentity {
        sub: sub.to_string(),
        email: None,
        idp_name: "Local".to_string(),
    };
    ccag::auth::session::issue(signing_key, &identity, 24)
}

// ============================================================
// Unauthenticated requests
// ============================================================

#[tokio::test]
async fn admin_requires_auth() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================
// Teams CRUD
// ============================================================

#[tokio::test]
async fn admin_teams_crud() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create team
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/teams")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"test-team"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let team_id = json["id"].as_str().unwrap().to_string();

    // List teams
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/teams")
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
    assert_eq!(json["teams"].as_array().unwrap().len(), 1);

    // Delete team
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/admin/teams/{team_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Users CRUD
// ============================================================

#[tokio::test]
async fn admin_users_crud() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create user
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/users")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"email":"newuser@test.com","role":"member"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id = json["id"].as_str().unwrap().to_string();

    // List users (should include the admin auto-created + our new user)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/users")
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
    assert!(json["users"].as_array().unwrap().len() >= 2);

    // Update user role
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/users/{user_id}"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"role":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete user
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/admin/users/{user_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Keys CRUD via HTTP
// ============================================================

#[tokio::test]
async fn admin_keys_crud() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create key
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/keys")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"http-key"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let key_id = json["id"].as_str().unwrap().to_string();
    assert!(json["key"].as_str().unwrap().starts_with("sk-proxy-"));

    // List keys
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/keys")
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
    assert_eq!(json["keys"].as_array().unwrap().len(), 1);

    // Revoke key
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/keys/{key_id}/revoke"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete key
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/admin/keys/{key_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Permission enforcement: member cannot access admin-only endpoints
// ============================================================

#[tokio::test]
async fn member_cannot_create_team() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    // Create a member user in DB
    let _ = ccag::db::users::create_user(&pool, "member@test.com", None, "member").await;
    let token = member_token("test-signing-key-for-integration-tests", "member@test.com");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/teams")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"sneaky-team"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn member_cannot_list_all_users() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let _ = ccag::db::users::create_user(&pool, "member2@test.com", None, "member").await;
    let token = member_token("test-signing-key-for-integration-tests", "member2@test.com");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/users")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ============================================================
// Member can create and manage own keys
// ============================================================

#[tokio::test]
async fn member_can_create_own_key() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let _ = ccag::db::users::create_user(&pool, "keyowner@test.com", None, "member").await;
    let token = member_token(
        "test-signing-key-for-integration-tests",
        "keyowner@test.com",
    );

    // Member creates a key — should succeed
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/keys")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"my-key"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Member lists keys — should see only their own
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/keys")
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
    let keys = json["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "my-key");
}

// ============================================================
// Settings via HTTP
// ============================================================

#[tokio::test]
async fn admin_settings_get_and_update() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Get settings (should be empty initially)
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

    // Update a setting (use a known setting key)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/session_token_ttl_hours")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"value":"48"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify the setting was persisted
    let val = ccag::db::settings::get_setting(&pool, "session_token_ttl_hours")
        .await
        .unwrap();
    assert_eq!(val.as_deref(), Some("48"));
}

// ============================================================
// Health endpoint (no auth required)
// ============================================================

#[tokio::test]
async fn health_endpoint() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

// ============================================================
// Auth login
// ============================================================

#[tokio::test]
async fn auth_login_success() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"username":"admin","password":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["token"].as_str().is_some());
    assert_eq!(json["role"], "admin");
}

#[tokio::test]
async fn auth_login_failure() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"username":"admin","password":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK); // returns 200 with error body
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["error"].is_object());
}

// ============================================================
// Budget: team budget CRUD via API
// ============================================================

#[tokio::test]
async fn admin_set_team_budget_preset() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create team first
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/teams")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"budget-team"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let team_id = json["id"].as_str().unwrap().to_string();

    // Set budget with preset policy (budget_policy accepts a preset string)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/teams/{team_id}/budget"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"budget_amount_usd":1000,"budget_period":"monthly","budget_policy":"standard"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["updated"], true);

    // Verify the budget was persisted via DB
    let team_uuid: uuid::Uuid = team_id.parse().unwrap();
    let team = ccag::db::teams::get_team(&pool, team_uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(team.budget_amount_usd, Some(1000.0));
    assert_eq!(team.budget_period, "monthly");
    let policy: Vec<ccag::budget::PolicyRule> =
        serde_json::from_value(team.budget_policy.unwrap()).unwrap();
    assert_eq!(policy.len(), 2);
    assert_eq!(policy[0].at_percent, 80);
    assert_eq!(policy[1].at_percent, 100);
    assert_eq!(policy[1].action, ccag::budget::PolicyAction::Block);
}

#[tokio::test]
async fn admin_set_team_budget_custom_policy() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create team
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/teams")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"custom-team"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let team_id = json["id"].as_str().unwrap().to_string();

    // Set budget with custom rules (budget_policy accepts an array of rules)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/teams/{team_id}/budget"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "budget_amount_usd": 500,
                        "budget_period": "weekly",
                        "budget_policy": [
                            {"at_percent": 50, "action": "notify"},
                            {"at_percent": 90, "action": "shape", "shaped_rpm": 3},
                            {"at_percent": 120, "action": "block"}
                        ],
                        "default_user_budget_usd": 75,
                        "notify_recipients": "admin"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify persisted values
    let team_uuid: uuid::Uuid = team_id.parse().unwrap();
    let team = ccag::db::teams::get_team(&pool, team_uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(team.budget_amount_usd, Some(500.0));
    assert_eq!(team.budget_period, "weekly");
    assert_eq!(team.default_user_budget_usd, Some(75.0));
    assert_eq!(team.notify_recipients, "admin");
    let policy: Vec<ccag::budget::PolicyRule> =
        serde_json::from_value(team.budget_policy.unwrap()).unwrap();
    assert_eq!(policy.len(), 3);
}

#[tokio::test]
async fn admin_set_team_budget_invalid_policy() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create team
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/teams")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"invalid-team"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let team_id = json["id"].as_str().unwrap().to_string();

    // Block not last — should fail validation
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/teams/{team_id}/budget"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "budget_amount_usd": 500,
                        "budget_policy": [
                            {"at_percent": 50, "action": "block"},
                            {"at_percent": 100, "action": "notify"}
                        ]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================
// Budget: analytics overview API
// ============================================================

#[tokio::test]
async fn admin_analytics_overview() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create a team with budget
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/teams")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"analytics-team"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let team_id = json["id"].as_str().unwrap().to_string();

    // Set budget
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/teams/{team_id}/budget"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"budget_amount_usd":2000,"budget_policy":"standard"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Get overview
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/analytics/overview")
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
    let teams = json["teams"].as_array().unwrap();
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0]["team_name"], "analytics-team");
    assert_eq!(teams[0]["budget_amount_usd"], 2000.0);
}

// ============================================================
// Budget: team analytics detail API
// ============================================================

#[tokio::test]
async fn admin_team_analytics_detail() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Create team and user
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/teams")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"detail-api-team"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let team_id = json["id"].as_str().unwrap().to_string();

    // Add user to team
    let team_uuid: uuid::Uuid = team_id.parse().unwrap();
    let _ =
        ccag::db::users::create_user(&pool, "detail-api@test.com", Some(team_uuid), "member").await;

    // Get team analytics
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/admin/teams/{team_id}/analytics"))
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
    let users = json["users"].as_array().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["email"], "detail-api@test.com");
}

// ============================================================
// Budget: CSV export API
// ============================================================

#[tokio::test]
async fn admin_analytics_export_csv() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // Insert some spend data
    let entries = vec![helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("csv-user@test.com"),
    )];
    ccag::db::spend::insert_batch(&pool, &entries)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/analytics/export?days=7")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Check content type is CSV
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.contains("text/csv"),
        "Expected CSV content type, got {ct}"
    );

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let csv_text = String::from_utf8(body.to_vec()).unwrap();
    // Should have header + at least 1 data row
    let lines: Vec<&str> = csv_text.lines().collect();
    assert!(lines.len() >= 2, "CSV should have header + data rows");
    assert!(
        lines[0].contains("recorded_at"),
        "CSV header should contain recorded_at"
    );
}

// ============================================================
// Budget: member cannot access admin budget endpoints
// ============================================================

#[tokio::test]
async fn member_cannot_set_budget() {
    let pool = helpers::setup_test_db().await;
    let (app, _) = test_app(&pool).await;

    // Create team and member
    let team = helpers::create_test_team(&pool, "member-budget-team").await;
    let _ = ccag::db::users::create_user(&pool, "budget-member@test.com", Some(team.id), "member")
        .await;
    let token = member_token(
        "test-signing-key-for-integration-tests",
        "budget-member@test.com",
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/admin/teams/{}/budget", team.id))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"budget_amount_usd":100}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ============================================================
// Default Budget Policy
// ============================================================

#[tokio::test]
async fn default_budget_lifecycle() {
    let pool = helpers::setup_test_db().await;
    let (app, token) = test_app(&pool).await;

    // --- Phase 1: GET empty defaults ---
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/settings/default-budget")
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
    assert!(json["budget_amount_usd"].is_null(), "initially null");
    assert_eq!(json["budget_period"], "monthly");
    assert!(json["budget_policy"].is_null(), "initially null");

    // --- Phase 2: SET with "standard" preset ---
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"budget_amount_usd":100,"budget_period":"monthly","budget_policy":"standard","notify_recipients":"admin"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["updated"], true);

    // GET should reflect what was set
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/settings/default-budget")
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
    assert_eq!(json["budget_amount_usd"], 100.0);
    assert_eq!(json["budget_period"], "monthly");
    assert!(json["budget_policy"].is_array());
    let policy = json["budget_policy"].as_array().unwrap();
    // "standard" preset: notify at 80%, block at 100%
    assert_eq!(policy.len(), 2);
    assert_eq!(policy[0]["at_percent"], 80);
    assert_eq!(policy[0]["action"], "notify");
    assert_eq!(policy[1]["at_percent"], 100);
    assert_eq!(policy[1]["action"], "block");
    assert_eq!(json["notify_recipients"], "admin");

    // --- Phase 3: SET with "soft" preset ---
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"budget_amount_usd":200,"budget_policy":"soft"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let policy = json["budget_policy"].as_array().unwrap();
    // soft preset: notify 80%, notify 100%, block 150%
    assert_eq!(policy.len(), 3);
    assert_eq!(policy[0]["action"], "notify");
    assert_eq!(policy[1]["action"], "notify");
    assert_eq!(policy[2]["action"], "block");
    assert_eq!(policy[2]["at_percent"], 150);

    // --- Phase 4: SET with "shaped" preset ---
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"budget_amount_usd":300,"budget_policy":"shaped"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let policy = json["budget_policy"].as_array().unwrap();
    let has_shape = policy.iter().any(|r| r["action"] == "shape");
    assert!(has_shape, "shaped preset should include a shape action");

    // --- Phase 5: Invalid policy rejects ---
    // Invalid preset name
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"budget_amount_usd":100,"budget_policy":"nonexistent"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Invalid custom policy (block not last)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"budget_amount_usd":100,"budget_policy":[{"at_percent":50,"action":"block"},{"at_percent":100,"action":"notify"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // --- Phase 6: Clear budget ---
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"budget_amount_usd":null}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["budget_amount_usd"].is_null(), "cleared to null");

    // --- Phase 7: Affected user count ---
    let team = helpers::create_test_team(&pool, "count-team").await;
    helpers::create_test_user(&pool, "assigned@test.com", Some(team.id), "member").await;
    helpers::create_test_user(&pool, "unassigned1@test.com", None, "member").await;
    helpers::create_test_user(&pool, "unassigned2@test.com", None, "member").await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/settings/default-budget")
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
    // admin user (created by test_app) + 2 unassigned users = 3
    let count = json["affected_user_count"].as_i64().unwrap();
    assert_eq!(count, 3, "should count only users without a team");

    // --- Phase 8: RBAC — member cannot modify default budget ---
    let member_tok = member_token(
        "test-signing-key-for-integration-tests",
        "unassigned1@test.com",
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/admin/settings/default-budget")
                .header("authorization", format!("Bearer {member_tok}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"budget_amount_usd":999}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
