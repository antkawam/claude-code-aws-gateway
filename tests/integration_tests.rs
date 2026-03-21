#![cfg(feature = "integration")]

mod integration;

use integration::helpers;

/// Smoke test: connect to DB, run migrations, insert and read a key.
#[tokio::test]
async fn smoke_connect_and_create_key() {
    let pool = helpers::setup_test_db().await;

    // Create a team and user first
    let team = helpers::create_test_team(&pool, "smoke-team").await;
    let user = helpers::create_test_user(&pool, "smoke@test.com", Some(team.id), "admin").await;

    // Create a virtual key
    let (raw_key, vk) =
        helpers::create_test_key(&pool, Some("smoke-key"), Some(user.id), Some(team.id)).await;

    assert!(!raw_key.is_empty());
    assert!(raw_key.starts_with("sk-proxy-"));
    assert_eq!(vk.name.as_deref(), Some("smoke-key"));
    assert_eq!(vk.user_id, Some(user.id));
    assert_eq!(vk.team_id, Some(team.id));
    assert!(vk.is_active);

    // Verify we can read it back
    let keys = ccag::db::keys::list_keys(&pool).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].id, vk.id);

    // Verify hash lookup works
    let hash = ccag::db::keys::hash_key(&raw_key);
    assert_eq!(hash, vk.key_hash);
}

/// Smoke test: verify cache_version bumps on key creation.
#[tokio::test]
async fn smoke_cache_version_bumps() {
    let pool = helpers::setup_test_db().await;

    let v1 = ccag::db::settings::get_cache_version(&pool).await.unwrap();
    helpers::create_test_key(&pool, Some("bump-key"), None, None).await;
    let v2 = ccag::db::settings::get_cache_version(&pool).await.unwrap();

    assert!(
        v2 > v1,
        "cache_version should bump after key creation: v1={v1}, v2={v2}"
    );
}
