/// Integration tests for SCIM Groups — role-mapping model.
///
/// These tests exercise `db::scim_groups` functions introduced in the
/// decoupled-groups model:
///   create_scim_group, get_scim_group, get_scim_group_by_external_id,
///   update_scim_group, delete_scim_group, list_scim_groups_for_idp,
///   get_scim_group_members, set_scim_group_members, add_scim_group_member,
///   remove_scim_group_member, evaluate_user_role, sync_user_role.
///
/// Run with: `make test-integration`
use uuid::Uuid;

use ccag::db;
use ccag::db::scim_groups;
use ccag::db::users;
use ccag::scim::filter::ScimFilter;

use crate::helpers;

// ============================================================
// test_create_scim_group
// ============================================================

/// Create a SCIM group and verify every field is populated correctly.
#[tokio::test]
async fn test_create_scim_group() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-create-idp").await;

    let group =
        scim_groups::create_scim_group(&pool, "Engineering", Some("okta-group-eng-001"), idp.id)
            .await
            .expect("create_scim_group failed");

    assert_eq!(group.display_name, "Engineering");
    assert_eq!(group.external_id.as_deref(), Some("okta-group-eng-001"));
    assert_eq!(group.idp_id, idp.id);
    assert!(group.id != Uuid::nil(), "group must have a non-nil UUID");
}

// ============================================================
// test_get_scim_group_by_external_id
// ============================================================

/// Create a group with an external_id, then fetch it by (external_id, idp_id).
#[tokio::test]
async fn test_get_scim_group_by_external_id() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-ext-id-idp").await;

    let created =
        scim_groups::create_scim_group(&pool, "Finance", Some("ext-group-finance-001"), idp.id)
            .await
            .expect("create_scim_group failed");

    let fetched =
        scim_groups::get_scim_group_by_external_id(&pool, "ext-group-finance-001", idp.id)
            .await
            .expect("get_scim_group_by_external_id failed")
            .expect("Group should be found");

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.display_name, "Finance");
    assert_eq!(
        fetched.external_id.as_deref(),
        Some("ext-group-finance-001")
    );
}

// ============================================================
// test_get_scim_group_by_external_id_wrong_idp
// ============================================================

/// Fetching a group using a different IDP's id should return None.
#[tokio::test]
async fn test_get_scim_group_by_external_id_wrong_idp() {
    let pool = helpers::setup_test_db().await;
    let idp_a = helpers::create_test_idp(&pool, "scim-group-wrong-idp-a").await;
    let idp_b = helpers::create_test_idp(&pool, "scim-group-wrong-idp-b").await;

    scim_groups::create_scim_group(&pool, "Marketing", Some("ext-group-mkt-001"), idp_a.id)
        .await
        .expect("create_scim_group failed");

    // Fetch using IDP-B's ID — must return None
    let result = scim_groups::get_scim_group_by_external_id(&pool, "ext-group-mkt-001", idp_b.id)
        .await
        .expect("get_scim_group_by_external_id failed");

    assert!(
        result.is_none(),
        "Fetching a group with the wrong IDP id must return None"
    );
}

// ============================================================
// test_update_scim_group
// ============================================================

/// Create a group, then update its display_name and external_id.
#[tokio::test]
async fn test_update_scim_group() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-update-idp").await;

    let group =
        scim_groups::create_scim_group(&pool, "OriginalName", Some("ext-update-001"), idp.id)
            .await
            .expect("create_scim_group failed");

    let updated =
        scim_groups::update_scim_group(&pool, group.id, "UpdatedName", Some("ext-update-002"))
            .await
            .expect("update_scim_group failed")
            .expect("update_scim_group should return the updated group");

    assert_eq!(updated.display_name, "UpdatedName");
    assert_eq!(updated.external_id.as_deref(), Some("ext-update-002"));
    assert_eq!(updated.id, group.id, "ID must not change on update");
    assert_eq!(updated.idp_id, idp.id, "idp_id must not change on update");
}

// ============================================================
// test_delete_scim_group
// ============================================================

/// Delete a group and verify it can no longer be fetched.
#[tokio::test]
async fn test_delete_scim_group() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-delete-idp").await;

    let group = scim_groups::create_scim_group(&pool, "ToDelete", Some("ext-del-001"), idp.id)
        .await
        .expect("create_scim_group failed");

    let deleted = scim_groups::delete_scim_group(&pool, group.id)
        .await
        .expect("delete_scim_group failed");

    assert!(
        deleted,
        "delete_scim_group should return true for an existing group"
    );

    let fetched = scim_groups::get_scim_group(&pool, group.id)
        .await
        .expect("get_scim_group failed");

    assert!(fetched.is_none(), "Group should not exist after deletion");
}

// ============================================================
// test_delete_scim_group_cascades_members
// ============================================================

/// Delete a group that has members; the scim_group_members rows should be gone
/// but the user rows themselves must still exist.
#[tokio::test]
async fn test_delete_scim_group_cascades_members() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-cascade-idp").await;

    let group = scim_groups::create_scim_group(&pool, "CascadeGroup", None, idp.id)
        .await
        .expect("create_scim_group failed");

    let user1 = users::create_scim_user(
        &pool,
        "cascade-u1@example.com",
        Some("ext-cascade-u1"),
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
        "cascade-u2@example.com",
        Some("ext-cascade-u2"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    scim_groups::set_scim_group_members(&pool, group.id, &[user1.id, user2.id])
        .await
        .expect("set_scim_group_members failed");

    // Verify members are present before deletion
    let before = scim_groups::get_scim_group_members(&pool, group.id)
        .await
        .expect("get_scim_group_members failed");
    assert_eq!(before.len(), 2, "Should have 2 members before delete");

    // Delete the group
    scim_groups::delete_scim_group(&pool, group.id)
        .await
        .expect("delete_scim_group failed");

    // Users must still exist
    let fetched_u1 = users::get_user_by_email(&pool, "cascade-u1@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user1 should still exist after group deletion");
    assert_eq!(fetched_u1.id, user1.id);

    let fetched_u2 = users::get_user_by_email(&pool, "cascade-u2@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user2 should still exist after group deletion");
    assert_eq!(fetched_u2.id, user2.id);

    // The scim_group_members rows should be gone (verified via DB query)
    let orphaned_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scim_group_members WHERE group_id = $1")
            .bind(group.id)
            .fetch_one(&pool)
            .await
            .expect("orphan count query failed");

    assert_eq!(
        orphaned_count, 0,
        "All scim_group_members rows for the deleted group should be CASCADE deleted"
    );
}

// ============================================================
// test_add_and_get_scim_group_members
// ============================================================

/// Add two users to a group via add_scim_group_member; get_scim_group_members
/// should return both.
#[tokio::test]
async fn test_add_and_get_scim_group_members() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-add-get-members-idp").await;

    let group = scim_groups::create_scim_group(&pool, "MembersGroup", None, idp.id)
        .await
        .expect("create_scim_group failed");

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

    let added1 = scim_groups::add_scim_group_member(&pool, group.id, user1.id)
        .await
        .expect("add_scim_group_member user1 failed");
    assert!(added1, "First add should return true");

    let added2 = scim_groups::add_scim_group_member(&pool, group.id, user2.id)
        .await
        .expect("add_scim_group_member user2 failed");
    assert!(added2, "Second add should return true");

    let members = scim_groups::get_scim_group_members(&pool, group.id)
        .await
        .expect("get_scim_group_members failed");

    assert_eq!(members.len(), 2, "Group should have 2 members");
    let member_ids: Vec<Uuid> = members.iter().map(|u| u.id).collect();
    assert!(member_ids.contains(&user1.id), "user1 should be a member");
    assert!(member_ids.contains(&user2.id), "user2 should be a member");
}

// ============================================================
// test_get_scim_group_members_empty
// ============================================================

/// A newly created group with no members returns an empty vec.
#[tokio::test]
async fn test_get_scim_group_members_empty() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-empty-members-idp").await;

    let group = scim_groups::create_scim_group(&pool, "EmptyGroup", None, idp.id)
        .await
        .expect("create_scim_group failed");

    let members = scim_groups::get_scim_group_members(&pool, group.id)
        .await
        .expect("get_scim_group_members failed");

    assert!(
        members.is_empty(),
        "Group with no members should return empty vec"
    );
}

// ============================================================
// test_set_scim_group_members_replaces
// ============================================================

/// set_scim_group_members atomically replaces the full member list.
/// Set [A, B], then set [C, D] — only C and D should remain.
#[tokio::test]
async fn test_set_scim_group_members_replaces() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-set-members-idp").await;

    let group = scim_groups::create_scim_group(&pool, "SetMembersGroup", None, idp.id)
        .await
        .expect("create_scim_group failed");

    let user_a = users::create_scim_user(
        &pool,
        "set-ua@example.com",
        Some("ext-set-ua"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user user_a failed");

    let user_b = users::create_scim_user(
        &pool,
        "set-ub@example.com",
        Some("ext-set-ub"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user user_b failed");

    let user_c = users::create_scim_user(
        &pool,
        "set-uc@example.com",
        Some("ext-set-uc"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user user_c failed");

    let user_d = users::create_scim_user(
        &pool,
        "set-ud@example.com",
        Some("ext-set-ud"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user user_d failed");

    // Initial set: A and B
    scim_groups::set_scim_group_members(&pool, group.id, &[user_a.id, user_b.id])
        .await
        .expect("set_scim_group_members (initial) failed");

    let initial = scim_groups::get_scim_group_members(&pool, group.id)
        .await
        .expect("get_scim_group_members after initial set failed");
    assert_eq!(initial.len(), 2, "Initial member count should be 2");

    // Replace with C and D
    scim_groups::set_scim_group_members(&pool, group.id, &[user_c.id, user_d.id])
        .await
        .expect("set_scim_group_members (replace) failed");

    let after = scim_groups::get_scim_group_members(&pool, group.id)
        .await
        .expect("get_scim_group_members after replacement failed");

    let after_ids: Vec<Uuid> = after.iter().map(|u| u.id).collect();

    assert_eq!(after.len(), 2, "Member count after replacement should be 2");
    assert!(
        !after_ids.contains(&user_a.id),
        "user_a should be removed after set_scim_group_members"
    );
    assert!(
        !after_ids.contains(&user_b.id),
        "user_b should be removed after set_scim_group_members"
    );
    assert!(
        after_ids.contains(&user_c.id),
        "user_c should be in the new member list"
    );
    assert!(
        after_ids.contains(&user_d.id),
        "user_d should be in the new member list"
    );
}

// ============================================================
// test_remove_scim_group_member
// ============================================================

/// Add a user to a group, remove them, verify empty member list.
#[tokio::test]
async fn test_remove_scim_group_member() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-remove-member-idp").await;

    let group = scim_groups::create_scim_group(&pool, "RemoveMemberGroup", None, idp.id)
        .await
        .expect("create_scim_group failed");

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

    scim_groups::add_scim_group_member(&pool, group.id, user.id)
        .await
        .expect("add_scim_group_member failed");

    let removed = scim_groups::remove_scim_group_member(&pool, group.id, user.id)
        .await
        .expect("remove_scim_group_member failed");

    assert!(
        removed,
        "remove_scim_group_member should return true when the member existed"
    );

    let members = scim_groups::get_scim_group_members(&pool, group.id)
        .await
        .expect("get_scim_group_members failed");

    assert!(
        members.is_empty(),
        "Group should be empty after removing the only member"
    );
}

// ============================================================
// test_add_scim_group_member_idempotent
// ============================================================

/// Adding the same user to a group twice must not error (ON CONFLICT DO NOTHING).
/// The second insert returns false (no rows affected) but does not panic.
#[tokio::test]
async fn test_add_scim_group_member_idempotent() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-idempotent-idp").await;

    let group = scim_groups::create_scim_group(&pool, "IdempotentGroup", None, idp.id)
        .await
        .expect("create_scim_group failed");

    let user = users::create_scim_user(
        &pool,
        "idempotent-user@example.com",
        Some("ext-idempotent"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let first = scim_groups::add_scim_group_member(&pool, group.id, user.id)
        .await
        .expect("first add_scim_group_member failed");
    assert!(first, "First add should return true");

    // Second add — should not error; rows_affected = 0
    let second = scim_groups::add_scim_group_member(&pool, group.id, user.id)
        .await
        .expect("second add_scim_group_member must not return an error");
    assert!(
        !second,
        "Second add should return false (ON CONFLICT DO NOTHING)"
    );

    // Member count must still be 1
    let members = scim_groups::get_scim_group_members(&pool, group.id)
        .await
        .expect("get_scim_group_members failed");
    assert_eq!(
        members.len(),
        1,
        "Duplicate add must not create duplicate row"
    );
}

// ============================================================
// test_list_scim_groups_for_idp
// ============================================================

/// Create groups for two IDPs; listing for IDP-A returns only IDP-A groups.
#[tokio::test]
async fn test_list_scim_groups_for_idp() {
    let pool = helpers::setup_test_db().await;
    let idp_a = helpers::create_test_idp(&pool, "scim-group-list-idp-a").await;
    let idp_b = helpers::create_test_idp(&pool, "scim-group-list-idp-b").await;

    scim_groups::create_scim_group(&pool, "Alpha Group", Some("ext-alpha"), idp_a.id)
        .await
        .expect("create_scim_group IDP-A #1 failed");

    scim_groups::create_scim_group(&pool, "Beta Group", Some("ext-beta"), idp_a.id)
        .await
        .expect("create_scim_group IDP-A #2 failed");

    scim_groups::create_scim_group(&pool, "Gamma Group", Some("ext-gamma"), idp_b.id)
        .await
        .expect("create_scim_group IDP-B failed");

    let (groups_a, total_a) = scim_groups::list_scim_groups_for_idp(&pool, idp_a.id, None, 0, 100)
        .await
        .expect("list_scim_groups_for_idp IDP-A failed");

    assert_eq!(
        groups_a.len(),
        2,
        "Should return exactly 2 groups for IDP-A"
    );
    assert_eq!(total_a, 2, "Total count should be 2 for IDP-A");

    for g in &groups_a {
        assert_eq!(g.idp_id, idp_a.id, "All listed groups must belong to IDP-A");
    }

    let (groups_b, total_b) = scim_groups::list_scim_groups_for_idp(&pool, idp_b.id, None, 0, 100)
        .await
        .expect("list_scim_groups_for_idp IDP-B failed");

    assert_eq!(groups_b.len(), 1, "Should return exactly 1 group for IDP-B");
    assert_eq!(total_b, 1);
}

// ============================================================
// test_list_scim_groups_filter_display_name
// ============================================================

/// Filtering by displayName eq "Engineering" returns only the matching group.
#[tokio::test]
async fn test_list_scim_groups_filter_display_name() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-filter-idp").await;

    scim_groups::create_scim_group(&pool, "Engineering", Some("ext-eng"), idp.id)
        .await
        .expect("create_scim_group Engineering failed");

    scim_groups::create_scim_group(&pool, "Design", Some("ext-design"), idp.id)
        .await
        .expect("create_scim_group Design failed");

    scim_groups::create_scim_group(&pool, "Product", Some("ext-product"), idp.id)
        .await
        .expect("create_scim_group Product failed");

    let filter = ScimFilter::Eq("displayName".to_string(), "Engineering".to_string());

    let (result, total) =
        scim_groups::list_scim_groups_for_idp(&pool, idp.id, Some(&filter), 0, 100)
            .await
            .expect("list_scim_groups_for_idp with filter failed");

    assert_eq!(result.len(), 1, "Filter should return only Engineering");
    assert_eq!(total, 1);
    assert_eq!(result[0].display_name, "Engineering");
}

// ============================================================
// test_list_scim_groups_pagination
// ============================================================

/// Create 5 groups, paginate with offset=2 limit=2; verify correct slice and total=5.
#[tokio::test]
async fn test_list_scim_groups_pagination() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-group-pagination-idp").await;

    for i in 1..=5 {
        scim_groups::create_scim_group(
            &pool,
            &format!("Page Group {:02}", i),
            Some(&format!("ext-page-{i:02}")),
            idp.id,
        )
        .await
        .expect("create_scim_group failed");
    }

    // offset=2 (0-based), limit=2 → should return groups 3 and 4 (sorted by display_name)
    let (page, total) = scim_groups::list_scim_groups_for_idp(&pool, idp.id, None, 2, 2)
        .await
        .expect("list_scim_groups_for_idp pagination failed");

    assert_eq!(page.len(), 2, "Should return exactly 2 results per page");
    assert_eq!(total, 5, "Total count must be 5 regardless of pagination");
}

// ============================================================
// test_evaluate_user_role_no_groups
// ============================================================

/// A user who belongs to no SCIM groups gets the IDP's default_role ("member").
#[tokio::test]
async fn test_evaluate_user_role_no_groups() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-role-no-groups-idp").await;

    let user = users::create_scim_user(
        &pool,
        "no-groups@example.com",
        Some("ext-no-groups"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let role = scim_groups::evaluate_user_role(&pool, user.id, idp.id)
        .await
        .expect("evaluate_user_role failed");

    // IDP default_role is "member" (from create_test_idp)
    assert_eq!(
        role, "member",
        "User with no groups should get default_role"
    );
}

// ============================================================
// test_evaluate_user_role_admin_group
// ============================================================

/// User in a group whose display_name is in scim_admin_groups → role = "admin".
#[tokio::test]
async fn test_evaluate_user_role_admin_group() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-role-admin-group-idp").await;

    // Set scim_admin_groups = ["admins"] on the IDP
    sqlx::query("UPDATE identity_providers SET scim_admin_groups = $1::jsonb WHERE id = $2")
        .bind(serde_json::json!(["admins"]).to_string())
        .bind(idp.id)
        .execute(&pool)
        .await
        .expect("set scim_admin_groups failed");

    let admin_group = scim_groups::create_scim_group(&pool, "admins", Some("ext-admins"), idp.id)
        .await
        .expect("create_scim_group failed");

    let user = users::create_scim_user(
        &pool,
        "admin-user@example.com",
        Some("ext-admin-user"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    scim_groups::add_scim_group_member(&pool, admin_group.id, user.id)
        .await
        .expect("add_scim_group_member failed");

    let role = scim_groups::evaluate_user_role(&pool, user.id, idp.id)
        .await
        .expect("evaluate_user_role failed");

    assert_eq!(
        role, "admin",
        "User in scim_admin_groups group should get role=admin"
    );
}

// ============================================================
// test_evaluate_user_role_non_admin_group
// ============================================================

/// User in a group NOT in scim_admin_groups → default_role ("member").
#[tokio::test]
async fn test_evaluate_user_role_non_admin_group() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-role-non-admin-group-idp").await;

    // Set scim_admin_groups = ["admin-only"] — the user's group is "developers"
    sqlx::query("UPDATE identity_providers SET scim_admin_groups = $1::jsonb WHERE id = $2")
        .bind(serde_json::json!(["admin-only"]).to_string())
        .bind(idp.id)
        .execute(&pool)
        .await
        .expect("set scim_admin_groups failed");

    let dev_group = scim_groups::create_scim_group(&pool, "developers", Some("ext-devs"), idp.id)
        .await
        .expect("create_scim_group failed");

    let user = users::create_scim_user(
        &pool,
        "dev-user@example.com",
        Some("ext-dev-user"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    scim_groups::add_scim_group_member(&pool, dev_group.id, user.id)
        .await
        .expect("add_scim_group_member failed");

    let role = scim_groups::evaluate_user_role(&pool, user.id, idp.id)
        .await
        .expect("evaluate_user_role failed");

    assert_eq!(
        role, "member",
        "User in a non-admin group should get default_role"
    );
}

// ============================================================
// test_sync_user_role_promotes_to_admin
// ============================================================

/// Add user to admin group, sync_user_role → user.role = "admin" in DB.
#[tokio::test]
async fn test_sync_user_role_promotes_to_admin() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-sync-promote-idp").await;

    sqlx::query("UPDATE identity_providers SET scim_admin_groups = $1::jsonb WHERE id = $2")
        .bind(serde_json::json!(["superadmins"]).to_string())
        .bind(idp.id)
        .execute(&pool)
        .await
        .expect("set scim_admin_groups failed");

    let admin_group =
        scim_groups::create_scim_group(&pool, "superadmins", Some("ext-superadmins"), idp.id)
            .await
            .expect("create_scim_group failed");

    let user = users::create_scim_user(
        &pool,
        "promote-user@example.com",
        Some("ext-promote-user"),
        None,
        None,
        None,
        "member", // starts as member
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    scim_groups::add_scim_group_member(&pool, admin_group.id, user.id)
        .await
        .expect("add_scim_group_member failed");

    scim_groups::sync_user_role(&pool, user.id, idp.id)
        .await
        .expect("sync_user_role failed");

    let updated = users::get_user_by_email(&pool, "promote-user@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user should still exist");

    assert_eq!(
        updated.role, "admin",
        "sync_user_role should promote user to admin when they are in an admin group"
    );
}

// ============================================================
// test_sync_user_role_demotes_to_member
// ============================================================

/// Remove user from admin group, sync_user_role → user.role = "member" in DB.
#[tokio::test]
async fn test_sync_user_role_demotes_to_member() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-sync-demote-idp").await;

    sqlx::query("UPDATE identity_providers SET scim_admin_groups = $1::jsonb WHERE id = $2")
        .bind(serde_json::json!(["ops-admins"]).to_string())
        .bind(idp.id)
        .execute(&pool)
        .await
        .expect("set scim_admin_groups failed");

    let admin_group =
        scim_groups::create_scim_group(&pool, "ops-admins", Some("ext-ops-admins"), idp.id)
            .await
            .expect("create_scim_group failed");

    let user = users::create_scim_user(
        &pool,
        "demote-user@example.com",
        Some("ext-demote-user"),
        None,
        None,
        None,
        "admin", // starts as admin
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Add to admin group
    scim_groups::add_scim_group_member(&pool, admin_group.id, user.id)
        .await
        .expect("add_scim_group_member failed");

    // Verify they are admin after sync
    scim_groups::sync_user_role(&pool, user.id, idp.id)
        .await
        .expect("sync_user_role (initial) failed");

    let after_add = users::get_user_by_email(&pool, "demote-user@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user should exist");
    assert_eq!(
        after_add.role, "admin",
        "Should be admin while in admin group"
    );

    // Remove from admin group
    scim_groups::remove_scim_group_member(&pool, admin_group.id, user.id)
        .await
        .expect("remove_scim_group_member failed");

    // Sync again
    scim_groups::sync_user_role(&pool, user.id, idp.id)
        .await
        .expect("sync_user_role (after remove) failed");

    let after_remove = users::get_user_by_email(&pool, "demote-user@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("user should still exist");

    assert_eq!(
        after_remove.role, "member",
        "sync_user_role should demote user to member when removed from all admin groups"
    );
}

// ============================================================
// test_evaluate_user_role_multiple_groups
// ============================================================

/// User in both an admin group and a non-admin group → "admin" (any match wins).
#[tokio::test]
async fn test_evaluate_user_role_multiple_groups() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-role-multi-group-idp").await;

    // Only "sre-admins" is in scim_admin_groups; "developers" is not
    sqlx::query("UPDATE identity_providers SET scim_admin_groups = $1::jsonb WHERE id = $2")
        .bind(serde_json::json!(["sre-admins"]).to_string())
        .bind(idp.id)
        .execute(&pool)
        .await
        .expect("set scim_admin_groups failed");

    let admin_group =
        scim_groups::create_scim_group(&pool, "sre-admins", Some("ext-sre-admins"), idp.id)
            .await
            .expect("create admin group failed");

    let dev_group = scim_groups::create_scim_group(&pool, "developers", Some("ext-devs2"), idp.id)
        .await
        .expect("create dev group failed");

    let user = users::create_scim_user(
        &pool,
        "multi-group-user@example.com",
        Some("ext-multi-group"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Add user to both groups
    scim_groups::add_scim_group_member(&pool, admin_group.id, user.id)
        .await
        .expect("add to admin group failed");
    scim_groups::add_scim_group_member(&pool, dev_group.id, user.id)
        .await
        .expect("add to dev group failed");

    let role = scim_groups::evaluate_user_role(&pool, user.id, idp.id)
        .await
        .expect("evaluate_user_role failed");

    assert_eq!(
        role, "admin",
        "User in both admin and non-admin groups should be 'admin' (any-match wins)"
    );
}
