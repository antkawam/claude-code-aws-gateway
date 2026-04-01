/// Integration tests for SCIM token CRUD operations.
///
/// These tests exercise the `db::scim_tokens` module against a real Postgres database.
/// Run with: `make test-integration`
use crate::helpers;

use ccag::db::scim_tokens;

// ============================================================
// Create and validate
// ============================================================

#[tokio::test]
async fn test_create_and_validate_scim_token() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "test-idp-validate").await;

    let (raw_token, record) =
        scim_tokens::create_scim_token(&pool, idp.id, Some("my-token"), "admin")
            .await
            .expect("create_scim_token failed");

    // The raw token starts with the expected prefix
    assert!(
        raw_token.starts_with("scim-ccag-"),
        "Token must start with 'scim-ccag-'"
    );

    // The record links to the correct IDP
    assert_eq!(record.idp_id, idp.id);
    assert!(record.enabled);
    assert_eq!(record.name.as_deref(), Some("my-token"));
    assert_eq!(record.created_by, "admin");
    assert!(record.last_used_at.is_none());

    // Validation by hash returns the correct record
    let token_hash = scim_tokens::hash_token(&raw_token);
    let validated = scim_tokens::validate_scim_token(&pool, &token_hash)
        .await
        .expect("validate_scim_token failed");

    let validated = validated.expect("Token should be valid");
    assert_eq!(validated.id, record.id);
    assert_eq!(validated.idp_id, idp.id);
}

// ============================================================
// Revoked token is rejected
// ============================================================

#[tokio::test]
async fn test_revoked_token_invalid() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "test-idp-revoke").await;

    let (raw_token, record) = scim_tokens::create_scim_token(&pool, idp.id, None, "admin")
        .await
        .expect("create_scim_token failed");

    // Revoke the token
    let revoked = scim_tokens::revoke_scim_token(&pool, record.id)
        .await
        .expect("revoke_scim_token failed");
    assert!(revoked, "revoke should return true");

    // Validation should return None (disabled)
    let token_hash = scim_tokens::hash_token(&raw_token);
    let result = scim_tokens::validate_scim_token(&pool, &token_hash)
        .await
        .expect("validate_scim_token failed");
    assert!(result.is_none(), "Revoked token must not validate");
}

// ============================================================
// Token prefix format
// ============================================================

#[tokio::test]
async fn test_token_prefix_format() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "test-idp-prefix").await;

    let (raw_token, record) = scim_tokens::create_scim_token(&pool, idp.id, None, "admin")
        .await
        .expect("create_scim_token failed");

    // Raw token starts with "scim-ccag-"
    assert!(
        raw_token.starts_with("scim-ccag-"),
        "Generated token must start with 'scim-ccag-'"
    );

    // Token is 74 chars total: "scim-ccag-" (10) + 64 hex chars
    assert_eq!(
        raw_token.len(),
        74,
        "Token length must be 74 (10 prefix + 64 hex chars)"
    );

    // The stored prefix is the first 16 chars of the raw token + "..."
    assert_eq!(
        record.token_prefix,
        format!("{}...", &raw_token[..16]),
        "Stored prefix must be first 16 chars + '...'"
    );
}

// ============================================================
// List tokens by IDP
// ============================================================

#[tokio::test]
async fn test_list_tokens_by_idp() {
    let pool = helpers::setup_test_db().await;
    let idp1 = helpers::create_test_idp(&pool, "test-idp-list-1").await;
    let idp2 = helpers::create_test_idp(&pool, "test-idp-list-2").await;

    // Create 2 tokens for idp1 and 1 for idp2
    scim_tokens::create_scim_token(&pool, idp1.id, Some("t1"), "admin")
        .await
        .unwrap();
    scim_tokens::create_scim_token(&pool, idp1.id, Some("t2"), "admin")
        .await
        .unwrap();
    scim_tokens::create_scim_token(&pool, idp2.id, Some("t3"), "admin")
        .await
        .unwrap();

    // Listing by idp1 returns exactly 2 tokens
    let idp1_tokens = scim_tokens::list_scim_tokens(&pool, Some(idp1.id))
        .await
        .expect("list_scim_tokens failed");
    assert_eq!(idp1_tokens.len(), 2);
    assert!(idp1_tokens.iter().all(|t| t.idp_id == idp1.id));

    // Listing by idp2 returns exactly 1 token
    let idp2_tokens = scim_tokens::list_scim_tokens(&pool, Some(idp2.id))
        .await
        .expect("list_scim_tokens failed");
    assert_eq!(idp2_tokens.len(), 1);
    assert_eq!(idp2_tokens[0].idp_id, idp2.id);

    // Listing all (None) returns 3 tokens
    let all_tokens = scim_tokens::list_scim_tokens(&pool, None)
        .await
        .expect("list_scim_tokens failed");
    assert_eq!(all_tokens.len(), 3);
}

// ============================================================
// Delete token
// ============================================================

#[tokio::test]
async fn test_delete_token() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "test-idp-delete").await;

    let (raw_token, record) = scim_tokens::create_scim_token(&pool, idp.id, None, "admin")
        .await
        .expect("create_scim_token failed");

    // Delete the token
    let deleted = scim_tokens::delete_scim_token(&pool, record.id)
        .await
        .expect("delete_scim_token failed");
    assert!(deleted, "delete should return true");

    // Deleting again returns false (no row)
    let deleted_again = scim_tokens::delete_scim_token(&pool, record.id)
        .await
        .expect("delete_scim_token second call failed");
    assert!(!deleted_again, "second delete should return false");

    // Validation returns None after deletion
    let token_hash = scim_tokens::hash_token(&raw_token);
    let validated = scim_tokens::validate_scim_token(&pool, &token_hash)
        .await
        .expect("validate after delete failed");
    assert!(validated.is_none(), "Deleted token must not validate");
}

// ============================================================
// last_used_at updates
// ============================================================

#[tokio::test]
async fn test_last_used_at_updates() {
    let pool = helpers::setup_test_db().await;
    let idp = helpers::create_test_idp(&pool, "test-idp-last-used").await;

    let (_, record) = scim_tokens::create_scim_token(&pool, idp.id, None, "admin")
        .await
        .expect("create_scim_token failed");

    // Initially null
    assert!(
        record.last_used_at.is_none(),
        "last_used_at should be NULL initially"
    );

    // Update last_used_at
    scim_tokens::update_last_used(&pool, record.id)
        .await
        .expect("update_last_used failed");

    // Fetch updated record and verify timestamp was set
    let tokens = scim_tokens::list_scim_tokens(&pool, Some(idp.id))
        .await
        .expect("list_scim_tokens failed");
    let updated = tokens
        .iter()
        .find(|t| t.id == record.id)
        .expect("Token not found after update");
    assert!(
        updated.last_used_at.is_some(),
        "last_used_at should be set after update_last_used"
    );
}

// ============================================================
// Invalid token hash
// ============================================================

#[tokio::test]
async fn test_validate_unknown_token_hash_returns_none() {
    let pool = helpers::setup_test_db().await;

    // A hash that doesn't exist in the DB
    let bogus_hash = "0".repeat(64);
    let result = scim_tokens::validate_scim_token(&pool, &bogus_hash)
        .await
        .expect("validate_scim_token should not error on missing hash");
    assert!(result.is_none(), "Unknown hash must return None");
}
