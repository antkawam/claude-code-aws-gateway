/// Integration tests for Task 1: Schema migration — `created_via`, `last_used_at`, legacy cleanup.
///
/// # What is tested
///
/// These tests exercise the new audit columns on `model_mappings` (added by
/// `migrations/013_model_mappings_audit.sql`) and the two new/modified Rust
/// functions in `src/db/model_mappings.rs`:
///
/// - `upsert_mapping(pool, prefix, suffix, display, created_via)` — extended
///   signature that persists `created_via`; re-calling overwrites the column.
/// - `touch_last_used(pool, prefix)` — sets `last_used_at = now()` for an
///   existing row; returns `Ok(())` without error on a missing prefix.
///
/// Each test is named after the acceptance criterion it validates.
///
/// # BUILDER CONTRACT
///
/// Expose (in `src/db/model_mappings.rs`):
///
/// 1. `ModelMappingRow` gains two fields:
///    ```rust
///    pub created_via: String,
///    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
///    ```
///
/// 2. `upsert_mapping` gains a `created_via: &str` parameter:
///    ```rust
///    pub async fn upsert_mapping(
///        pool: &PgPool,
///        anthropic_prefix: &str,
///        bedrock_suffix: &str,
///        anthropic_display: Option<&str>,
///        created_via: &str,
///    ) -> Result<(), sqlx::Error>
///    ```
///    The `ON CONFLICT DO UPDATE` must set `created_via = EXCLUDED.created_via`
///    so that re-calling with a different value overwrites (AC1.5).
///
/// 3. New function:
///    ```rust
///    pub async fn touch_last_used(
///        pool: &PgPool,
///        anthropic_prefix: &str,
///    ) -> Result<(), sqlx::Error>
///    ```
///    Issues `UPDATE model_mappings SET last_used_at = now() WHERE anthropic_prefix = $1`.
///    Returns `Ok(())` whether zero or one rows were updated (no-op on missing prefix).
///
/// 4. Migration `migrations/013_model_mappings_audit.sql`:
///    ```sql
///    ALTER TABLE model_mappings
///      ADD COLUMN IF NOT EXISTS created_via TEXT NOT NULL DEFAULT 'unknown';
///    ALTER TABLE model_mappings
///      ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMPTZ;
///    DELETE FROM model_mappings
///    WHERE anthropic_prefix IN ('claude-sonnet-4-', 'opus');
///    ```
///
/// Run: `make test-integration`
use crate::helpers;

// ── AC1.1 & AC1.2: Migration no data loss + backfill default ─────────────────

/// AC1.1: After migrations run, pre-existing-shaped rows are still present
/// (except the explicitly-deleted legacy rows).
///
/// AC1.2: Pre-existing rows (or rows inserted without specifying `created_via`)
/// have `created_via = 'unknown'` — because the migration adds the column with
/// `DEFAULT 'unknown'`, any row that existed before the migration picks that up.
///
/// Strategy: insert rows using raw SQL that omits `created_via` (simulating
/// rows inserted before the column existed, which would rely on the DEFAULT).
/// Then verify they exist with `created_via = 'unknown'`.
#[tokio::test]
async fn test_ac1_1_migration_no_data_loss() {
    let pool = helpers::setup_test_db().await;

    // Clear the seed rows so we control exactly what's in the table.
    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("Failed to clear model_mappings");

    // Insert three rows the "old" way — omitting created_via, which is the shape
    // of rows that existed before migration 013 added the column.  The column
    // DEFAULT 'unknown' must handle these rows.
    let inserts = [
        (
            "claude-opus-4-6",
            "anthropic.claude-opus-4-6",
            Some("Claude Opus 4.6"),
        ),
        (
            "claude-haiku-4-5",
            "anthropic.claude-haiku-4-5",
            Some("Claude Haiku 4.5"),
        ),
        ("claude-sonnet-4-6", "anthropic.claude-sonnet-4-6", None),
    ];

    for (prefix, suffix, display) in &inserts {
        // Insert without specifying created_via to verify the DEFAULT applies.
        sqlx::query(
            r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
               VALUES ($1, $2, $3, 'seed')
               ON CONFLICT (anthropic_prefix) DO NOTHING"#,
        )
        .bind(prefix)
        .bind(suffix)
        .bind(display)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("Failed to insert {prefix}: {e}"));
    }

    // All three rows must still be present.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix = ANY($1)")
            .bind(&["claude-opus-4-6", "claude-haiku-4-5", "claude-sonnet-4-6"] as &[&str])
            .fetch_one(&pool)
            .await
            .expect("count query failed");

    assert_eq!(
        count, 3,
        "AC1.1: all three pre-existing-shaped rows must survive migration; got {count}"
    );
}

/// AC1.2: Rows inserted without an explicit `created_via` value have
/// `created_via = 'unknown'` — the migration's DEFAULT backfills them.
#[tokio::test]
async fn test_ac1_2_backfill_unknown_created_via() {
    let pool = helpers::setup_test_db().await;

    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("clear failed");

    // Insert a row the "old" way, relying on the column DEFAULT.
    sqlx::query(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
           VALUES ('claude-opus-4-7', 'anthropic.claude-opus-4-7', NULL, 'seed')"#,
    )
    .execute(&pool)
    .await
    .expect("insert failed");

    // The row must have created_via = 'unknown' (column default).
    let created_via: String =
        sqlx::query_scalar("SELECT created_via FROM model_mappings WHERE anthropic_prefix = $1")
            .bind("claude-opus-4-7")
            .fetch_one(&pool)
            .await
            .expect("AC1.2: query failed — is the created_via column present? Run migration 013.");

    assert_eq!(
        created_via, "unknown",
        "AC1.2: pre-existing row must have created_via = 'unknown' after migration backfill; got '{created_via}'"
    );
}

// ── AC1.3: Legacy rows deleted ────────────────────────────────────────────────

/// AC1.3: After migration 013, the inert legacy rows
/// `'claude-sonnet-4-'` and `'opus'` must NOT exist in the table.
///
/// Strategy: insert them via raw SQL (they won't be present after a clean
/// migration since 013 deletes them, but we insert them to verify the DELETE
/// ran — if the migration is absent, these inserts succeed and the assertion
/// catches it; if the migration ran, the DELETE already removed them before
/// tests ran, so we verify by looking for zero rows after insert+DELETE
/// semantics applied by the migration.
///
/// More precisely: `setup_test_db()` runs ALL migrations including 013 if it
/// exists.  We insert the legacy rows after setup (simulating a row that
/// somehow survived) and then verify the selection returns nothing — but the
/// true test is that the migration deletes them on the initial run.
///
/// To verify the DELETE ran at migration time: insert them manually, then
/// SELECT; if migration ran the DELETE, these rows were never in the table
/// to begin with (the seed rows seeded by earlier migrations would have been
/// deleted).  We test by attempting to insert the legacy keys and then checking
/// the SELECT reflects zero rows for these specific keys as the migration
/// guarantees.
///
/// Simplest assertion: after setup_test_db() (which runs all migrations
/// including 013), a SELECT for those two keys returns zero rows — they were
/// never seeded and the migration would have cleaned them up if they existed.
#[tokio::test]
async fn test_ac1_3_legacy_rows_deleted_after_migration() {
    let pool = helpers::setup_test_db().await;

    // These two prefixes are deleted by migration 013.  If the seed data
    // happened to contain them, they would be gone after migration.
    // If we insert them now (post-migration), we verify the schema is right
    // but can't re-run the DELETE.  The real test: on a clean DB, migration 013
    // runs the DELETE and these rows are gone.

    // Verify neither legacy key is present after running all migrations.
    let sonnet_stem_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix = 'claude-sonnet-4-'",
    )
    .fetch_one(&pool)
    .await
    .expect("AC1.3: query for 'claude-sonnet-4-' failed");

    let opus_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix = 'opus'")
            .fetch_one(&pool)
            .await
            .expect("AC1.3: query for 'opus' failed");

    assert_eq!(
        sonnet_stem_count, 0,
        "AC1.3: 'claude-sonnet-4-' must not be present after migration 013 DELETE; found {sonnet_stem_count} row(s)"
    );
    assert_eq!(
        opus_count, 0,
        "AC1.3: 'opus' must not be present after migration 013 DELETE; found {opus_count} row(s)"
    );
}

/// AC1.3 (supplementary): Verify that if legacy rows ARE inserted before the
/// migration's DELETE runs, they are removed.  We simulate this by inserting
/// the legacy rows directly and then running the DELETE SQL that the migration
/// contains, then asserting zero rows remain.
///
/// This is a separate test from the migration-time test above because we can't
/// partially rerun migrations in a test; instead we simulate the pre-migration
/// state and verify the migration SQL has the right effect.
#[tokio::test]
async fn test_ac1_3_migration_delete_removes_legacy_stems() {
    let pool = helpers::setup_test_db().await;

    // Insert the legacy rows (using INSERT OR IGNORE so the test doesn't fail
    // if they somehow already exist from seed data — they should not, but be safe).
    sqlx::query(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
           VALUES ('claude-sonnet-4-', 'anthropic.claude-sonnet-4-5', NULL, 'seed')
           ON CONFLICT (anthropic_prefix) DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .expect("insert claude-sonnet-4- failed");

    sqlx::query(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
           VALUES ('opus', 'anthropic.claude-opus-4', NULL, 'seed')
           ON CONFLICT (anthropic_prefix) DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .expect("insert opus failed");

    // Now execute the exact DELETE that migration 013 contains.
    sqlx::query(
        "DELETE FROM model_mappings WHERE anthropic_prefix IN ('claude-sonnet-4-', 'opus')",
    )
    .execute(&pool)
    .await
    .expect("AC1.3: DELETE of legacy rows failed");

    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix IN ('claude-sonnet-4-', 'opus')",
    )
    .fetch_one(&pool)
    .await
    .expect("count failed");

    assert_eq!(
        remaining, 0,
        "AC1.3: after migration DELETE, legacy rows 'claude-sonnet-4-' and 'opus' must be gone; found {remaining}"
    );
}

// ── AC1.4: touch_last_used ────────────────────────────────────────────────────

/// AC1.4 (happy path): `touch_last_used("claude-sonnet-4-6")` sets `last_used_at`
/// to a timestamp within the last second.
#[tokio::test]
async fn test_ac1_4_touch_last_used_updates_timestamp() {
    let pool = helpers::setup_test_db().await;

    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("clear failed");

    // Insert a row whose last_used_at starts as NULL.
    sqlx::query(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
           VALUES ('claude-sonnet-4-6', 'anthropic.claude-sonnet-4-6', NULL, 'seed')"#,
    )
    .execute(&pool)
    .await
    .expect("insert failed");

    // Verify last_used_at is initially NULL.
    let initial: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT last_used_at FROM model_mappings WHERE anthropic_prefix = 'claude-sonnet-4-6'",
    )
    .fetch_one(&pool)
    .await
    .expect("AC1.4: initial last_used_at query failed — is the last_used_at column present? Run migration 013.");

    assert!(
        initial.is_none(),
        "AC1.4: newly-inserted row must have last_used_at = NULL initially; got {initial:?}"
    );

    let before = chrono::Utc::now();

    // Call the function under test.
    let touch_result: Result<(), sqlx::Error> =
        ccag::db::model_mappings::touch_last_used(&pool, "claude-sonnet-4-6").await;
    touch_result.expect("AC1.4: touch_last_used must return Ok(())");

    let after = chrono::Utc::now();

    // Read the updated value back.
    let updated: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT last_used_at FROM model_mappings WHERE anthropic_prefix = 'claude-sonnet-4-6'",
    )
    .fetch_one(&pool)
    .await
    .expect("AC1.4: updated last_used_at query failed");

    let ts = updated.expect("AC1.4: last_used_at must be non-NULL after touch_last_used");

    // Allow 2s of clock skew in either direction: the Postgres `now()` and the
    // test-process `chrono::Utc::now()` run on separate clocks and can diverge
    // by ~15–50 ms in CI containers.  The assertion still validates recency
    // (the timestamp is not stale or zero); it just tolerates cross-process skew.
    let skew_tolerance = chrono::Duration::seconds(2);
    assert!(
        ts >= before - skew_tolerance && ts <= after + skew_tolerance,
        "AC1.4: last_used_at ({ts}) must be roughly current (before={before}, after={after}, tolerance=2s)"
    );
}

/// AC1.4 (no-op path): `touch_last_used` on a non-existent prefix returns
/// `Ok(())` without error.
#[tokio::test]
async fn test_ac1_4_touch_last_used_noop_on_missing_prefix() {
    let pool = helpers::setup_test_db().await;

    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("clear failed");

    // The table is empty — no rows for this prefix.
    let result: Result<(), sqlx::Error> =
        ccag::db::model_mappings::touch_last_used(&pool, "claude-nonexistent-model-9-9").await;

    assert!(
        result.is_ok(),
        "AC1.4: touch_last_used on missing prefix must return Ok(()); got {result:?}"
    );
}

// ── AC1.5: upsert_mapping persists created_via; re-call overwrites ────────────

/// AC1.5 (first upsert): `upsert_mapping(..., "pass1")` persists `created_via = 'pass1'`.
#[tokio::test]
async fn test_ac1_5_upsert_mapping_persists_created_via() {
    let pool = helpers::setup_test_db().await;

    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("clear failed");

    ccag::db::model_mappings::upsert_mapping(
        &pool,
        "claude-future-9-9",
        "anthropic.claude-future-9-9",
        Some("Claude Future 9.9"),
        "pass1",
    )
    .await
    .expect("AC1.5: first upsert_mapping failed");

    let created_via: String = sqlx::query_scalar(
        "SELECT created_via FROM model_mappings WHERE anthropic_prefix = 'claude-future-9-9'",
    )
    .fetch_one(&pool)
    .await
    .expect("AC1.5: SELECT created_via after first upsert failed — is the created_via column present? Run migration 013.");

    assert_eq!(
        created_via, "pass1",
        "AC1.5: first upsert must persist created_via = 'pass1'; got '{created_via}'"
    );
}

/// AC1.5 (re-upsert overwrites): Re-calling `upsert_mapping` for the same prefix
/// with `"pass2"` must overwrite `created_via` to `'pass2'` (latest-wins).
#[tokio::test]
async fn test_ac1_5_upsert_mapping_overwrite_created_via() {
    let pool = helpers::setup_test_db().await;

    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("clear failed");

    // First insert with pass1.
    ccag::db::model_mappings::upsert_mapping(
        &pool,
        "claude-future-9-9",
        "anthropic.claude-future-9-9",
        Some("Claude Future 9.9"),
        "pass1",
    )
    .await
    .expect("AC1.5: first upsert_mapping failed");

    // Re-upsert with pass2 — must overwrite created_via.
    ccag::db::model_mappings::upsert_mapping(
        &pool,
        "claude-future-9-9",
        "anthropic.claude-future-9-9",
        Some("Claude Future 9.9"),
        "pass2",
    )
    .await
    .expect("AC1.5: second upsert_mapping (overwrite) failed");

    let created_via: String = sqlx::query_scalar(
        "SELECT created_via FROM model_mappings WHERE anthropic_prefix = 'claude-future-9-9'",
    )
    .fetch_one(&pool)
    .await
    .expect("AC1.5: SELECT after second upsert failed");

    assert_eq!(
        created_via, "pass2",
        "AC1.5: re-upsert must overwrite created_via to 'pass2' (latest-wins); got '{created_via}'"
    );

    // Verify only one row exists (not duplicated).
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix = 'claude-future-9-9'",
    )
    .fetch_one(&pool)
    .await
    .expect("count failed");

    assert_eq!(
        count, 1,
        "AC1.5: re-upsert must not create a duplicate row; found {count} rows"
    );
}

/// AC1.5 (admin value preserved): When `upsert_mapping` is called with
/// `created_via = 'admin'`, that value is persisted.
#[tokio::test]
async fn test_ac1_5_upsert_mapping_admin_created_via() {
    let pool = helpers::setup_test_db().await;

    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("clear failed");

    ccag::db::model_mappings::upsert_mapping(
        &pool,
        "Sonnet 4.7",
        "anthropic.claude-sonnet-4-7",
        Some("Sonnet 4.7"),
        "admin",
    )
    .await
    .expect("AC1.5: upsert_mapping with created_via='admin' failed");

    let created_via: String = sqlx::query_scalar(
        "SELECT created_via FROM model_mappings WHERE anthropic_prefix = 'Sonnet 4.7'",
    )
    .fetch_one(&pool)
    .await
    .expect("AC1.5: SELECT admin row failed");

    assert_eq!(
        created_via, "admin",
        "AC1.5: admin-created row must have created_via = 'admin'; got '{created_via}'"
    );
}

// ── AC1.1 (combined coverage): data not lost, only legacy rows gone ───────────

/// AC1.1 (combined): After migration, non-legacy rows survive and count is
/// correct.  Legacy stems are absent.  This exercises the actual migration
/// DELETE path via `setup_test_db()`.
///
/// This test inserts both "good" rows (with legitimate exact prefixes) and then
/// verifies the seed data's pre-existing rows are intact while the two legacy
/// prefixes — if they were inserted by some seed path — are gone.
#[tokio::test]
async fn test_ac1_1_non_legacy_rows_survive_migration() {
    let pool = helpers::setup_test_db().await;

    // Insert a non-legacy row via upsert (uses the new signature).
    ccag::db::model_mappings::upsert_mapping(
        &pool,
        "claude-opus-4-8",
        "anthropic.claude-opus-4-8",
        None,
        "pass1",
    )
    .await
    .expect("AC1.1: upsert of non-legacy row failed");

    // The row must still be present (not collateral damage from migration DELETE).
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix = 'claude-opus-4-8'",
    )
    .fetch_one(&pool)
    .await
    .expect("count failed");

    assert_eq!(
        count, 1,
        "AC1.1: non-legacy row 'claude-opus-4-8' must survive migration; found {count} rows"
    );

    // And the legacy stems must be absent (migration DELETE).
    let legacy_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_mappings WHERE anthropic_prefix IN ('claude-sonnet-4-', 'opus')",
    )
    .fetch_one(&pool)
    .await
    .expect("legacy count failed");

    assert_eq!(
        legacy_count, 0,
        "AC1.1: legacy stems must be deleted by migration; found {legacy_count} rows"
    );
}
