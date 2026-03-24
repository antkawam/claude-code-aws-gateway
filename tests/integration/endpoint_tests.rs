use ccag::db;
use ccag::endpoint::stats::EndpointStats;
use uuid::Uuid;

use crate::helpers;

// ============================================================
// Endpoint CRUD: create and list
// ============================================================

#[tokio::test]
async fn endpoint_create_and_list() {
    let pool = helpers::setup_test_db().await;

    let ep =
        db::endpoints::create_endpoint(&pool, "test-ep-1", None, None, None, "us-east-1", "us", 0)
            .await
            .unwrap();

    assert_eq!(ep.name, "test-ep-1");
    assert_eq!(ep.region, "us-east-1");
    assert!(!ep.is_default);
    assert!(ep.enabled);

    let all = db::endpoints::list_endpoints(&pool).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, ep.id);
}

// ============================================================
// Endpoint: set as default
// ============================================================

#[tokio::test]
async fn endpoint_set_default() {
    let pool = helpers::setup_test_db().await;

    let ep1 = db::endpoints::create_endpoint(&pool, "ep-1", None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap();
    let ep2 = db::endpoints::create_endpoint(&pool, "ep-2", None, None, None, "us-west-2", "us", 0)
        .await
        .unwrap();

    // Neither should be default initially
    let all = db::endpoints::list_endpoints(&pool).await.unwrap();
    assert!(all.iter().all(|e| !e.is_default));

    // Set ep2 as default
    db::endpoints::set_default_endpoint(&pool, ep2.id)
        .await
        .unwrap();

    let all = db::endpoints::list_endpoints(&pool).await.unwrap();
    let default_ep = all.iter().find(|e| e.is_default);
    assert!(default_ep.is_some());
    assert_eq!(default_ep.unwrap().id, ep2.id);

    // ep1 should not be default
    let non_default = all.iter().find(|e| e.id == ep1.id).unwrap();
    assert!(!non_default.is_default);
}

// ============================================================
// Endpoint: team assignment with priorities
// ============================================================

#[tokio::test]
async fn endpoint_team_assignment() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "ep-team").await;

    let ep1 = db::endpoints::create_endpoint(&pool, "ep-a", None, None, None, "us-east-1", "us", 0)
        .await
        .unwrap();
    let ep2 = db::endpoints::create_endpoint(&pool, "ep-b", None, None, None, "us-west-2", "us", 0)
        .await
        .unwrap();

    // Assign both endpoints to the team with different priorities
    db::endpoints::set_team_endpoints(&pool, team.id, &[(ep1.id, 1), (ep2.id, 2)])
        .await
        .unwrap();

    // Query team endpoints — should be ordered by priority
    let team_eps = db::endpoints::get_team_endpoints(&pool, team.id)
        .await
        .unwrap();
    assert_eq!(team_eps.len(), 2);
    assert_eq!(team_eps[0].id, ep1.id, "ep1 (priority 1) should be first");
    assert_eq!(team_eps[1].id, ep2.id, "ep2 (priority 2) should be second");
}

// ============================================================
// Endpoint: routing strategy persistence
// ============================================================

#[tokio::test]
async fn endpoint_routing_strategy_persisted() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "routing-team").await;

    // Default should be "sticky_user" (migration 005)
    let t = db::teams::get_team(&pool, team.id).await.unwrap().unwrap();
    assert_eq!(t.routing_strategy, "sticky_user");

    // Update to round_robin
    db::teams::update_team_routing_strategy(&pool, team.id, "round_robin")
        .await
        .unwrap();

    let t = db::teams::get_team(&pool, team.id).await.unwrap().unwrap();
    assert_eq!(t.routing_strategy, "round_robin");
}

// ============================================================
// EndpointStats: record and retrieve
// ============================================================

#[tokio::test]
async fn endpoint_stats_tracking() {
    let stats = EndpointStats::new();
    let ep_id = Uuid::new_v4();

    // Record some stats
    stats.record_request(ep_id).await;
    stats.record_request(ep_id).await;
    stats.record_request(ep_id).await;
    stats.record_throttle(ep_id).await;
    stats.record_error(ep_id).await;

    let all = stats.get_all_stats().await;
    let snap = all.get(&ep_id).unwrap();
    assert_eq!(snap.request_count, 3);
    assert_eq!(snap.throttle_count_1h, 1);
    assert_eq!(snap.error_count_1h, 1);
}

// ============================================================
// Endpoint: priority ordering preserved in team query
// ============================================================

#[tokio::test]
async fn endpoint_priority_ordering() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "priority-team").await;

    // Create 3 endpoints
    let ep1 =
        db::endpoints::create_endpoint(&pool, "ep-low", None, None, None, "us-east-1", "us", 0)
            .await
            .unwrap();
    let ep2 =
        db::endpoints::create_endpoint(&pool, "ep-mid", None, None, None, "us-west-2", "us", 0)
            .await
            .unwrap();
    let ep3 =
        db::endpoints::create_endpoint(&pool, "ep-high", None, None, None, "eu-west-1", "eu", 0)
            .await
            .unwrap();

    // Assign with reversed priorities: ep3 first, ep1 last
    db::endpoints::set_team_endpoints(&pool, team.id, &[(ep3.id, 1), (ep2.id, 2), (ep1.id, 3)])
        .await
        .unwrap();

    let team_eps = db::endpoints::get_team_endpoints(&pool, team.id)
        .await
        .unwrap();
    assert_eq!(team_eps.len(), 3);
    assert_eq!(team_eps[0].id, ep3.id, "ep3 (priority 1) should be first");
    assert_eq!(team_eps[1].id, ep2.id, "ep2 (priority 2) should be second");
    assert_eq!(team_eps[2].id, ep1.id, "ep1 (priority 3) should be third");
}
