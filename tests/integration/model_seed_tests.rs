use crate::helpers;

/// AC14: On startup against an empty `model_mappings` table, the seed JSON's
/// rows are inserted with `source='seed'`.
///
/// This test verifies that the cold-start seeding mechanism populates the
/// database from the embedded model_seed.json when no mappings exist.
#[tokio::test]
async fn seed_inserts_into_empty_table() {
    let pool = helpers::setup_test_db().await;

    // The migration adds seed rows; clear them to get a clean slate.
    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("Failed to clear model_mappings");

    // Parse the embedded seed JSON
    let seed_rows = ccag::translate::model_seed::parse_seed();
    assert!(
        !seed_rows.is_empty(),
        "Seed JSON must contain at least one row"
    );
    let expected_count = seed_rows.len();

    // Call the seeding function
    let inserted_count = ccag::db::model_mappings::seed_missing(&pool, seed_rows)
        .await
        .expect("seed_missing failed");

    // Assert that all rows were inserted
    assert_eq!(
        inserted_count, expected_count,
        "Should have inserted all {} seed rows",
        expected_count
    );

    // Verify specific known seed rows exist with source='seed'
    let row = sqlx::query!(
        r#"SELECT anthropic_prefix, bedrock_suffix, anthropic_display, source
           FROM model_mappings
           WHERE anthropic_prefix = 'claude-opus-4-8'"#
    )
    .fetch_optional(&pool)
    .await
    .expect("Query failed");

    assert!(row.is_some(), "Expected claude-opus-4-8 to be in seed data");

    let row = row.unwrap();
    assert_eq!(
        row.bedrock_suffix, "anthropic.claude-opus-4-8",
        "Bedrock suffix should match seed JSON"
    );
    assert_eq!(row.source, "seed", "Source should be 'seed'");
    assert_eq!(
        row.anthropic_display,
        Some("claude-opus-4-8".to_string()),
        "Display name should match for exact model ID"
    );
}

/// AC15: On startup against a table with an existing row, the seed for that
/// row does NOT overwrite the existing data (ON CONFLICT DO NOTHING).
///
/// This test verifies that operator-corrected or discovered mappings survive
/// seeding — the seed never overwrites existing rows.
#[tokio::test]
async fn seed_does_not_overwrite_existing_rows() {
    let pool = helpers::setup_test_db().await;

    // Clear migration-inserted seed rows
    sqlx::query("DELETE FROM model_mappings")
        .execute(&pool)
        .await
        .expect("Failed to clear model_mappings");

    // Insert a deliberately WRONG row that conflicts with the seed
    sqlx::query!(
        r#"INSERT INTO model_mappings (anthropic_prefix, bedrock_suffix, anthropic_display, source)
           VALUES ('claude-opus-4-8', 'WRONG_SUFFIX', NULL, 'discovered')"#
    )
    .execute(&pool)
    .await
    .expect("Failed to insert test row");

    // Parse seed and call seed_missing
    let seed_rows = ccag::translate::model_seed::parse_seed();
    let inserted_count = ccag::db::model_mappings::seed_missing(&pool, seed_rows.clone())
        .await
        .expect("seed_missing failed");

    // Assert the conflict was respected — only OTHER rows were inserted
    assert!(
        inserted_count < seed_rows.len(),
        "Should have inserted fewer rows than total seed count due to conflict"
    );

    // Verify the existing row was NOT overwritten
    let row = sqlx::query!(
        r#"SELECT anthropic_prefix, bedrock_suffix, source
           FROM model_mappings
           WHERE anthropic_prefix = 'claude-opus-4-8'"#
    )
    .fetch_one(&pool)
    .await
    .expect("Query failed");

    assert_eq!(
        row.bedrock_suffix, "WRONG_SUFFIX",
        "Existing row should NOT have been overwritten"
    );
    assert_eq!(
        row.source, "discovered",
        "Source should remain 'discovered' from the original insert"
    );

    // Verify that OTHER seed rows (not conflicting) WERE inserted
    let other_seed_row = sqlx::query!(
        r#"SELECT anthropic_prefix, bedrock_suffix, source
           FROM model_mappings
           WHERE anthropic_prefix = 'claude-opus-4-7'"#
    )
    .fetch_optional(&pool)
    .await
    .expect("Query failed");

    assert!(
        other_seed_row.is_some(),
        "Non-conflicting seed rows should have been inserted"
    );
    assert_eq!(
        other_seed_row.unwrap().source,
        "seed",
        "Non-conflicting row source should be 'seed'"
    );
}

/// AC16 (build-time parse validation): The embedded seed JSON parses without
/// errors at compile time. If the JSON is malformed or the schema doesn't
/// match, this test fails and breaks `cargo test`.
///
/// This test also validates the curated seed data:
/// - All entries have non-empty anthropic_prefix and bedrock_suffix
/// - All bedrock_suffix values start with "anthropic."
/// - No entry is a bare stem (would recreate prefix-matching bugs)
#[tokio::test]
async fn seed_json_parses_and_validates() {
    let rows = ccag::translate::model_seed::parse_seed();

    assert!(
        !rows.is_empty(),
        "Seed JSON must contain at least one entry"
    );

    for row in &rows {
        // Basic non-empty checks
        assert!(
            !row.anthropic_prefix.is_empty(),
            "anthropic_prefix must not be empty"
        );
        assert!(
            !row.bedrock_suffix.is_empty(),
            "bedrock_suffix must not be empty"
        );

        // Bedrock suffix format check
        assert!(
            row.bedrock_suffix.starts_with("anthropic."),
            "bedrock_suffix '{}' must start with 'anthropic.'",
            row.bedrock_suffix
        );

        // Guard against bare stems (e.g. "claude-sonnet-4-", "opus")
        // Seed must contain EXACT model IDs, not stripped catch-all forms.
        // Reject entries that end with a dash (partial match pattern).
        assert!(
            !row.anthropic_prefix.ends_with('-'),
            "anthropic_prefix '{}' must be an exact model ID, not a bare stem ending with '-'",
            row.anthropic_prefix
        );

        // Reject single-word stems (e.g. "opus", "sonnet")
        assert!(
            row.anthropic_prefix.contains('-'),
            "anthropic_prefix '{}' must be a fully-qualified model ID with dashes, not a single-word stem",
            row.anthropic_prefix
        );
    }
}

/// AC18 (static analysis): No outbound HTTP call is made for mapping data.
/// The strings "github", "raw.githubusercontent", "http://", "https://" must
/// NOT appear in the seed JSON or in production model-mapping code paths.
///
/// This test reads the embedded seed JSON constant and asserts forbidden
/// substrings are absent. It guards against accidental call-home fetch logic.
#[tokio::test]
async fn seed_data_contains_no_registry_urls() {
    let seed_json = ccag::translate::model_seed::SEED_JSON;

    // Forbidden URL patterns
    let forbidden = &[
        "github",
        "githubusercontent",
        "http://",
        "https://",
        "registry",
        "api.anthropic",
        "console.anthropic",
    ];

    for pattern in forbidden {
        assert!(
            !seed_json.contains(pattern),
            "Seed JSON must not contain '{}' — no call-home fetches allowed",
            pattern
        );
    }
}
