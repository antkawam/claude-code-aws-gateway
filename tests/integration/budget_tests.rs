use ccag::budget::{
    self, BudgetDecision, BudgetPeriod, BudgetSpendCache, PolicyAction, PolicyRule,
};
use ccag::db;

use crate::helpers;

// ============================================================
// Budget evaluation with real DB spend: Allow
// ============================================================

#[tokio::test]
async fn budget_evaluate_allow_when_under_limit() {
    let pool = helpers::setup_test_db().await;

    // Create team with $100 budget and standard policy
    let team = helpers::create_test_team(&pool, "eval-allow-team").await;
    let _user =
        helpers::create_test_user(&pool, "allow-user@test.com", Some(team.id), "member").await;

    let policy = vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ];
    let limit_usd = 100.0;

    // Insert spend entries totaling well under limit (~$0.003 per entry)
    let entries = vec![helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("allow-user@test.com"),
    )];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    let spend = db::budget::get_user_spend(&pool, "allow-user@test.com", BudgetPeriod::Monthly)
        .await
        .unwrap();

    let decision = budget::evaluate(&policy, spend, limit_usd);
    assert!(
        matches!(decision, BudgetDecision::Allow),
        "Expected Allow, got {:?} (spend={spend}, limit={limit_usd})",
        decision
    );
}

// ============================================================
// Budget evaluation: Notify at threshold
// ============================================================

#[tokio::test]
async fn budget_evaluate_notify_at_threshold() {
    let policy = vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ];

    // Simulate 85% spend
    let decision = budget::evaluate(&policy, 85.0, 100.0);
    assert!(
        matches!(
            decision,
            BudgetDecision::Notify {
                threshold_percent: 80
            }
        ),
        "Expected Notify at 80%, got {:?}",
        decision
    );
}

// ============================================================
// Budget evaluation: Block at limit
// ============================================================

#[tokio::test]
async fn budget_evaluate_block_at_limit() {
    let policy = vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ];

    // Simulate 105% spend
    let decision = budget::evaluate(&policy, 105.0, 100.0);
    assert!(
        matches!(
            decision,
            BudgetDecision::Block {
                threshold_percent: 100
            }
        ),
        "Expected Block at 100%, got {:?}",
        decision
    );
}

// ============================================================
// Budget evaluation: Shape applies RPM
// ============================================================

#[tokio::test]
async fn budget_evaluate_shape_applies_rpm() {
    let policy = vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 90,
            action: PolicyAction::Shape,
            shaped_rpm: Some(3),
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ];

    // At 95% — should trigger Shape
    let decision = budget::evaluate(&policy, 95.0, 100.0);
    assert!(
        matches!(
            decision,
            BudgetDecision::Shape {
                threshold_percent: 90,
                rpm: 3
            }
        ),
        "Expected Shape at 90% with rpm=3, got {:?}",
        decision
    );
}

// ============================================================
// Budget: most_restrictive combines user + team decisions
// ============================================================

#[tokio::test]
async fn budget_most_restrictive_combines_decisions() {
    let user_decision = BudgetDecision::Notify {
        threshold_percent: 80,
    };
    let team_decision = BudgetDecision::Block {
        threshold_percent: 100,
    };

    let combined = budget::most_restrictive(user_decision, team_decision);
    assert!(
        matches!(combined, BudgetDecision::Block { .. }),
        "Block should win over Notify"
    );
}

// ============================================================
// Budget: per-user budget evaluated independently from team
// ============================================================

#[tokio::test]
async fn budget_per_user_override() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "override-team").await;
    let _user =
        helpers::create_test_user(&pool, "override-user@test.com", Some(team.id), "member").await;

    // Set team budget to $500
    db::teams::update_team_budget(
        &pool,
        team.id,
        Some(500.0),
        "monthly",
        Some(serde_json::json!([
            {"at_percent": 80, "action": "notify"},
            {"at_percent": 100, "action": "block"}
        ])),
        None,
        "both",
    )
    .await
    .unwrap();

    // Insert spend
    let entries = vec![helpers::make_spend_entry(
        "claude-sonnet-4-20250514",
        Some("override-user@test.com"),
    )];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    // Query user spend and team spend separately — both paths should work
    let user_spend =
        db::budget::get_user_spend(&pool, "override-user@test.com", BudgetPeriod::Monthly)
            .await
            .unwrap();
    let team_spend = db::budget::get_team_spend(&pool, team.id, BudgetPeriod::Monthly)
        .await
        .unwrap();

    // Both should be the same (only one user in team)
    assert!(
        (user_spend - team_spend).abs() < 0.001,
        "User and team spend should match with single user"
    );

    // Evaluate user against team budget — should Allow (small spend, $500 limit)
    let rules = vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ];
    let decision = budget::evaluate(&rules, user_spend, 500.0);
    assert!(
        matches!(decision, BudgetDecision::Allow),
        "Small spend against $500 limit should Allow, got {:?}",
        decision
    );
}

// ============================================================
// Budget spend cache: TTL behavior
// ============================================================

#[tokio::test]
async fn budget_spend_cache_stores_and_retrieves() {
    let cache = BudgetSpendCache::new(30); // 30 second TTL

    // Cache should miss initially
    let miss = cache.get_user_spend("cache-user@test.com").await;
    assert!(miss.is_none());

    // Set and get
    cache.set_user_spend("cache-user@test.com", 42.5).await;
    let hit = cache.get_user_spend("cache-user@test.com").await;
    assert_eq!(hit, Some(42.5));
}

// ============================================================
// Budget: full flow with real DB — insert spend, query, evaluate
// ============================================================

#[tokio::test]
async fn budget_full_flow_insert_query_evaluate() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "flow-team").await;
    let _user =
        helpers::create_test_user(&pool, "flow-user@test.com", Some(team.id), "member").await;

    // Set team budget to $0.01 (very low so test spend exceeds it)
    let policy_json = serde_json::json!([
        {"at_percent": 80, "action": "notify"},
        {"at_percent": 100, "action": "block"}
    ]);
    db::teams::update_team_budget(
        &pool,
        team.id,
        Some(0.01),
        "monthly",
        Some(policy_json.clone()),
        None,
        "both",
    )
    .await
    .unwrap();

    // Insert spend entries — even small test entries cost ~$0.003 each
    let entries = vec![
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("flow-user@test.com")),
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("flow-user@test.com")),
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("flow-user@test.com")),
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("flow-user@test.com")),
        helpers::make_spend_entry("claude-sonnet-4-20250514", Some("flow-user@test.com")),
    ];
    db::spend::insert_batch(&pool, &entries).await.unwrap();

    // Query team spend
    let spend = db::budget::get_team_spend(&pool, team.id, BudgetPeriod::Monthly)
        .await
        .unwrap();
    assert!(
        spend > 0.01,
        "Spend ({spend}) should exceed the $0.01 limit"
    );

    // Parse policy rules and evaluate
    let rules: Vec<PolicyRule> = serde_json::from_value(policy_json).unwrap();
    let decision = budget::evaluate(&rules, spend, 0.01);
    assert!(
        matches!(decision, BudgetDecision::Block { .. }),
        "Expected Block when spend ({spend}) exceeds $0.01 limit, got {:?}",
        decision
    );
}
