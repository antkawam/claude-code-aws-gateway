/// Integration tests for SCIM Phase 3 Group CRUD — DB layer.
///
/// These tests exercise `db::teams` SCIM functions introduced in Phase 3:
///   create_scim_team, get_team_by_external_id, update_scim_team,
///   get_team_members, set_team_members, add_team_member,
///   remove_team_member, list_teams_for_idp, delete_team.
///
/// Run with: `make test-integration`
use uuid::Uuid;

use ccag::db;
use ccag::db::teams;
use ccag::db::users;
use ccag::scim::filter::ScimFilter;

use crate::helpers;

// ============================================================
// test_create_scim_team
// ============================================================

/// Create a SCIM team and verify every field is populated correctly.
#[tokio::test]
async fn test_create_scim_team() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-create-idp").await;

    let team = teams::create_scim_team(
        &pool,
        "Engineering",
        Some("okta-group-eng-001"),
        Some("Engineering"),
        idp.id,
    )
    .await
    .expect("create_scim_team failed");

    assert_eq!(team.name, "Engineering");
    assert_eq!(team.external_id.as_deref(), Some("okta-group-eng-001"));
    assert_eq!(team.display_name.as_deref(), Some("Engineering"));
    assert_eq!(team.idp_id, Some(idp.id));
    assert!(team.scim_managed, "SCIM team must have scim_managed=true");
}

// ============================================================
// test_get_team_by_external_id
// ============================================================

/// Create a team with an external_id, then fetch it by (external_id, idp_id).
#[tokio::test]
async fn test_get_team_by_external_id() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-ext-id-idp").await;

    let created = teams::create_scim_team(
        &pool,
        "Finance",
        Some("ext-group-finance-001"),
        Some("Finance"),
        idp.id,
    )
    .await
    .expect("create_scim_team failed");

    let fetched = teams::get_team_by_external_id(&pool, "ext-group-finance-001", idp.id)
        .await
        .expect("get_team_by_external_id failed")
        .expect("Team should be found");

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, "Finance");
    assert_eq!(
        fetched.external_id.as_deref(),
        Some("ext-group-finance-001")
    );
}

// ============================================================
// test_get_team_by_external_id_wrong_idp
// ============================================================

/// Fetching a team using a different IDP's id should return None.
#[tokio::test]
async fn test_get_team_by_external_id_wrong_idp() {
    let pool = helpers::setup_test_db().await;
    let idp_a = helpers::create_test_idp(&pool, "scim-group-wrong-idp-a").await;
    let idp_b = helpers::create_test_idp(&pool, "scim-group-wrong-idp-b").await;

    teams::create_scim_team(
        &pool,
        "Marketing",
        Some("ext-group-mkt-001"),
        Some("Marketing"),
        idp_a.id,
    )
    .await
    .expect("create_scim_team failed");

    // Fetch using IDP-B's ID — must return None
    let result = teams::get_team_by_external_id(&pool, "ext-group-mkt-001", idp_b.id)
        .await
        .expect("get_team_by_external_id failed");

    assert!(
        result.is_none(),
        "Fetching a team with the wrong IDP id must return None"
    );
}

// ============================================================
// test_update_scim_team
// ============================================================

/// Create a team, then update its name, external_id, and display_name.
#[tokio::test]
async fn test_update_scim_team() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-update-idp").await;

    let team = teams::create_scim_team(
        &pool,
        "OriginalName",
        Some("ext-update-001"),
        Some("OriginalName"),
        idp.id,
    )
    .await
    .expect("create_scim_team failed");

    let updated = teams::update_scim_team(
        &pool,
        team.id,
        "UpdatedName",
        Some("ext-update-002"),
        Some("Updated Display Name"),
    )
    .await
    .expect("update_scim_team failed")
    .expect("update_scim_team should return the updated team");

    assert_eq!(updated.name, "UpdatedName");
    assert_eq!(updated.external_id.as_deref(), Some("ext-update-002"));
    assert_eq!(
        updated.display_name.as_deref(),
        Some("Updated Display Name")
    );
    assert_eq!(updated.id, team.id, "ID must not change on update");
}

// ============================================================
// test_get_team_members
// ============================================================

/// Create a team and two users assigned to it; get_team_members should return both.
#[tokio::test]
async fn test_get_team_members() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-members-idp").await;

    let team = teams::create_scim_team(&pool, "Members Team", None, None, idp.id)
        .await
        .expect("create_scim_team failed");

    let user1 = users::create_scim_user(
        &pool,
        "member1@example.com",
        Some("ext-m1"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let user2 = users::create_scim_user(
        &pool,
        "member2@example.com",
        Some("ext-m2"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Assign both users to the team
    teams::add_team_member(&pool, team.id, user1.id)
        .await
        .expect("add_team_member failed");
    teams::add_team_member(&pool, team.id, user2.id)
        .await
        .expect("add_team_member failed");

    let members = teams::get_team_members(&pool, team.id)
        .await
        .expect("get_team_members failed");

    assert_eq!(members.len(), 2, "Team should have 2 members");

    let member_ids: Vec<Uuid> = members.iter().map(|u| u.id).collect();
    assert!(member_ids.contains(&user1.id), "member1 should be in team");
    assert!(member_ids.contains(&user2.id), "member2 should be in team");
}

// ============================================================
// test_get_team_members_empty
// ============================================================

/// A team with no users should return an empty vec.
#[tokio::test]
async fn test_get_team_members_empty() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-empty-members-idp").await;

    let team = teams::create_scim_team(&pool, "Empty Team", None, None, idp.id)
        .await
        .expect("create_scim_team failed");

    let members = teams::get_team_members(&pool, team.id)
        .await
        .expect("get_team_members failed");

    assert!(
        members.is_empty(),
        "Team with no users should return empty vec"
    );
}

// ============================================================
// test_set_team_members
// ============================================================

/// set_team_members atomically replaces the full member list.
/// Old members are removed; new members are assigned.
#[tokio::test]
async fn test_set_team_members() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-set-members-idp").await;

    let team = teams::create_scim_team(&pool, "Set Members Team", None, None, idp.id)
        .await
        .expect("create_scim_team failed");

    // Create 4 users
    let user1 = users::create_scim_user(
        &pool,
        "set-u1@example.com",
        Some("ext-set-u1"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let user2 = users::create_scim_user(
        &pool,
        "set-u2@example.com",
        Some("ext-set-u2"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let user3 = users::create_scim_user(
        &pool,
        "set-u3@example.com",
        Some("ext-set-u3"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let user4 = users::create_scim_user(
        &pool,
        "set-u4@example.com",
        Some("ext-set-u4"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Assign user1 and user2 as initial members
    teams::set_team_members(&pool, team.id, &[user1.id, user2.id])
        .await
        .expect("set_team_members (initial) failed");

    // Verify initial state
    let initial_members = teams::get_team_members(&pool, team.id)
        .await
        .expect("get_team_members failed");
    assert_eq!(initial_members.len(), 2, "Initial member count should be 2");

    // Replace with user3 and user4
    teams::set_team_members(&pool, team.id, &[user3.id, user4.id])
        .await
        .expect("set_team_members (replace) failed");

    let new_members = teams::get_team_members(&pool, team.id)
        .await
        .expect("get_team_members after replacement failed");

    let new_member_ids: Vec<Uuid> = new_members.iter().map(|u| u.id).collect();

    assert_eq!(
        new_members.len(),
        2,
        "Member count after replacement should be 2"
    );
    assert!(
        !new_member_ids.contains(&user1.id),
        "user1 should be removed after set_team_members"
    );
    assert!(
        !new_member_ids.contains(&user2.id),
        "user2 should be removed after set_team_members"
    );
    assert!(
        new_member_ids.contains(&user3.id),
        "user3 should be a new member"
    );
    assert!(
        new_member_ids.contains(&user4.id),
        "user4 should be a new member"
    );

    // Verify old members no longer have this team_id
    let fetched_u1 = db::users::get_user_by_email(&pool, "set-u1@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user1 should still exist");
    assert!(
        fetched_u1.team_id.is_none(),
        "user1 team_id should be NULL after being removed from team"
    );
}

// ============================================================
// test_add_team_member
// ============================================================

/// add_team_member sets the user's team_id to the given team.
#[tokio::test]
async fn test_add_team_member() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-add-member-idp").await;

    let team = teams::create_scim_team(&pool, "Add Member Team", None, None, idp.id)
        .await
        .expect("create_scim_team failed");

    let user = users::create_scim_user(
        &pool,
        "add-member@example.com",
        Some("ext-add-member"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    assert!(user.team_id.is_none(), "User should start without a team");

    let updated = teams::add_team_member(&pool, team.id, user.id)
        .await
        .expect("add_team_member failed");

    assert!(
        updated,
        "add_team_member should return true when user was found"
    );

    let fetched = db::users::get_user_by_email(&pool, "add-member@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("User should still exist");

    assert_eq!(
        fetched.team_id,
        Some(team.id),
        "User team_id should be set to the team after add_team_member"
    );
}

// ============================================================
// test_remove_team_member
// ============================================================

/// remove_team_member sets the user's team_id to NULL.
#[tokio::test]
async fn test_remove_team_member() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-remove-member-idp").await;

    let team = teams::create_scim_team(&pool, "Remove Member Team", None, None, idp.id)
        .await
        .expect("create_scim_team failed");

    let user = users::create_scim_user(
        &pool,
        "remove-member@example.com",
        Some("ext-remove-member"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // First add the user
    teams::add_team_member(&pool, team.id, user.id)
        .await
        .expect("add_team_member failed");

    // Verify they are assigned
    let after_add = db::users::get_user_by_email(&pool, "remove-member@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("User should exist");
    assert_eq!(
        after_add.team_id,
        Some(team.id),
        "User should be in team before removal"
    );

    // Remove the user
    let removed = teams::remove_team_member(&pool, team.id, user.id)
        .await
        .expect("remove_team_member failed");

    assert!(
        removed,
        "remove_team_member should return true when user was a member"
    );

    let after_remove = db::users::get_user_by_email(&pool, "remove-member@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("User should still exist after removal");

    assert!(
        after_remove.team_id.is_none(),
        "User team_id should be NULL after remove_team_member"
    );
}

// ============================================================
// test_list_teams_for_idp
// ============================================================

/// Create teams for two IDPs; listing for IDP-A returns only IDP-A teams.
#[tokio::test]
async fn test_list_teams_for_idp() {
    let pool = helpers::setup_test_db().await;
    let idp_a = helpers::create_test_idp(&pool, "scim-group-list-idp-a").await;
    let idp_b = helpers::create_test_idp(&pool, "scim-group-list-idp-b").await;

    // Create 2 teams for IDP-A and 1 for IDP-B
    teams::create_scim_team(&pool, "Alpha Team", Some("ext-alpha"), None, idp_a.id)
        .await
        .expect("create_scim_team IDP-A #1 failed");

    teams::create_scim_team(&pool, "Beta Team", Some("ext-beta"), None, idp_a.id)
        .await
        .expect("create_scim_team IDP-A #2 failed");

    teams::create_scim_team(&pool, "Gamma Team", Some("ext-gamma"), None, idp_b.id)
        .await
        .expect("create_scim_team IDP-B failed");

    let (teams_a, total_a) = teams::list_teams_for_idp(&pool, idp_a.id, None, 0, 100)
        .await
        .expect("list_teams_for_idp failed");

    assert_eq!(teams_a.len(), 2, "Should return exactly 2 teams for IDP-A");
    assert_eq!(total_a, 2, "Total count should be 2 for IDP-A");

    for t in &teams_a {
        assert_eq!(
            t.idp_id,
            Some(idp_a.id),
            "All listed teams must belong to IDP-A"
        );
    }

    let (teams_b, total_b) = teams::list_teams_for_idp(&pool, idp_b.id, None, 0, 100)
        .await
        .expect("list_teams_for_idp for IDP-B failed");

    assert_eq!(teams_b.len(), 1, "Should return exactly 1 team for IDP-B");
    assert_eq!(total_b, 1);
}

// ============================================================
// test_list_teams_for_idp_with_filter
// ============================================================

/// Filtering by displayName eq "Engineering" returns only the matching team.
#[tokio::test]
async fn test_list_teams_for_idp_with_filter() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-filter-idp").await;

    teams::create_scim_team(&pool, "Engineering", Some("ext-eng"), None, idp.id)
        .await
        .expect("create_scim_team Engineering failed");

    teams::create_scim_team(&pool, "Design", Some("ext-design"), None, idp.id)
        .await
        .expect("create_scim_team Design failed");

    teams::create_scim_team(&pool, "Product", Some("ext-product"), None, idp.id)
        .await
        .expect("create_scim_team Product failed");

    let filter = ScimFilter::Eq("displayName".to_string(), "Engineering".to_string());

    let (result, total) = teams::list_teams_for_idp(&pool, idp.id, Some(&filter), 0, 100)
        .await
        .expect("list_teams_for_idp with filter failed");

    assert_eq!(result.len(), 1, "Filter should return only Engineering");
    assert_eq!(total, 1);
    assert_eq!(result[0].name, "Engineering");
}

/// displayName eq filter is case-insensitive.
#[tokio::test]
async fn test_list_teams_for_idp_filter_case_insensitive() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-filter-case-idp").await;

    teams::create_scim_team(&pool, "SalesTeam", Some("ext-sales"), None, idp.id)
        .await
        .expect("create_scim_team failed");

    // Match with different casing
    let filter = ScimFilter::Eq("displayName".to_string(), "salesteam".to_string());

    let (result, total) = teams::list_teams_for_idp(&pool, idp.id, Some(&filter), 0, 100)
        .await
        .expect("list_teams_for_idp with case-insensitive filter failed");

    assert_eq!(result.len(), 1, "Case-insensitive eq filter should match");
    assert_eq!(total, 1);
}

// ============================================================
// test_list_teams_for_idp_pagination
// ============================================================

/// Create 5 teams, list with offset=2, limit=2; verify correct slice and total_count=5.
#[tokio::test]
async fn test_list_teams_for_idp_pagination() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-pagination-idp").await;

    for i in 1..=5 {
        teams::create_scim_team(
            &pool,
            &format!("Page Team {:02}", i),
            Some(&format!("ext-page-{i:02}")),
            None,
            idp.id,
        )
        .await
        .expect("create_scim_team failed");
    }

    // offset=2 (0-based), limit=2 → should return teams 3 and 4 (sorted by name)
    let (page, total) = teams::list_teams_for_idp(&pool, idp.id, None, 2, 2)
        .await
        .expect("list_teams_for_idp pagination failed");

    assert_eq!(page.len(), 2, "Should return exactly 2 results per page");
    assert_eq!(total, 5, "Total count must be 5 regardless of pagination");
}

/// Pagination beyond the last team returns empty results with correct total.
#[tokio::test]
async fn test_list_teams_for_idp_pagination_beyond_end() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-pagination-beyond-idp").await;

    for i in 1..=3 {
        teams::create_scim_team(
            &pool,
            &format!("Beyond Team {i}"),
            Some(&format!("ext-beyond-{i}")),
            None,
            idp.id,
        )
        .await
        .expect("create_scim_team failed");
    }

    // offset=10 is beyond all 3 teams
    let (page, total) = teams::list_teams_for_idp(&pool, idp.id, None, 10, 2)
        .await
        .expect("list_teams_for_idp beyond end failed");

    assert!(
        page.is_empty(),
        "Page beyond end should return empty results"
    );
    assert_eq!(total, 3, "Total count should still be 3");
}

// ============================================================
// test_create_scim_team_duplicate_name
// ============================================================

/// Creating two SCIM teams with the same name should fail (DB unique constraint).
#[tokio::test]
async fn test_create_scim_team_duplicate_name() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-dup-name-idp").await;

    teams::create_scim_team(&pool, "Duplicate Team", Some("ext-dup-001"), None, idp.id)
        .await
        .expect("First create_scim_team should succeed");

    // Second team with the same name (teams.name has a unique constraint)
    let result = teams::create_scim_team(
        &pool,
        "Duplicate Team",
        Some("ext-dup-002"), // different external_id
        None,
        idp.id,
    )
    .await;

    assert!(
        result.is_err(),
        "Creating a second team with the same name must fail due to unique constraint"
    );
}

// ============================================================
// test_delete_scim_team_unassigns_members
// ============================================================

/// Hard-delete a team after unassigning its members.
/// After set_team_members([]) followed by delete_team, users' team_id is NULL
/// and the team row no longer exists.
#[tokio::test]
async fn test_delete_scim_team_unassigns_members() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-delete-idp").await;

    let team = teams::create_scim_team(&pool, "Delete Me Team", None, None, idp.id)
        .await
        .expect("create_scim_team failed");

    let user1 = users::create_scim_user(
        &pool,
        "del-u1@example.com",
        Some("ext-del-u1"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let user2 = users::create_scim_user(
        &pool,
        "del-u2@example.com",
        Some("ext-del-u2"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Assign both users
    teams::set_team_members(&pool, team.id, &[user1.id, user2.id])
        .await
        .expect("set_team_members failed");

    // Verify members are assigned
    let before_delete = teams::get_team_members(&pool, team.id)
        .await
        .expect("get_team_members failed");
    assert_eq!(
        before_delete.len(),
        2,
        "Should have 2 members before delete"
    );

    // Unassign all members (as delete_group handler does)
    teams::set_team_members(&pool, team.id, &[])
        .await
        .expect("set_team_members (unassign all) failed");

    // Hard-delete the team
    let deleted = teams::delete_team(&pool, team.id)
        .await
        .expect("delete_team failed");
    assert!(deleted, "delete_team should return true for existing team");

    // Team row must no longer exist
    let fetched_team = teams::get_team(&pool, team.id)
        .await
        .expect("get_team failed");
    assert!(
        fetched_team.is_none(),
        "Team should not exist after hard delete"
    );

    // Users must still exist with team_id = NULL
    let fetched_u1 = db::users::get_user_by_email(&pool, "del-u1@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user1 should still exist after team deletion");
    assert!(
        fetched_u1.team_id.is_none(),
        "user1 team_id should be NULL after team was deleted"
    );

    let fetched_u2 = db::users::get_user_by_email(&pool, "del-u2@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user2 should still exist after team deletion");
    assert!(
        fetched_u2.team_id.is_none(),
        "user2 team_id should be NULL after team was deleted"
    );
}
