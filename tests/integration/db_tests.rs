use crate::helpers;
use ccag::budget::BudgetPeriod;
use ccag::db;

// ============================================================
// Keys
// ============================================================

#[tokio::test]
async fn keys_create_and_list() {
    let pool = helpers::setup_test_db().await;
    let (raw, vk) = helpers::create_test_key(&pool, Some("k1"), None, None).await;

    assert!(raw.starts_with("sk-proxy-"));
    assert_eq!(vk.name.as_deref(), Some("k1"));
    assert!(vk.is_active);

    let keys = db::keys::list_keys(&pool).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].id, vk.id);
}

#[tokio::test]
async fn keys_get_by_hash() {
    let pool = helpers::setup_test_db().await;
    let (raw, vk) = helpers::create_test_key(&pool, Some("h1"), None, None).await;

    let hash = db::keys::hash_key(&raw);
    assert_eq!(hash, vk.key_hash);

    let active = db::keys::get_active_keys(&pool).await.unwrap();
    assert!(active.iter().any(|k| k.key_hash == hash));
}

#[tokio::test]
async fn keys_revoke() {
    let pool = helpers::setup_test_db().await;
    let (_, vk) = helpers::create_test_key(&pool, Some("revoke-me"), None, None).await;

    assert!(db::keys::revoke_key(&pool, vk.id).await.unwrap());

    let active = db::keys::get_active_keys(&pool).await.unwrap();
    assert!(
        active.is_empty(),
        "Revoked key should not appear in active keys"
    );

    let all = db::keys::list_keys(&pool).await.unwrap();
    assert_eq!(all.len(), 1);
    assert!(!all[0].is_active);
}

#[tokio::test]
async fn keys_delete() {
    let pool = helpers::setup_test_db().await;
    let (_, vk) = helpers::create_test_key(&pool, Some("del-me"), None, None).await;

    assert!(db::keys::delete_key(&pool, vk.id).await.unwrap());

    let all = db::keys::list_keys(&pool).await.unwrap();
    assert!(all.is_empty());
}

#[tokio::test]
async fn keys_delete_nonexistent() {
    let pool = helpers::setup_test_db().await;
    let fake_id = uuid::Uuid::new_v4();
    assert!(!db::keys::delete_key(&pool, fake_id).await.unwrap());
}

#[tokio::test]
async fn keys_with_user_and_team() {
    let pool = helpers::setup_test_db().await;
    let team = helpers::create_test_team(&pool, "t1").await;
    let user = helpers::create_test_user(&pool, "u@test.com", Some(team.id), "member").await;

    let (_, vk) =
        helpers::create_test_key(&pool, Some("owned"), Some(user.id), Some(team.id)).await;
    assert_eq!(vk.user_id, Some(user.id));
    assert_eq!(vk.team_id, Some(team.id));
}

#[tokio::test]
async fn keys_with_rate_limit() {
    let pool = helpers::setup_test_db().await;
    let (_, vk) = db::keys::create_key(&pool, Some("rl-key"), None, None, Some(60))
        .await
        .unwrap();
    assert_eq!(vk.rate_limit_rpm, Some(60));
}

// ============================================================
// Users
// ============================================================

#[tokio::test]
async fn users_create_and_list() {
    let pool = helpers::setup_test_db().await;
    let user = helpers::create_test_user(&pool, "alice@test.com", None, "admin").await;
    assert_eq!(user.email, "alice@test.com");
    assert_eq!(user.role, "admin");

    let users = db::users::list_users(&pool).await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].id, user.id);
}

#[tokio::test]
async fn users_get_by_email() {
    let pool = helpers::setup_test_db().await;
    let user = helpers::create_test_user(&pool, "bob@test.com", None, "member").await;

    let found = db::users::get_user_by_email(&pool, "bob@test.com")
        .await
        .unwrap();
    assert_eq!(found.unwrap().id, user.id);

    let missing = db::users::get_user_by_email(&pool, "nobody@test.com")
        .await
        .unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn users_update_role() {
    let pool = helpers::setup_test_db().await;
    let user = helpers::create_test_user(&pool, "carol@test.com", None, "member").await;

    assert!(
        db::users::update_user_role(&pool, user.id, "admin")
            .await
            .unwrap()
    );

    let updated = db::users::get_user_by_email(&pool, "carol@test.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.role, "admin");
}

#[tokio::test]
async fn users_delete() {
    let pool = helpers::setup_test_db().await;
    let user = helpers::create_test_user(&pool, "del@test.com", None, "member").await;

    assert!(db::users::delete_user(&pool, user.id).await.unwrap());
    assert!(
        db::users::get_user_by_email(&pool, "del@test.com")
            .await
            .unwrap()
            .is_none()
    );
}

// ============================================================
// Teams
// ============================================================

#[tokio::test]
async fn teams_create_and_list() {
    let pool = helpers::setup_test_db().await;
    let team = helpers::create_test_team(&pool, "engineering").await;
    assert_eq!(team.name, "engineering");

    let teams = db::teams::list_teams(&pool).await.unwrap();
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0].id, team.id);
}

#[tokio::test]
async fn teams_delete() {
    let pool = helpers::setup_test_db().await;
    let team = helpers::create_test_team(&pool, "to-delete").await;

    assert!(db::teams::delete_team(&pool, team.id).await.unwrap());

    let teams = db::teams::list_teams(&pool).await.unwrap();
    assert!(teams.is_empty());
}

#[tokio::test]
async fn teams_user_association() {
    let pool = helpers::setup_test_db().await;
    let team = helpers::create_test_team(&pool, "my-team").await;
    let user = helpers::create_test_user(&pool, "member@test.com", Some(team.id), "member").await;
    assert_eq!(user.team_id, Some(team.id));
}

// ============================================================
// Settings
// ============================================================

#[tokio::test]
async fn settings_get_set() {
    let pool = helpers::setup_test_db().await;

    // Initially no setting
    let val = db::settings::get_setting(&pool, "test_key").await.unwrap();
    assert!(val.is_none());

    // Set it
    db::settings::set_setting(&pool, "test_key", "test_value")
        .await
        .unwrap();
    let val = db::settings::get_setting(&pool, "test_key").await.unwrap();
    assert_eq!(val.as_deref(), Some("test_value"));

    // Update it (upsert)
    db::settings::set_setting(&pool, "test_key", "new_value")
        .await
        .unwrap();
    let val = db::settings::get_setting(&pool, "test_key").await.unwrap();
    assert_eq!(val.as_deref(), Some("new_value"));
}

#[tokio::test]
async fn settings_cache_version() {
    let pool = helpers::setup_test_db().await;

    let v1 = db::settings::get_cache_version(&pool).await.unwrap();
    db::settings::bump_cache_version(&pool).await.unwrap();
    let v2 = db::settings::get_cache_version(&pool).await.unwrap();
    assert_eq!(v2, v1 + 1);
}

// ============================================================
// Spend
// ============================================================

#[tokio::test]
async fn spend_batch_insert() {
    let pool = helpers::setup_test_db().await;

    let entries = vec![
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("user1@test.com")),
        helpers::make_spend_entry("claude-haiku-4-5-20251001", Some("user1@test.com")),
    ];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    // Verify rows were inserted
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM spend_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 2);
}

#[tokio::test]
async fn spend_empty_batch() {
    let pool = helpers::setup_test_db().await;
    db::spend::insert_batch(&pool, &[]).await.unwrap();
}

// ============================================================
// Sessions
// ============================================================

#[tokio::test]
async fn sessions_materialize() {
    let pool = helpers::setup_test_db().await;

    // Insert spend_log entries with a session_id (need >= 2 for materialization)
    let mut e1 = helpers::make_spend_entry("claude-sonnet-4-20250514", Some("user@test.com"));
    e1.session_id = Some("sess-001".to_string());
    e1.project_key = Some("proj-a".to_string());
    e1.tool_names = vec!["Read".to_string()];

    let mut e2 = helpers::make_spend_entry("claude-sonnet-4-20250514", Some("user@test.com"));
    e2.session_id = Some("sess-001".to_string());
    e2.project_key = Some("proj-a".to_string());
    e2.tool_names = vec!["Edit".to_string(), "Bash".to_string()];

    let mut e3 = helpers::make_spend_entry("claude-haiku-4-5-20251001", Some("user@test.com"));
    e3.session_id = Some("sess-001".to_string());
    e3.project_key = Some("proj-a".to_string());

    db::spend::insert_batch(&pool, &[e1, e2, e3]).await.unwrap();

    let count = db::sessions::materialize_sessions(&pool).await.unwrap();
    assert_eq!(count, 1);

    let sessions = db::sessions::list_sessions(&pool, "user@test.com", 10)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "sess-001");
    assert_eq!(sessions[0].request_count, 3);
    assert_eq!(sessions[0].project_key.as_deref(), Some("proj-a"));
}

#[tokio::test]
async fn sessions_unanalyzed() {
    let pool = helpers::setup_test_db().await;

    // Insert 3 spend_log entries with detection flags (need >= 3 requests AND flag_count > 0)
    let mut entries = Vec::new();
    for _ in 0..3 {
        let mut e = helpers::make_spend_entry("claude-sonnet-4-20250514", Some("user@test.com"));
        e.session_id = Some("sess-unanalyzed".to_string());
        e.detection_flags =
            Some(serde_json::json!([{"category":"test","rule":"test_rule","severity":"low"}]));
        entries.push(e);
    }
    db::spend::insert_batch(&pool, &entries).await.unwrap();
    db::sessions::materialize_sessions(&pool).await.unwrap();

    let unanalyzed = db::sessions::get_unanalyzed_sessions(&pool, 10)
        .await
        .unwrap();
    assert_eq!(unanalyzed.len(), 1);
    assert_eq!(unanalyzed[0].session_id, "sess-unanalyzed");

    // Mark as analyzed
    let facets = serde_json::json!({"summary": "test session"});
    db::sessions::update_session_facets(&pool, "sess-unanalyzed", &facets)
        .await
        .unwrap();

    let unanalyzed = db::sessions::get_unanalyzed_sessions(&pool, 10)
        .await
        .unwrap();
    assert!(unanalyzed.is_empty());
}

// ============================================================
// IDP
// ============================================================

#[tokio::test]
async fn idp_crud() {
    let pool = helpers::setup_test_db().await;

    // Create
    let idp = db::idp::create_idp(
        &pool,
        "Test IDP",
        "https://idp.example.com",
        Some("client-id"),
        Some("audience"),
        None,
        "authorization_code",
        true,
        "member",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(idp.name, "Test IDP");
    assert!(idp.enabled);

    // List
    let all = db::idp::list_idps(&pool).await.unwrap();
    assert_eq!(all.len(), 1);

    // Get enabled
    let enabled = db::idp::get_enabled_idps(&pool).await.unwrap();
    assert_eq!(enabled.len(), 1);

    // Update (disable)
    let updated = db::idp::update_idp(
        &pool,
        idp.id,
        "Updated IDP",
        "https://idp.example.com",
        Some("client-id"),
        Some("audience"),
        None,
        "authorization_code",
        false,
        "admin",
        None,
        false,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(updated);

    let enabled = db::idp::get_enabled_idps(&pool).await.unwrap();
    assert!(enabled.is_empty());

    // Delete
    let deleted = db::idp::delete_idp(&pool, idp.id).await.unwrap();
    assert!(deleted);

    let all = db::idp::list_idps(&pool).await.unwrap();
    assert!(all.is_empty());
}

// ============================================================
// Key Cache
// ============================================================

#[tokio::test]
async fn key_cache_load_and_validate() {
    let pool = helpers::setup_test_db().await;

    let (raw_key, _vk) = helpers::create_test_key(&pool, Some("cache-key"), None, None).await;

    let cache = ccag::auth::KeyCache::new();
    let count = cache.load_from_db(&pool).await.unwrap();
    assert_eq!(count, 1);

    // Validate with the raw key
    let cached = cache.validate(&raw_key).await;
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().name.as_deref(), Some("cache-key"));

    // Invalid key
    let invalid = cache.validate("sk-proxy-bogus").await;
    assert!(invalid.is_none());
}

// ============================================================
// Budget: spend queries
// ============================================================

#[tokio::test]
async fn budget_user_spend_empty() {
    let pool = helpers::setup_test_db().await;
    let spend = db::budget::get_user_spend(&pool, "nobody@test.com", BudgetPeriod::Monthly)
        .await
        .unwrap();
    assert_eq!(spend, 0.0);
}

#[tokio::test]
async fn budget_user_spend_with_data() {
    let pool = helpers::setup_test_db().await;

    // Insert some spend entries
    let entries = vec![
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("budget-user@test.com")),
        helpers::make_spend_entry("claude-haiku-4-5-20251001", Some("budget-user@test.com")),
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("other-user@test.com")),
    ];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    let spend = db::budget::get_user_spend(&pool, "budget-user@test.com", BudgetPeriod::Monthly)
        .await
        .unwrap();
    assert!(spend > 0.0, "User spend should be non-zero after inserts");

    // Other user's spend should be separate
    let other_spend =
        db::budget::get_user_spend(&pool, "other-user@test.com", BudgetPeriod::Monthly)
            .await
            .unwrap();
    assert!(other_spend > 0.0);
    // User with 2 entries should have >= the user with 1 entry (model costs differ, but both > 0)
}

#[tokio::test]
async fn budget_team_spend() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "spend-team").await;
    let _user =
        helpers::create_test_user(&pool, "team-member@test.com", Some(team.id), "member").await;

    let entries = vec![helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("team-member@test.com"),
    )];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    let spend = db::budget::get_team_spend(&pool, team.id, BudgetPeriod::Monthly)
        .await
        .unwrap();
    assert!(spend > 0.0, "Team spend should reflect member's usage");
}

// ============================================================
// Budget: event deduplication
// ============================================================

#[tokio::test]
async fn budget_event_insert_and_dedup() {
    let pool = helpers::setup_test_db().await;

    let period_start = BudgetPeriod::Monthly.period_start();

    // First insert should succeed
    let inserted = db::budget::insert_event(
        &pool,
        Some("alice@test.com"),
        None,
        "warning",
        80,
        80.0,
        100.0,
        80.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();
    assert!(inserted, "First event insert should succeed");

    // Duplicate (same user, threshold, period_start) should be rejected
    let dup = db::budget::insert_event(
        &pool,
        Some("alice@test.com"),
        None,
        "warning",
        80,
        85.0, // different spend amount
        100.0,
        85.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();
    assert!(!dup, "Duplicate event should be rejected");

    // Different threshold should succeed
    let different_threshold = db::budget::insert_event(
        &pool,
        Some("alice@test.com"),
        None,
        "block",
        100,
        100.0,
        100.0,
        100.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();
    assert!(different_threshold, "Different threshold should succeed");
}

// ============================================================
// Budget: event delivery lifecycle
// ============================================================

#[tokio::test]
async fn budget_event_delivery_lifecycle() {
    let pool = helpers::setup_test_db().await;

    let period_start = BudgetPeriod::Monthly.period_start();

    // Insert events
    db::budget::insert_event(
        &pool,
        Some("user1@test.com"),
        None,
        "warning",
        80,
        80.0,
        100.0,
        80.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();
    db::budget::insert_event(
        &pool,
        Some("user2@test.com"),
        None,
        "block",
        100,
        100.0,
        100.0,
        100.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();

    // Both should be undelivered
    let undelivered = db::budget::get_undelivered_events(&pool, 10).await.unwrap();
    assert_eq!(undelivered.len(), 2);

    // Mark first as delivered
    db::budget::mark_delivered(&pool, undelivered[0].id)
        .await
        .unwrap();

    // Now only one undelivered
    let remaining = db::budget::get_undelivered_events(&pool, 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, undelivered[1].id);
}

// ============================================================
// Budget: user events query
// ============================================================

#[tokio::test]
async fn budget_user_events() {
    let pool = helpers::setup_test_db().await;

    let period_start = BudgetPeriod::Monthly.period_start();

    db::budget::insert_event(
        &pool,
        Some("events-user@test.com"),
        None,
        "warning",
        80,
        80.0,
        100.0,
        80.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();
    db::budget::insert_event(
        &pool,
        Some("events-user@test.com"),
        None,
        "block",
        100,
        100.0,
        100.0,
        100.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();
    db::budget::insert_event(
        &pool,
        Some("other@test.com"),
        None,
        "warning",
        80,
        80.0,
        100.0,
        80.0,
        "monthly",
        period_start,
    )
    .await
    .unwrap();

    let events = db::budget::get_user_events(&pool, "events-user@test.com", 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);

    // Shouldn't include other user's events
    let other_events = db::budget::get_user_events(&pool, "other@test.com", 10)
        .await
        .unwrap();
    assert_eq!(other_events.len(), 1);
}

// ============================================================
// Budget: team budget update
// ============================================================

#[tokio::test]
async fn budget_team_update() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "budget-team").await;
    assert!(team.budget_amount_usd.is_none());
    assert_eq!(team.budget_period, "monthly");

    // Set budget
    let policy = serde_json::json!([
        {"at_percent": 80, "action": "notify"},
        {"at_percent": 100, "action": "block"}
    ]);
    let updated = db::teams::update_team_budget(
        &pool,
        team.id,
        Some(500.0),
        "weekly",
        Some(policy),
        Some(50.0),
        "admin",
    )
    .await
    .unwrap();
    assert!(updated);

    // Verify
    let t = db::teams::get_team(&pool, team.id).await.unwrap().unwrap();
    assert_eq!(t.budget_amount_usd, Some(500.0));
    assert_eq!(t.budget_period, "weekly");
    assert_eq!(t.default_user_budget_usd, Some(50.0));
    assert_eq!(t.notify_recipients, "admin");
    assert!(t.budget_policy.is_some());
}

// ============================================================
// Budget: analytics overview
// ============================================================

#[tokio::test]
async fn budget_analytics_overview() {
    let pool = helpers::setup_test_db().await;

    // Create two teams with budgets
    let team1 = helpers::create_test_team(&pool, "team-alpha").await;
    let team2 = helpers::create_test_team(&pool, "team-beta").await;

    db::teams::update_team_budget(&pool, team1.id, Some(1000.0), "monthly", None, None, "both")
        .await
        .unwrap();
    db::teams::update_team_budget(&pool, team2.id, Some(500.0), "weekly", None, None, "both")
        .await
        .unwrap();

    // Add users to team1
    let _u1 = helpers::create_test_user(&pool, "alpha1@test.com", Some(team1.id), "member").await;
    let _u2 = helpers::create_test_user(&pool, "alpha2@test.com", Some(team1.id), "member").await;

    let overview = db::budget::get_analytics_overview(&pool).await.unwrap();
    assert_eq!(overview.len(), 2);

    // Find team1 in results
    let t1 = overview.iter().find(|o| o.team_id == team1.id).unwrap();
    assert_eq!(t1.team_name, "team-alpha");
    assert_eq!(t1.budget_amount_usd, Some(1000.0));
    assert_eq!(t1.user_count, Some(2));
}

// ============================================================
// Budget: team analytics (per-user breakdown)
// ============================================================

#[tokio::test]
async fn budget_team_analytics_detail() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "detail-team").await;
    let _user = helpers::create_test_user(&pool, "detail@test.com", Some(team.id), "member").await;

    // Insert spend for this user
    let entries = vec![
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("detail@test.com")),
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("detail@test.com")),
    ];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    let details = db::budget::get_team_analytics(&pool, team.id)
        .await
        .unwrap();
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].email, "detail@test.com");
    assert_eq!(details[0].request_count, Some(2));
    assert!(details[0].current_spend_usd.unwrap_or(0.0) > 0.0);
}

// ============================================================
// Budget: spend export
// ============================================================

#[tokio::test]
async fn budget_spend_export() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "export-team").await;
    let _user = helpers::create_test_user(&pool, "export@test.com", Some(team.id), "member").await;

    let entries = vec![helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("export@test.com"),
    )];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    let rows = db::budget::get_spend_export(&pool, 7).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "claude-sonnet-4-20250514");
    assert_eq!(rows[0].team_name.as_deref(), Some("export-team"));
    assert!(rows[0].cost_usd.unwrap_or(0.0) > 0.0);
}

// ============================================================
// Budget: period-aware spend isolation
// ============================================================

#[tokio::test]
async fn budget_daily_period_spend() {
    let pool = helpers::setup_test_db().await;

    // Insert spend for today
    let entries = vec![helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("daily-user@test.com"),
    )];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    // Daily spend should include today's entries
    let daily = db::budget::get_user_spend(&pool, "daily-user@test.com", BudgetPeriod::Daily)
        .await
        .unwrap();
    assert!(daily > 0.0);

    // Weekly should also include it (today is within this week)
    let weekly = db::budget::get_user_spend(&pool, "daily-user@test.com", BudgetPeriod::Weekly)
        .await
        .unwrap();
    assert!(weekly > 0.0);
    assert_eq!(
        daily, weekly,
        "Same data should appear in both daily and weekly"
    );
}
