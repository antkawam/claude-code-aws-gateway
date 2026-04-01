/// Integration tests for SCIM Phase 2 User CRUD and active-user enforcement.
///
/// These tests exercise `db::users` SCIM functions (create_scim_user,
/// get_user_by_external_id, set_user_active, update_scim_user,
/// list_users_for_idp) and the active-user DB-layer behavior that backs
/// `resolve_oidc_role`.
///
/// Run with: `make test-integration`
use uuid::Uuid;

use ccag::db;
use ccag::db::users;
use ccag::scim::filter::ScimFilter;

use crate::helpers;

// ============================================================
// test_create_scim_user
// ============================================================

/// Create a SCIM user and verify every field is populated correctly.
#[tokio::test]
async fn test_create_scim_user() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-create-idp").await;

    let user = users::create_scim_user(
        &pool,
        "alice@example.com",
        Some("okta-user-123"),
        Some("Alice Smith"),
        Some("Alice"),
        Some("Smith"),
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    assert_eq!(user.email, "alice@example.com");
    assert_eq!(user.external_id.as_deref(), Some("okta-user-123"));
    assert_eq!(user.display_name.as_deref(), Some("Alice Smith"));
    assert_eq!(user.given_name.as_deref(), Some("Alice"));
    assert_eq!(user.family_name.as_deref(), Some("Smith"));
    assert_eq!(user.role, "member");
    assert_eq!(user.idp_id, Some(idp.id));
    assert!(user.active, "SCIM user should be active=true by default");
    assert!(user.scim_managed, "SCIM user should have scim_managed=true");
    assert!(user.team_id.is_none(), "No team assigned at creation");
}

// ============================================================
// test_get_user_by_external_id
// ============================================================

/// Create a user with an external_id, then fetch it by (external_id, idp_id).
#[tokio::test]
async fn test_get_user_by_external_id() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-ext-id-idp").await;

    let created = users::create_scim_user(
        &pool,
        "bob@example.com",
        Some("ext-bob-001"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let fetched = users::get_user_by_external_id(&pool, "ext-bob-001", idp.id)
        .await
        .expect("get_user_by_external_id failed")
        .expect("User should be found");

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.email, "bob@example.com");
    assert_eq!(fetched.external_id.as_deref(), Some("ext-bob-001"));
}

// ============================================================
// test_get_user_by_external_id_wrong_idp
// ============================================================

/// Fetching a user with the wrong IDP id should return None.
#[tokio::test]
async fn test_get_user_by_external_id_wrong_idp() {
    let pool = helpers::setup_test_db().await;
    let idp_a = helpers::create_test_idp(&pool, "scim-ext-wrong-idp-a").await;
    let idp_b = helpers::create_test_idp(&pool, "scim-ext-wrong-idp-b").await;

    users::create_scim_user(
        &pool,
        "carol@example.com",
        Some("ext-carol-001"),
        None,
        None,
        None,
        "member",
        idp_a.id,
    )
    .await
    .expect("create_scim_user failed");

    // Fetch using IDP-B's ID — should return None
    let result = users::get_user_by_external_id(&pool, "ext-carol-001", idp_b.id)
        .await
        .expect("get_user_by_external_id failed");

    assert!(
        result.is_none(),
        "Fetching user with wrong IDP id must return None"
    );
}

// ============================================================
// test_set_user_active
// ============================================================

/// Deactivate a user and verify the active flag is updated in the DB.
#[tokio::test]
async fn test_set_user_active() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-active-idp").await;

    let user = users::create_scim_user(
        &pool,
        "dave@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    assert!(user.active, "User should start active");

    let updated = users::set_user_active(&pool, user.id, false)
        .await
        .expect("set_user_active failed");
    assert!(
        updated,
        "set_user_active should return true when row was updated"
    );

    // Re-fetch and verify
    let fetched = users::get_user_by_email(&pool, "dave@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("User should still exist");

    assert!(!fetched.active, "User should now have active=false");
}

// ============================================================
// test_update_scim_user
// ============================================================

/// Update a SCIM user's fields and verify all changes persist.
#[tokio::test]
async fn test_update_scim_user() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-update-idp").await;

    let user = users::create_scim_user(
        &pool,
        "eve@example.com",
        Some("ext-eve-001"),
        Some("Eve Original"),
        Some("Eve"),
        Some("Original"),
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let updated = users::update_scim_user(
        &pool,
        user.id,
        "eve-updated@example.com",
        Some("ext-eve-updated"),
        Some("Eve Updated"),
        Some("Eve"),
        Some("Updated"),
        true,
    )
    .await
    .expect("update_scim_user failed")
    .expect("User should be returned after update");

    assert_eq!(updated.email, "eve-updated@example.com");
    assert_eq!(updated.external_id.as_deref(), Some("ext-eve-updated"));
    assert_eq!(updated.display_name.as_deref(), Some("Eve Updated"));
    assert_eq!(updated.family_name.as_deref(), Some("Updated"));
    assert!(updated.active);
}

/// update_scim_user can deactivate a user by setting active=false.
#[tokio::test]
async fn test_update_scim_user_deactivate() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-update-deactivate-idp").await;

    let user = users::create_scim_user(
        &pool,
        "frank@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let updated = users::update_scim_user(
        &pool,
        user.id,
        "frank@example.com",
        None,
        None,
        None,
        None,
        false, // deactivate
    )
    .await
    .expect("update_scim_user failed")
    .expect("User should be returned after update");

    assert!(!updated.active, "User should be deactivated");
}

/// update_scim_user on a non-existent ID returns None.
#[tokio::test]
async fn test_update_scim_user_not_found() {
    let pool = helpers::setup_test_db().await;

    let result = users::update_scim_user(
        &pool,
        Uuid::new_v4(), // does not exist
        "nobody@example.com",
        None,
        None,
        None,
        None,
        true,
    )
    .await
    .expect("update_scim_user should not error on missing user");

    assert!(
        result.is_none(),
        "update_scim_user should return None for a non-existent user id"
    );
}

// ============================================================
// test_list_users_for_idp
// ============================================================

/// Create users for two IDPs, list for IDP-A, verify only IDP-A users returned.
#[tokio::test]
async fn test_list_users_for_idp() {
    let pool = helpers::setup_test_db().await;
    let idp_a = helpers::create_test_idp(&pool, "scim-list-idp-a").await;
    let idp_b = helpers::create_test_idp(&pool, "scim-list-idp-b").await;

    // Create 2 users for IDP-A and 1 for IDP-B
    users::create_scim_user(
        &pool,
        "list-user-1@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp_a.id,
    )
    .await
    .expect("create_scim_user failed");

    users::create_scim_user(
        &pool,
        "list-user-2@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp_a.id,
    )
    .await
    .expect("create_scim_user failed");

    users::create_scim_user(
        &pool,
        "list-user-3@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp_b.id,
    )
    .await
    .expect("create_scim_user failed");

    let (users_a, total_a) = users::list_users_for_idp(&pool, idp_a.id, None, 0, 100)
        .await
        .expect("list_users_for_idp failed");

    // total_a reflects users scoped to IDP-A (scim_managed=true, idp_id=idp_a)
    // plus any non-scim_managed users (none in this test)
    assert_eq!(users_a.len(), 2, "Should return exactly 2 users for IDP-A");
    assert_eq!(total_a, 2, "Total count should be 2 for IDP-A");

    // Verify all returned users belong to IDP-A
    for u in &users_a {
        assert_eq!(u.idp_id, Some(idp_a.id), "User should belong to IDP-A");
    }

    let (users_b, total_b) = users::list_users_for_idp(&pool, idp_b.id, None, 0, 100)
        .await
        .expect("list_users_for_idp failed");
    assert_eq!(users_b.len(), 1, "Should return exactly 1 user for IDP-B");
    assert_eq!(total_b, 1);
}

// ============================================================
// test_list_users_for_idp_with_filter
// ============================================================

/// List with a userName eq filter returns only the matched user.
#[tokio::test]
async fn test_list_users_for_idp_with_filter() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-filter-idp").await;

    users::create_scim_user(
        &pool,
        "alice@filter.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    users::create_scim_user(
        &pool,
        "bob@filter.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let filter = ScimFilter::Eq("userName".to_string(), "alice@filter.com".to_string());

    let (result, total) = users::list_users_for_idp(&pool, idp.id, Some(&filter), 0, 100)
        .await
        .expect("list_users_for_idp with filter failed");

    assert_eq!(result.len(), 1, "Filter should return only alice");
    assert_eq!(total, 1);
    assert_eq!(result[0].email, "alice@filter.com");
}

/// Filter with userName eq is case-insensitive.
#[tokio::test]
async fn test_list_users_for_idp_filter_case_insensitive() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-filter-case-idp").await;

    users::create_scim_user(
        &pool,
        "CaseSensitive@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Match with different casing
    let filter = ScimFilter::Eq(
        "userName".to_string(),
        "casesensitive@example.com".to_string(),
    );

    let (result, total) = users::list_users_for_idp(&pool, idp.id, Some(&filter), 0, 100)
        .await
        .expect("list_users_for_idp with filter failed");

    assert_eq!(result.len(), 1, "Case-insensitive eq filter should match");
    assert_eq!(total, 1);
}

/// Filter with userName co (contains) returns matching users.
#[tokio::test]
async fn test_list_users_for_idp_with_contains_filter() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-contains-idp").await;

    users::create_scim_user(
        &pool,
        "alpha@contains.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    users::create_scim_user(
        &pool,
        "beta@contains.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    users::create_scim_user(
        &pool,
        "gamma@other.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // "contains.com" should match alpha and beta but not gamma
    let filter = ScimFilter::Contains("userName".to_string(), "contains.com".to_string());

    let (result, total) = users::list_users_for_idp(&pool, idp.id, Some(&filter), 0, 100)
        .await
        .expect("list_users_for_idp with contains filter failed");

    assert_eq!(
        result.len(),
        2,
        "Contains filter should match alpha and beta"
    );
    assert_eq!(total, 2);
}

/// Filter with userName sw (starts with) returns matching users.
#[tokio::test]
async fn test_list_users_for_idp_with_startswith_filter() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-startswith-idp").await;

    users::create_scim_user(
        &pool,
        "prefix-user1@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    users::create_scim_user(
        &pool,
        "prefix-user2@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    users::create_scim_user(
        &pool,
        "other-user@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let filter = ScimFilter::StartsWith("userName".to_string(), "prefix-".to_string());

    let (result, total) = users::list_users_for_idp(&pool, idp.id, Some(&filter), 0, 100)
        .await
        .expect("list_users_for_idp with startswith filter failed");

    assert_eq!(
        result.len(),
        2,
        "StartsWith filter should match prefix-user1 and prefix-user2"
    );
    assert_eq!(total, 2);
}

/// Compound AND filter matches users satisfying both conditions.
#[tokio::test]
async fn test_list_users_for_idp_with_and_filter() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-and-filter-idp").await;

    let u1 = users::create_scim_user(
        &pool,
        "and-active@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let u2 = users::create_scim_user(
        &pool,
        "and-inactive@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Deactivate u2
    users::set_user_active(&pool, u2.id, false)
        .await
        .expect("set_user_active failed");

    // Filter: userName eq "and-active@example.com" AND active eq true
    let filter = ScimFilter::And(
        Box::new(ScimFilter::Eq(
            "userName".to_string(),
            "and-active@example.com".to_string(),
        )),
        Box::new(ScimFilter::Eq("active".to_string(), "true".to_string())),
    );

    let (result, total) = users::list_users_for_idp(&pool, idp.id, Some(&filter), 0, 100)
        .await
        .expect("list_users_for_idp with AND filter failed");

    assert_eq!(
        result.len(),
        1,
        "AND filter should return only the active user"
    );
    assert_eq!(total, 1);
    assert_eq!(result[0].id, u1.id);
}

// ============================================================
// test_list_users_for_idp_pagination
// ============================================================

/// Pagination: create 5 users, list with offset=2, limit=2.
/// Verify 2 returned and total_count=5.
#[tokio::test]
async fn test_list_users_for_idp_pagination() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-pagination-idp").await;

    for i in 1..=5 {
        users::create_scim_user(
            &pool,
            &format!("page-user{i:02}@example.com"),
            None,
            None,
            None,
            None,
            "member",
            idp.id,
        )
        .await
        .expect("create_scim_user failed");
    }

    // offset=2 (0-based), limit=2 → should return users 3 and 4 (sorted by email)
    let (page, total) = users::list_users_for_idp(&pool, idp.id, None, 2, 2)
        .await
        .expect("list_users_for_idp failed");

    assert_eq!(page.len(), 2, "Should return exactly 2 results per page");
    assert_eq!(total, 5, "Total count should be 5 regardless of pagination");
}

/// Pagination: offset beyond the last user returns empty results with correct total.
#[tokio::test]
async fn test_list_users_for_idp_pagination_beyond_end() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-pagination-beyond-idp").await;

    for i in 1..=3 {
        users::create_scim_user(
            &pool,
            &format!("beyond-user{i}@example.com"),
            None,
            None,
            None,
            None,
            "member",
            idp.id,
        )
        .await
        .expect("create_scim_user failed");
    }

    // offset=10 is beyond all 3 users
    let (page, total) = users::list_users_for_idp(&pool, idp.id, None, 10, 2)
        .await
        .expect("list_users_for_idp failed");

    assert!(
        page.is_empty(),
        "Page beyond end should return empty results"
    );
    assert_eq!(total, 3, "Total count should still be 3");
}

// ============================================================
// Active-User Enforcement (DB layer)
// ============================================================
//
// resolve_oidc_role() requires a &GatewayState which is complex to construct
// in integration tests (requires AWS SDK clients, Arc<RwLock<>>, etc.).
//
// We test the DB-layer behavior that resolve_oidc_role() depends on:
//   1. get_user_by_email returns users with the active flag set correctly
//   2. get_user_by_email returns users with the correct idp_id
//   3. get_enabled_idps returns IDPs with the correct scim_enabled flag
//
// The handler-level integration is verified via e2e tests.

/// Active user: get_user_by_email returns user with active=true.
#[tokio::test]
async fn test_resolve_oidc_role_active_user_db_layer() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-active-role-idp").await;

    users::create_scim_user(
        &pool,
        "active-oidc@example.com",
        None,
        None,
        None,
        None,
        "admin",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let user = users::get_user_by_email(&pool, "active-oidc@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("User should exist");

    // resolve_oidc_role checks active before returning the role
    assert!(
        user.active,
        "Active user: active flag must be true for resolve_oidc_role to succeed"
    );
    assert_eq!(user.role, "admin");
}

/// Inactive user: get_user_by_email returns user with active=false.
/// This is what causes resolve_oidc_role to return Err("Your account has been deactivated").
#[tokio::test]
async fn test_resolve_oidc_role_inactive_user_db_layer() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-inactive-role-idp").await;

    let user = users::create_scim_user(
        &pool,
        "inactive-oidc@example.com",
        None,
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    // Deactivate the user
    users::set_user_active(&pool, user.id, false)
        .await
        .expect("set_user_active failed");

    let fetched = users::get_user_by_email(&pool, "inactive-oidc@example.com")
        .await
        .expect("get_user_by_email failed")
        .expect("User should still exist after deactivation");

    // resolve_oidc_role branches on `!user.active` → Err("Your account has been deactivated")
    assert!(
        !fetched.active,
        "Inactive user: active flag must be false, causing resolve_oidc_role to return Err"
    );
}

/// SCIM-managed IDP: get_enabled_idps returns IDP with scim_enabled=true.
/// This is what causes resolve_oidc_role to return Err("User not provisioned...").
#[tokio::test]
async fn test_resolve_oidc_role_scim_managed_idp_db_layer() {
    let pool = helpers::setup_test_db().await;

    // create_test_idp creates an IDP with scim_enabled=false (default).
    // We need one with scim_enabled=true.
    let idp = helpers::create_test_idp_scim_enabled(&pool, "scim-managed-idp").await;

    // Verify get_enabled_idps returns it with scim_enabled=true
    let enabled_idps = db::idp::get_enabled_idps(&pool)
        .await
        .expect("get_enabled_idps failed");

    let found = enabled_idps.iter().find(|i| i.id == idp.id);
    assert!(
        found.is_some(),
        "SCIM-enabled IDP must appear in get_enabled_idps"
    );
    assert!(
        found.unwrap().scim_enabled,
        "IDP must have scim_enabled=true for resolve_oidc_role to block unprovisioned users"
    );
}

/// When IDP has scim_enabled=true and user does not exist, get_user_by_email returns None.
/// Combined with IDP check, resolve_oidc_role returns Err("User not provisioned...").
#[tokio::test]
async fn test_resolve_oidc_role_scim_idp_no_user_db_layer() {
    let pool = helpers::setup_test_db().await;
    let _idp = helpers::create_test_idp_scim_enabled(&pool, "scim-no-user-idp").await;

    // User does not exist in the DB
    let fetched = users::get_user_by_email(&pool, "notprovisioned@example.com")
        .await
        .expect("get_user_by_email failed");

    assert!(
        fetched.is_none(),
        "Non-provisioned user must not exist in DB"
    );

    // resolve_oidc_role then checks IDPs, finds scim_enabled=true for the user's IDP,
    // and returns Err("User not provisioned. Contact your administrator.")
}

// ============================================================
// Soft-delete (set_user_active = false) preserves user record
// ============================================================

/// After deactivation, the user record is still queryable.
/// Spend history (virtual keys, logs) is preserved.
#[tokio::test]
async fn test_soft_delete_preserves_user_record() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-soft-delete-idp").await;

    let user = users::create_scim_user(
        &pool,
        "soft-delete@example.com",
        Some("ext-soft-delete"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("create_scim_user failed");

    let deleted = users::set_user_active(&pool, user.id, false)
        .await
        .expect("set_user_active failed");
    assert!(deleted);

    // User record must still exist (soft-delete, not hard-delete)
    let fetched = users::get_user_by_email(&pool, "soft-delete@example.com")
        .await
        .expect("get_user_by_email failed");

    assert!(
        fetched.is_some(),
        "Soft-delete should not remove the user row"
    );
    assert_eq!(
        fetched.unwrap().external_id.as_deref(),
        Some("ext-soft-delete")
    );
}

// ============================================================
// Uniqueness enforcement
// ============================================================

/// Creating two SCIM users with the same external_id + idp_id should fail.
#[tokio::test]
async fn test_create_scim_user_duplicate_external_id_same_idp() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-dup-ext-idp").await;

    users::create_scim_user(
        &pool,
        "dup-user-1@example.com",
        Some("dup-ext-id"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("First create_scim_user should succeed");

    // Second user with same external_id but different email should fail (unique constraint)
    let result = users::create_scim_user(
        &pool,
        "dup-user-2@example.com",
        Some("dup-ext-id"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await;

    // external_id has a partial unique index: unique where external_id IS NOT NULL
    // The DB enforces this at the row level
    assert!(
        result.is_err(),
        "Creating a second user with the same external_id must fail"
    );
}

/// Creating a SCIM user with the same email as an existing user should fail.
#[tokio::test]
async fn test_create_scim_user_duplicate_email() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "scim-dup-email-idp").await;

    users::create_scim_user(
        &pool,
        "dup-email@example.com",
        Some("ext-dup-1"),
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await
    .expect("First create_scim_user should succeed");

    // Same email (email column has a unique constraint)
    let result = users::create_scim_user(
        &pool,
        "dup-email@example.com",
        Some("ext-dup-2"), // different external_id
        None,
        None,
        None,
        "member",
        idp.id,
    )
    .await;

    assert!(
        result.is_err(),
        "Creating a second user with the same email must fail"
    );
}
