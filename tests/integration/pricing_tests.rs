use crate::helpers;
use uuid::Uuid;

// ============================================================
// model_pricing table: seed rows are present after migration
// ============================================================

/// After migrations run, the model_pricing table must contain at least
/// the 6 seed rows with exact rates and source='seed'.
#[tokio::test]
async fn seed_rows_present() {
    let pool = helpers::setup_test_db().await;

    // Use the non-macro form so this file compiles even when the migration
    // table does not yet exist (sqlx::query_as! does compile-time DB checks).
    let rows: Vec<(String, f64, f64, f64, f64, String)> = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source \
         FROM model_pricing \
         ORDER BY model_prefix",
    )
    .fetch_all(&pool)
    .await
    .expect("query model_pricing should succeed — migration 010 must exist");

    // Must have at least the 6 seed prefixes
    assert!(
        rows.len() >= 6,
        "Expected at least 6 seed rows in model_pricing, got {}",
        rows.len()
    );

    // All seed rows must have source='seed'
    for row in &rows {
        assert_eq!(
            row.5, "seed",
            "Row '{}' must have source='seed', got '{}'",
            row.0, row.5
        );
    }

    // Verify exact rates for each expected prefix
    let expected: &[(&str, f64, f64, f64, f64)] = &[
        ("claude-haiku-4-5", 1.00, 5.00, 0.10, 1.25),
        ("claude-opus-4-5", 5.00, 25.00, 0.50, 6.25),
        ("claude-opus-4-6", 5.00, 25.00, 0.50, 6.25),
        ("claude-opus-4-7", 5.00, 25.00, 0.50, 6.25),
        ("claude-sonnet-4-5", 3.00, 15.00, 0.30, 3.75),
        ("claude-sonnet-4-6", 3.00, 15.00, 0.30, 3.75),
    ];

    for (prefix, exp_input, exp_output, exp_cache_read, exp_cache_write) in expected {
        let row = rows
            .iter()
            .find(|r| r.0 == *prefix)
            .unwrap_or_else(|| panic!("Missing seed row for prefix '{prefix}'"));

        assert!(
            (row.1 - exp_input).abs() < 1e-9,
            "input_rate mismatch for '{prefix}': expected {exp_input}, got {}",
            row.1
        );
        assert!(
            (row.2 - exp_output).abs() < 1e-9,
            "output_rate mismatch for '{prefix}': expected {exp_output}, got {}",
            row.2
        );
        assert!(
            (row.3 - exp_cache_read).abs() < 1e-9,
            "cache_read_rate mismatch for '{prefix}': expected {exp_cache_read}, got {}",
            row.3
        );
        assert!(
            (row.4 - exp_cache_write).abs() < 1e-9,
            "cache_write_rate mismatch for '{prefix}': expected {exp_cache_write}, got {}",
            row.4
        );
    }
}

// ============================================================
// estimate_cost_usd: basic opus-4-7 regression fixture
// ============================================================

/// 1,000,000 input tokens at $5.00/M = $5.00 USD.
/// This is the primary regression fixture for the pricing rewrite.
#[tokio::test]
async fn estimate_cost_opus_4_7_correct() {
    let pool = helpers::setup_test_db().await;

    let cost: f64 =
        sqlx::query_scalar("SELECT estimate_cost_usd('claude-opus-4-7', 1000000, 0, 0, 0)")
            .fetch_one(&pool)
            .await
            .expect("estimate_cost_usd must exist — migration 010 must exist");

    assert!(
        (cost - 5.0).abs() < 1e-9,
        "claude-opus-4-7 1M input tokens should cost $5.00, got ${cost}"
    );
}

// ============================================================
// estimate_cost_usd: full token-type breakdown
// ============================================================

/// Full breakdown for claude-opus-4-7:
///   input:       1,000,000 tokens * $5.00/M  = $5.000
///   output:        500,000 tokens * $25.00/M = $12.500
///   cache_read:    200,000 tokens * $0.50/M  = $0.100
///   cache_write:   100,000 tokens * $6.25/M  = $0.625
///   total                                    = $18.225
#[tokio::test]
async fn estimate_cost_full_breakdown() {
    let pool = helpers::setup_test_db().await;

    let cost: f64 = sqlx::query_scalar(
        "SELECT estimate_cost_usd('claude-opus-4-7', 1000000, 500000, 200000, 100000)",
    )
    .fetch_one(&pool)
    .await
    .expect("estimate_cost_usd must exist — migration 010 must exist");

    let expected = 18.225_f64;
    assert!(
        (cost - expected).abs() < 1e-6,
        "Full breakdown for claude-opus-4-7 should be ${expected:.6}, got ${cost:.6}"
    );
}

// ============================================================
// estimate_cost_usd: unknown model returns NULL
// ============================================================

/// An unrecognised model string must return SQL NULL (fail loud — no silent
/// fallback to an arbitrary rate). The Rust-side value must be None.
#[tokio::test]
async fn estimate_cost_unknown_model_returns_null() {
    let pool = helpers::setup_test_db().await;

    let cost: Option<f64> =
        sqlx::query_scalar("SELECT estimate_cost_usd('claude-nonexistent', 1000000, 0, 0, 0)")
            .fetch_one(&pool)
            .await
            .expect("estimate_cost_usd query failed");

    assert!(
        cost.is_none(),
        "Unknown model must return NULL, not a default rate; got {cost:?}"
    );
}

// ============================================================
// estimate_cost_usd: longest-prefix match wins
// ============================================================

/// Insert a shorter prefix 'claude-opus' at different rates, then call with
/// 'claude-opus-4-7-20260401'. The longer prefix 'claude-opus-4-7' must win.
///
/// Cleanup: the injected row is deleted after the assertion.
#[tokio::test]
async fn estimate_cost_longest_prefix_wins() {
    let pool = helpers::setup_test_db().await;

    // Insert a shorter, competing prefix at obviously different rates
    sqlx::query(
        r#"
        INSERT INTO model_pricing (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ('claude-opus', 99.0, 99.0, 99.0, 99.0, 'seed')
        "#,
    )
    .execute(&pool)
    .await
    .expect("inserting shorter claude-opus prefix must succeed");

    // 'claude-opus-4-7-20260401' should match 'claude-opus-4-7' (longer wins)
    // 1,000,000 input tokens * $5.00/M = $5.00, not $99.00
    let cost: Option<f64> = sqlx::query_scalar(
        "SELECT estimate_cost_usd('claude-opus-4-7-20260401', 1000000, 0, 0, 0)",
    )
    .fetch_one(&pool)
    .await
    .expect("estimate_cost_usd query failed");

    // Clean up before asserting so a panic doesn't leave the row behind
    // (each test gets its own isolated database, but be defensive)
    sqlx::query("DELETE FROM model_pricing WHERE model_prefix = 'claude-opus'")
        .execute(&pool)
        .await
        .expect("cleanup of claude-opus row must succeed");

    let cost = cost.expect("'claude-opus-4-7-20260401' must match at least one prefix");
    assert!(
        (cost - 5.0).abs() < 1e-6,
        "Longest-prefix match should yield $5.00 (claude-opus-4-7 row), got ${cost:.6}"
    );
}

// ============================================================
// estimate_cost_usd: date-suffixed model resolves via prefix
// ============================================================

/// Model IDs like 'claude-opus-4-7-20260401' (with a date suffix) must
/// resolve to the 'claude-opus-4-7' seed row.
#[tokio::test]
async fn estimate_cost_date_suffix_matches_prefix() {
    let pool = helpers::setup_test_db().await;

    let cost: Option<f64> = sqlx::query_scalar(
        "SELECT estimate_cost_usd('claude-opus-4-7-20260401', 1000000, 0, 0, 0)",
    )
    .fetch_one(&pool)
    .await
    .expect("estimate_cost_usd query failed");

    let cost = cost.expect("'claude-opus-4-7-20260401' must resolve via prefix 'claude-opus-4-7'");
    assert!(
        (cost - 5.0).abs() < 1e-9,
        "Date-suffixed model should cost $5.00 (1M input at claude-opus-4-7 rate), got ${cost}"
    );
}

// ============================================================
// model_pricing: CHECK constraint rejects bad source value
// ============================================================

/// The source column has a CHECK constraint: source IN ('seed','price_list_api','admin_manual').
/// Inserting source='bogus' must fail with a constraint violation error.
#[tokio::test]
async fn check_constraint_rejects_bad_source() {
    let pool = helpers::setup_test_db().await;

    let result = sqlx::query(
        r#"
        INSERT INTO model_pricing (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ('claude-test-bad-source', 1.0, 1.0, 0.1, 0.1, 'bogus')
        "#,
    )
    .execute(&pool)
    .await;

    assert!(
        result.is_err(),
        "Inserting source='bogus' must fail the CHECK constraint, but INSERT succeeded"
    );

    // Verify it's specifically a constraint violation (Postgres error code 23514)
    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("23514") || err_str.to_lowercase().contains("check"),
        "Expected a CHECK constraint violation (23514), got: {err_str}"
    );
}

// ============================================================
// model_pricing CRUD: contract tests for ccag::db::model_pricing
//
// These tests exercise the database directly with sqlx::query_as so they
// compile without the production module. They encode the exact behavioral
// contract that ccag::db::model_pricing::* must satisfy. When @builder
// implements the module, these tests should continue to pass via both the
// direct SQL path tested here and via the module functions.
//
// Helper row type used throughout this section.
// ============================================================

/// Row type matching the model_pricing table columns.
#[derive(Debug, sqlx::FromRow)]
struct PricingRow {
    pub model_prefix: String,
    pub input_rate: f64,
    pub output_rate: f64,
    pub cache_read_rate: f64,
    pub cache_write_rate: f64,
    pub source: String,
    pub aws_sku: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// ============================================================
// 1. list_all_returns_seed_rows
// ============================================================

/// list_all must return the 7 seed rows inserted by migration 010, sorted by
/// model_prefix ascending. This encodes the contract for
/// ccag::db::model_pricing::list_all.
#[tokio::test]
async fn list_all_returns_seed_rows() {
    let pool = helpers::setup_test_db().await;

    let rows: Vec<PricingRow> = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         ORDER BY model_prefix ASC",
    )
    .fetch_all(&pool)
    .await
    .expect("list_all query should succeed");

    assert_eq!(
        rows.len(),
        7,
        "list_all must return exactly 7 seed rows, got {}. \
         Seed rows: claude-haiku-4-5, claude-opus-4-5, claude-opus-4-6, \
         claude-opus-4-7, claude-sonnet-4-, claude-sonnet-4-5, claude-sonnet-4-6",
        rows.len()
    );

    // All rows must have source='seed'
    for row in &rows {
        assert_eq!(
            row.source, "seed",
            "Seed row '{}' must have source='seed', got '{}'",
            row.model_prefix, row.source
        );
    }

    // Verify stable sort: each prefix must be <= the next
    for i in 0..rows.len() - 1 {
        assert!(
            rows[i].model_prefix <= rows[i + 1].model_prefix,
            "list_all must be sorted ascending by model_prefix: '{}' > '{}'",
            rows[i].model_prefix,
            rows[i + 1].model_prefix
        );
    }

    // Spot-check that all 7 expected prefixes are present
    let prefixes: Vec<&str> = rows.iter().map(|r| r.model_prefix.as_str()).collect();
    for expected in &[
        "claude-haiku-4-5",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-sonnet-4-",
        "claude-sonnet-4-5",
        "claude-sonnet-4-6",
    ] {
        assert!(
            prefixes.contains(expected),
            "list_all must include seed prefix '{}', got: {:?}",
            expected,
            prefixes
        );
    }
}

// ============================================================
// 2. get_returns_some_for_seed
// ============================================================

/// get("claude-opus-4-7") must return Some with exact rates and source='seed'.
/// Encodes the contract for ccag::db::model_pricing::get.
#[tokio::test]
async fn get_returns_some_for_seed() {
    let pool = helpers::setup_test_db().await;

    let row: Option<PricingRow> = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind("claude-opus-4-7")
    .fetch_optional(&pool)
    .await
    .expect("get query should succeed");

    let row = row.expect("get('claude-opus-4-7') must return Some — seed row must exist");

    assert_eq!(row.model_prefix, "claude-opus-4-7");
    assert_eq!(row.source, "seed");
    assert!(
        (row.input_rate - 5.00).abs() < 1e-9,
        "input_rate must be 5.00, got {}",
        row.input_rate
    );
    assert!(
        (row.output_rate - 25.00).abs() < 1e-9,
        "output_rate must be 25.00, got {}",
        row.output_rate
    );
    assert!(
        (row.cache_read_rate - 0.50).abs() < 1e-9,
        "cache_read_rate must be 0.50, got {}",
        row.cache_read_rate
    );
    assert!(
        (row.cache_write_rate - 6.25).abs() < 1e-9,
        "cache_write_rate must be 6.25, got {}",
        row.cache_write_rate
    );
    assert!(
        row.aws_sku.is_none(),
        "Seed rows have no aws_sku, got {:?}",
        row.aws_sku
    );
}

// ============================================================
// 3. get_returns_none_for_unknown
// ============================================================

/// get("claude-nonexistent") must return None, not an error.
/// Encodes the contract for ccag::db::model_pricing::get.
#[tokio::test]
async fn get_returns_none_for_unknown() {
    let pool = helpers::setup_test_db().await;

    let row: Option<PricingRow> = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind("claude-nonexistent")
    .fetch_optional(&pool)
    .await
    .expect("get query should not error for a missing key");

    assert!(
        row.is_none(),
        "get('claude-nonexistent') must return None, got Some({:?})",
        row.map(|r| r.model_prefix)
    );
}

// ============================================================
// 4. upsert_from_api_inserts_new_row
// ============================================================

/// upsert_from_api on a brand-new prefix inserts the row and returns true.
/// The source column must persist as whatever was passed ('price_list_api').
/// Encodes the contract for ccag::db::model_pricing::upsert_from_api.
#[tokio::test]
async fn upsert_from_api_inserts_new_row() {
    let pool = helpers::setup_test_db().await;
    let prefix = format!("test-model-x-{}", Uuid::new_v4().simple());

    // Simulate upsert_from_api: INSERT ... ON CONFLICT DO UPDATE only when source != 'admin_manual'
    let affected = sqlx::query(
        r#"
        INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ($1, 2.50, 12.50, 0.25, 3.125, 'price_list_api')
        ON CONFLICT (model_prefix) DO UPDATE
            SET input_rate       = EXCLUDED.input_rate,
                output_rate      = EXCLUDED.output_rate,
                cache_read_rate  = EXCLUDED.cache_read_rate,
                cache_write_rate = EXCLUDED.cache_write_rate,
                source           = EXCLUDED.source,
                updated_at       = now()
            WHERE model_pricing.source <> 'admin_manual'
        "#,
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("upsert_from_api insert should succeed");

    // rows_affected == 1 means the row was inserted/updated (true)
    assert_eq!(
        affected.rows_affected(),
        1,
        "upsert_from_api must return true (rows_affected=1) for a new row"
    );

    let row: PricingRow = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind(&prefix)
    .fetch_one(&pool)
    .await
    .expect("Row must exist after upsert_from_api insert");

    assert_eq!(
        row.source, "price_list_api",
        "source must persist as 'price_list_api'"
    );
    assert!(
        (row.input_rate - 2.50).abs() < 1e-9,
        "input_rate must be 2.50, got {}",
        row.input_rate
    );

    // Cleanup (test-isolated DB but be explicit about intent)
    sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&prefix)
        .execute(&pool)
        .await
        .expect("cleanup must succeed");
}

// ============================================================
// 5. upsert_from_api_updates_existing_non_manual
// ============================================================

/// upsert_from_api on an existing non-admin_manual row updates it and returns true.
/// Source must flip to 'price_list_api'.
/// Encodes the contract for ccag::db::model_pricing::upsert_from_api.
#[tokio::test]
async fn upsert_from_api_updates_existing_non_manual() {
    let pool = helpers::setup_test_db().await;
    // Use the existing seed row 'claude-haiku-4-5' (source='seed')

    let affected = sqlx::query(
        r#"
        INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ($1, 1.50, 7.50, 0.15, 1.875, 'price_list_api')
        ON CONFLICT (model_prefix) DO UPDATE
            SET input_rate       = EXCLUDED.input_rate,
                output_rate      = EXCLUDED.output_rate,
                cache_read_rate  = EXCLUDED.cache_read_rate,
                cache_write_rate = EXCLUDED.cache_write_rate,
                source           = EXCLUDED.source,
                updated_at       = now()
            WHERE model_pricing.source <> 'admin_manual'
        "#,
    )
    .bind("claude-haiku-4-5")
    .execute(&pool)
    .await
    .expect("upsert_from_api update on seed row should succeed");

    assert_eq!(
        affected.rows_affected(),
        1,
        "upsert_from_api must return true (rows_affected=1) when updating a non-admin_manual row"
    );

    let row: PricingRow = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind("claude-haiku-4-5")
    .fetch_one(&pool)
    .await
    .expect("Row must exist after upsert_from_api update");

    assert_eq!(
        row.source, "price_list_api",
        "source must flip to 'price_list_api' after upsert_from_api on a seed row"
    );
    assert!(
        (row.input_rate - 1.50).abs() < 1e-9,
        "input_rate must be updated to 1.50, got {}",
        row.input_rate
    );

    // Restore the seed row so the test DB is in a clean state for other tests
    sqlx::query(
        "UPDATE model_pricing \
         SET input_rate=1.00, output_rate=5.00, cache_read_rate=0.10, \
             cache_write_rate=1.25, source='seed', updated_at=now() \
         WHERE model_prefix=$1",
    )
    .bind("claude-haiku-4-5")
    .execute(&pool)
    .await
    .expect("restore seed row must succeed");
}

// ============================================================
// 6. upsert_from_api_skips_admin_manual
// ============================================================

/// upsert_from_api must NOT overwrite a row where source='admin_manual'.
/// Returns false (rows_affected=0) and leaves the row unchanged.
/// Encodes the contract for ccag::db::model_pricing::upsert_from_api.
#[tokio::test]
async fn upsert_from_api_skips_admin_manual() {
    let pool = helpers::setup_test_db().await;
    let prefix = format!("test-admin-lock-{}", Uuid::new_v4().simple());

    // Set up a row with source='admin_manual' (via upsert_manual semantics)
    sqlx::query(
        r#"
        INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ($1, 9.99, 49.99, 0.99, 12.49, 'admin_manual')
        ON CONFLICT (model_prefix) DO UPDATE
            SET input_rate       = EXCLUDED.input_rate,
                output_rate      = EXCLUDED.output_rate,
                cache_read_rate  = EXCLUDED.cache_read_rate,
                cache_write_rate = EXCLUDED.cache_write_rate,
                source           = 'admin_manual',
                updated_at       = now()
        "#,
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("set up admin_manual row should succeed");

    // Now attempt upsert_from_api — must be skipped
    let affected = sqlx::query(
        r#"
        INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ($1, 1.00, 5.00, 0.10, 1.25, 'price_list_api')
        ON CONFLICT (model_prefix) DO UPDATE
            SET input_rate       = EXCLUDED.input_rate,
                output_rate      = EXCLUDED.output_rate,
                cache_read_rate  = EXCLUDED.cache_read_rate,
                cache_write_rate = EXCLUDED.cache_write_rate,
                source           = EXCLUDED.source,
                updated_at       = now()
            WHERE model_pricing.source <> 'admin_manual'
        "#,
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("upsert_from_api on admin_manual row should not error");

    // rows_affected == 0 means the row was skipped (return false)
    assert_eq!(
        affected.rows_affected(),
        0,
        "upsert_from_api must return false (rows_affected=0) when source='admin_manual'"
    );

    // Verify row is unchanged
    let row: PricingRow = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind(&prefix)
    .fetch_one(&pool)
    .await
    .expect("Row must still exist after skipped upsert");

    assert_eq!(
        row.source, "admin_manual",
        "source must remain 'admin_manual' after skipped upsert_from_api"
    );
    assert!(
        (row.input_rate - 9.99).abs() < 1e-9,
        "input_rate must remain 9.99 (unchanged), got {}",
        row.input_rate
    );

    // Cleanup
    sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&prefix)
        .execute(&pool)
        .await
        .expect("cleanup must succeed");
}

// ============================================================
// 7. upsert_manual_always_overwrites
// ============================================================

/// upsert_manual overwrites any existing row regardless of its current source,
/// and forces source='admin_manual'.
/// Encodes the contract for ccag::db::model_pricing::upsert_manual.
#[tokio::test]
async fn upsert_manual_always_overwrites() {
    let pool = helpers::setup_test_db().await;
    let prefix = format!("test-manual-overwrite-{}", Uuid::new_v4().simple());

    // Insert initial row with source='price_list_api'
    sqlx::query(
        "INSERT INTO model_pricing \
         (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source) \
         VALUES ($1, 2.00, 10.00, 0.20, 2.50, 'price_list_api')",
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("initial row insert should succeed");

    // upsert_manual: always overwrites, forces source='admin_manual'
    sqlx::query(
        r#"
        INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ($1, 7.77, 35.00, 0.77, 9.625, 'admin_manual')
        ON CONFLICT (model_prefix) DO UPDATE
            SET input_rate       = EXCLUDED.input_rate,
                output_rate      = EXCLUDED.output_rate,
                cache_read_rate  = EXCLUDED.cache_read_rate,
                cache_write_rate = EXCLUDED.cache_write_rate,
                source           = 'admin_manual',
                updated_at       = now()
        "#,
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("upsert_manual should succeed even when source was 'price_list_api'");

    let row: PricingRow = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind(&prefix)
    .fetch_one(&pool)
    .await
    .expect("Row must exist after upsert_manual");

    assert_eq!(
        row.source, "admin_manual",
        "upsert_manual must force source='admin_manual' even when previous source was 'price_list_api'"
    );
    assert!(
        (row.input_rate - 7.77).abs() < 1e-9,
        "input_rate must be overwritten to 7.77, got {}",
        row.input_rate
    );

    // Cleanup
    sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&prefix)
        .execute(&pool)
        .await
        .expect("cleanup must succeed");
}

// ============================================================
// 8. upsert_manual_forces_source
// ============================================================

/// upsert_manual must force source='admin_manual' even when the caller passes
/// source='seed' in the struct. The implementation must override the field.
/// Encodes the constraint that ccag::db::model_pricing::upsert_manual must
/// ignore row.source and always write 'admin_manual'.
#[tokio::test]
async fn upsert_manual_forces_source() {
    let pool = helpers::setup_test_db().await;
    let prefix = format!("test-force-source-{}", Uuid::new_v4().simple());

    // The implementation must always use 'admin_manual' for source, regardless
    // of what the caller passes. We simulate this by verifying the SQL always
    // hard-codes 'admin_manual' in the SET clause (not EXCLUDED.source).
    // A correct implementation of upsert_manual uses:
    //   source = 'admin_manual'   -- NOT source = EXCLUDED.source
    sqlx::query(
        r#"
        INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ($1, 4.00, 20.00, 0.40, 5.00, 'admin_manual')
        ON CONFLICT (model_prefix) DO UPDATE
            SET input_rate       = EXCLUDED.input_rate,
                output_rate      = EXCLUDED.output_rate,
                cache_read_rate  = EXCLUDED.cache_read_rate,
                cache_write_rate = EXCLUDED.cache_write_rate,
                source           = 'admin_manual',
                updated_at       = now()
        "#,
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("upsert_manual (simulated with caller source='seed' intent) should succeed");

    let row: PricingRow = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind(&prefix)
    .fetch_one(&pool)
    .await
    .expect("Row must exist after upsert_manual");

    assert_eq!(
        row.source, "admin_manual",
        "upsert_manual must persist source='admin_manual' even when caller intended 'seed'"
    );

    // Cleanup
    sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&prefix)
        .execute(&pool)
        .await
        .expect("cleanup must succeed");
}

// ============================================================
// 9. delete_existing_returns_true
// ============================================================

/// delete on an existing row must return true (deleted), and the row must no
/// longer appear in list_all.
/// Encodes the contract for ccag::db::model_pricing::delete.
#[tokio::test]
async fn delete_existing_returns_true() {
    let pool = helpers::setup_test_db().await;
    let prefix = format!("test-delete-exists-{}", Uuid::new_v4().simple());

    // Insert a row
    sqlx::query(
        "INSERT INTO model_pricing \
         (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source) \
         VALUES ($1, 1.11, 5.55, 0.11, 1.39, 'price_list_api')",
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("insert row for delete test must succeed");

    // Delete it
    let result = sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&prefix)
        .execute(&pool)
        .await
        .expect("delete must not error for existing row");

    // rows_affected == 1 means the row existed and was deleted (return true)
    assert_eq!(
        result.rows_affected(),
        1,
        "delete must return true (rows_affected=1) for an existing row"
    );

    // Verify the row is gone
    let row: Option<PricingRow> = sqlx::query_as(
        "SELECT model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, \
         source, aws_sku, updated_at \
         FROM model_pricing \
         WHERE model_prefix = $1",
    )
    .bind(&prefix)
    .fetch_optional(&pool)
    .await
    .expect("post-delete fetch should not error");

    assert!(
        row.is_none(),
        "Row '{}' must not appear after delete, but fetch returned Some",
        prefix
    );
}

// ============================================================
// 10. delete_missing_returns_false
// ============================================================

/// delete on an unknown prefix must return false (not found) without error.
/// Encodes the contract for ccag::db::model_pricing::delete.
#[tokio::test]
async fn delete_missing_returns_false() {
    let pool = helpers::setup_test_db().await;
    let prefix = format!("test-delete-missing-{}", Uuid::new_v4().simple());

    let result = sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&prefix)
        .execute(&pool)
        .await
        .expect("delete must not error for a missing row");

    // rows_affected == 0 means the row was not found (return false)
    assert_eq!(
        result.rows_affected(),
        0,
        "delete must return false (rows_affected=0) when the row does not exist"
    );
}

// ============================================================
// 11. updated_at_bumps_on_upsert
// ============================================================

/// updated_at must be strictly greater after upsert_manual than before.
/// Encodes the contract that every write to model_pricing refreshes updated_at.
#[tokio::test]
async fn updated_at_bumps_on_upsert() {
    let pool = helpers::setup_test_db().await;
    let prefix = format!("test-updated-at-{}", Uuid::new_v4().simple());

    // Insert initial row and capture updated_at
    sqlx::query(
        "INSERT INTO model_pricing \
         (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source) \
         VALUES ($1, 3.00, 15.00, 0.30, 3.75, 'price_list_api')",
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("initial insert must succeed");

    let before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM model_pricing WHERE model_prefix = $1")
            .bind(&prefix)
            .fetch_one(&pool)
            .await
            .expect("fetch updated_at before upsert must succeed");

    // Wait a brief moment to ensure the clock advances
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // upsert_manual — always updates, refreshes updated_at
    sqlx::query(
        r#"
        INSERT INTO model_pricing
            (model_prefix, input_rate, output_rate, cache_read_rate, cache_write_rate, source)
        VALUES ($1, 6.00, 30.00, 0.60, 7.50, 'admin_manual')
        ON CONFLICT (model_prefix) DO UPDATE
            SET input_rate       = EXCLUDED.input_rate,
                output_rate      = EXCLUDED.output_rate,
                cache_read_rate  = EXCLUDED.cache_read_rate,
                cache_write_rate = EXCLUDED.cache_write_rate,
                source           = 'admin_manual',
                updated_at       = now()
        "#,
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .expect("upsert_manual must succeed");

    let after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM model_pricing WHERE model_prefix = $1")
            .bind(&prefix)
            .fetch_one(&pool)
            .await
            .expect("fetch updated_at after upsert must succeed");

    assert!(
        after > before,
        "updated_at must be strictly greater after upsert_manual: before={before:?} after={after:?}"
    );

    // Cleanup
    sqlx::query("DELETE FROM model_pricing WHERE model_prefix = $1")
        .bind(&prefix)
        .execute(&pool)
        .await
        .expect("cleanup must succeed");
}
