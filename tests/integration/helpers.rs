use sqlx::PgPool;
use uuid::Uuid;

use ccag::db;
use ccag::db::schema::{IdentityProvider, Team, User, VirtualKey};
use ccag::db::spend::RequestLogEntry;

/// Default test database URL (matches docker-compose.test.yml).
fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://proxy:testpass@localhost:5433/proxy_test".to_string())
}

/// Split a Postgres URL into base (without db name) and database name.
fn parse_base_url(url: &str) -> (String, String) {
    let last_slash = url.rfind('/').expect("Invalid database URL: missing /");
    (
        url[..last_slash].to_string(),
        url[last_slash + 1..].to_string(),
    )
}

/// Drop stale `test_*` databases once per process (best-effort).
static CLEANUP_ONCE: std::sync::Once = std::sync::Once::new();

fn schedule_cleanup(base_url: String, default_db: String) {
    CLEANUP_ONCE.call_once(|| {
        tokio::spawn(async move {
            let Ok(pool) = sqlx::PgPool::connect(&format!("{}/{}", base_url, default_db)).await
            else {
                return;
            };
            let rows = sqlx::query_scalar::<_, String>(
                "SELECT datname FROM pg_database WHERE datname LIKE 'test_%'"
            )
            .fetch_all(&pool)
            .await
            .unwrap_or_default();

            for db in rows {
                // Terminate connections and drop — ignore errors (may be in use by current run)
                let _ = sqlx::query(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()"
                ))
                .execute(&pool)
                .await;
                let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db}\""))
                    .execute(&pool)
                    .await;
            }
            pool.close().await;
        });
    });
}

/// Create an isolated database for a single test and run migrations.
///
/// Each test gets its own `test_{uuid}` database, eliminating parallel
/// test interference (truncation races, deadlocks, FK violations).
/// Stale databases from previous runs are cleaned up once per process.
pub async fn setup_test_db() -> PgPool {
    let url = test_database_url();
    let (base_url, default_db) = parse_base_url(&url);
    let db_name = format!("test_{}", Uuid::new_v4().simple());

    // Best-effort cleanup of leftover databases from previous runs
    schedule_cleanup(base_url.clone(), default_db.clone());

    // Connect to the default database to issue CREATE DATABASE
    let admin_pool = sqlx::PgPool::connect(&format!("{}/{}", base_url, default_db))
        .await
        .expect("Failed to connect to template database");

    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin_pool)
        .await
        .unwrap_or_else(|e| panic!("Failed to create test database {db_name}: {e}"));
    admin_pool.close().await;

    // Connect to the new database and run migrations
    let pool = db::connect(&format!("{}/{}", base_url, db_name))
        .await
        .unwrap_or_else(|e| panic!("Failed to connect to test database {db_name}: {e}"));
    db::run_migrations(&pool)
        .await
        .expect("Failed to run migrations");

    pool
}

// --- Factory functions ---

pub async fn create_test_team(pool: &PgPool, name: &str) -> Team {
    db::teams::create_team(pool, name)
        .await
        .expect("create_test_team failed")
}

pub async fn create_test_user(
    pool: &PgPool,
    email: &str,
    team_id: Option<Uuid>,
    role: &str,
) -> User {
    db::users::create_user(pool, email, team_id, role)
        .await
        .expect("create_test_user failed")
}

pub async fn create_test_key(
    pool: &PgPool,
    name: Option<&str>,
    user_id: Option<Uuid>,
    team_id: Option<Uuid>,
) -> (String, VirtualKey) {
    db::keys::create_key(pool, name, user_id, team_id, None)
        .await
        .expect("create_test_key failed")
}

/// Create a test identity provider with sensible defaults.
pub async fn create_test_idp(pool: &PgPool, name: &str) -> IdentityProvider {
    db::idp::create_idp(
        pool,
        name,
        &format!("https://idp-{}.example.com", Uuid::new_v4().simple()),
        Some("client-id"),
        Some("audience"),
        None,
        "authorization_code",
        false,
        "member",
        None,
        None,
        None,
    )
    .await
    .expect("create_test_idp failed")
}

/// Create a test identity provider with `scim_enabled = true`.
///
/// Used by active-user enforcement tests to verify that resolve_oidc_role
/// rejects unprovisioned users when the IDP has SCIM enabled.
pub async fn create_test_idp_scim_enabled(pool: &PgPool, name: &str) -> IdentityProvider {
    let idp = db::idp::create_idp(
        pool,
        name,
        &format!("https://scim-idp-{}.example.com", Uuid::new_v4().simple()),
        Some("client-id"),
        Some("audience"),
        None,
        "authorization_code",
        false,
        "member",
        None,
        None,
        None,
    )
    .await
    .expect("create_test_idp_scim_enabled: create_idp failed");

    // Enable SCIM on the IDP directly via SQL (create_idp doesn't expose scim_enabled)
    sqlx::query("UPDATE identity_providers SET scim_enabled = true WHERE id = $1")
        .bind(idp.id)
        .execute(pool)
        .await
        .expect("create_test_idp_scim_enabled: update failed");

    // Re-fetch to get the updated row
    sqlx::query_as::<_, IdentityProvider>("SELECT * FROM identity_providers WHERE id = $1")
        .bind(idp.id)
        .fetch_one(pool)
        .await
        .expect("create_test_idp_scim_enabled: re-fetch failed")
}

pub fn make_spend_entry(model: &str, user_identity: Option<&str>) -> RequestLogEntry {
    RequestLogEntry {
        key_id: None,
        user_identity: user_identity.map(|s| s.to_string()),
        request_id: Uuid::new_v4().to_string(),
        model: model.to_string(),
        streaming: false,
        duration_ms: 100,
        input_tokens: 500,
        output_tokens: 200,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        stop_reason: Some("end_turn".to_string()),
        tool_count: 0,
        tool_names: vec![],
        turn_count: 1,
        thinking_enabled: false,
        has_system_prompt: false,
        session_id: None,
        project_key: None,
        tool_errors: None,
        has_correction: false,
        content_block_types: vec![],
        system_prompt_hash: None,
        detection_flags: None,
        endpoint_id: None,
    }
}
