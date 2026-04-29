/// Unit-level tests for `ccag::pricing::parse_price_list_csv` and
/// `ccag::pricing::normalize_service_name`.
///
/// These are pure-function tests (no DB, no AWS) living in the integration tree
/// because the test agent cannot write new production files in src/. Once @builder
/// creates `src/pricing/mod.rs` with the stubs, these tests will compile.
///
/// All tests should fail with `unimplemented!()` panics before the implementation
/// is written. No test should fail at compile time.
// Inline unit tests that don't require the `#[cfg(feature = "integration")]` gate —
// the functions under test are pure and need no DB pool.
use ccag::pricing::{normalize_service_name, parse_price_list_csv};

// CSV header used by the AWS Bedrock Foundation Models price list (confirmed by live fetch).
const CSV_HEADER: &str = r#""SKU","OfferTermCode","RateCode","TermType","PriceDescription","EffectiveDate","StartingRange","EndingRange","Unit","PricePerUnit","Currency","Location","Location Type","usageType","operation","Region Code","serviceName""#;

// Build a minimal CSV string from header + rows.
fn csv_with_rows(rows: &[&str]) -> String {
    let mut out = CSV_HEADER.to_string();
    out.push('\n');
    for row in rows {
        out.push_str(row);
        out.push('\n');
    }
    out
}

// Build a single quoted CSV row with the fields the parser cares about.
// Column order matches the header above.
fn make_row(
    sku: &str,
    price_description: &str,
    price_per_unit: &str,
    service_name: &str,
) -> String {
    format!(
        r#""{sku}","JRTCKXETXF","JRTCKXETXF.JRTCKXETXF.6YS6EN2CT7","OnDemand","{price_description}","2024-01-01T00:00:00Z","0","Inf","Units","{price_per_unit}","USD","US East (N. Virginia)","AWS Region","USN-bedrock:InvokedModelUnit:us-east-1","InvokeModel","us-east-1","{service_name}""#
    )
}

// Helper: assert a ModelPricing row has the expected values (ignoring updated_at).
fn assert_pricing(
    row: &ccag::db::model_pricing::ModelPricing,
    model_prefix: &str,
    input_rate: f64,
    output_rate: f64,
    cache_read_rate: f64,
    cache_write_rate: f64,
) {
    assert_eq!(row.model_prefix, model_prefix, "model_prefix mismatch");
    assert!(
        (row.input_rate - input_rate).abs() < 1e-9,
        "input_rate mismatch: expected {input_rate}, got {}",
        row.input_rate
    );
    assert!(
        (row.output_rate - output_rate).abs() < 1e-9,
        "output_rate mismatch: expected {output_rate}, got {}",
        row.output_rate
    );
    assert!(
        (row.cache_read_rate - cache_read_rate).abs() < 1e-9,
        "cache_read_rate mismatch: expected {cache_read_rate}, got {}",
        row.cache_read_rate
    );
    assert!(
        (row.cache_write_rate - cache_write_rate).abs() < 1e-9,
        "cache_write_rate mismatch: expected {cache_write_rate}, got {}",
        row.cache_write_rate
    );
    assert_eq!(
        row.source, "price_list_api",
        "source must be 'price_list_api'"
    );
}

// -----------------------------------------------------------------------
// Test 1: Parses all 4 global dimensions for Claude Opus 4.7
// -----------------------------------------------------------------------
#[test]
fn parses_claude_opus_4_7_global_rates() {
    let service = "Claude Opus 4.7 (Amazon Bedrock Edition)";
    let csv = csv_with_rows(&[
        &make_row(
            "SKU-OPUS-INPUT",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "5.0000000000",
            service,
        ),
        &make_row(
            "SKU-OPUS-OUTPUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "25.0000000000",
            service,
        ),
        &make_row(
            "SKU-OPUS-CREAD",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.5000000000",
            service,
        ),
        &make_row(
            "SKU-OPUS-CWRITE",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "6.2500000000",
            service,
        ),
    ]);

    let result = parse_price_list_csv(&csv).expect("parse should succeed");

    assert_eq!(
        result.len(),
        1,
        "expected exactly 1 ModelPricing row for Opus 4.7"
    );

    let row = &result[0];
    assert_pricing(row, "claude-opus-4-7", 5.0, 25.0, 0.5, 6.25);
    // aws_sku should be populated (any non-None value is acceptable)
    assert!(row.aws_sku.is_some(), "aws_sku should be Some(_)");
}

// -----------------------------------------------------------------------
// Test 2: Regional rows (without ", Global") are ignored
// -----------------------------------------------------------------------
#[test]
fn skips_non_global_rows() {
    let service = "Claude Opus 4.7 (Amazon Bedrock Edition)";

    // Only regional rows — no ", Global" in PriceDescription
    let regional_only = csv_with_rows(&[
        &make_row(
            "SKU-REGIONAL-INPUT",
            "AWS Marketplace software usage|us-east-1|Million Input Tokens Standard",
            "5.0000000000",
            service,
        ),
        &make_row(
            "SKU-REGIONAL-OUTPUT",
            "AWS Marketplace software usage|us-east-1|Million Response Tokens Standard",
            "25.0000000000",
            service,
        ),
        &make_row(
            "SKU-REGIONAL-CREAD",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard",
            "0.5000000000",
            service,
        ),
        &make_row(
            "SKU-REGIONAL-CWRITE",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard",
            "6.2500000000",
            service,
        ),
    ]);

    let result = parse_price_list_csv(&regional_only).expect("parse should succeed");
    assert_eq!(result.len(), 0, "all-regional CSV should produce empty Vec");

    // Mixed: regional + global → only global rates are extracted
    let mixed = csv_with_rows(&[
        // Regional — skip
        &make_row(
            "SKU-REGIONAL-INPUT",
            "AWS Marketplace software usage|us-east-1|Million Input Tokens Standard",
            "99.0",
            service,
        ),
        // Global — use
        &make_row(
            "SKU-GLOBAL-INPUT",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "5.0000000000",
            service,
        ),
        &make_row(
            "SKU-GLOBAL-OUTPUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "25.0000000000",
            service,
        ),
        &make_row(
            "SKU-GLOBAL-CREAD",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.5000000000",
            service,
        ),
        &make_row(
            "SKU-GLOBAL-CWRITE",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "6.2500000000",
            service,
        ),
    ]);

    let result = parse_price_list_csv(&mixed).expect("parse should succeed");
    assert_eq!(
        result.len(),
        1,
        "mixed CSV should yield 1 row (global only)"
    );
    // input_rate should be the global price (5.0), not the regional (99.0)
    assert!(
        (result[0].input_rate - 5.0).abs() < 1e-9,
        "input_rate should be global 5.0, got {}",
        result[0].input_rate
    );
}

// -----------------------------------------------------------------------
// Test 3: 1h TTL cache write rows are skipped; 5m rate is used
// -----------------------------------------------------------------------
#[test]
fn skips_1h_ttl_cache_write() {
    let service = "Claude Opus 4.7 (Amazon Bedrock Edition)";
    let csv = csv_with_rows(&[
        &make_row(
            "SKU-INPUT",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "5.0000000000",
            service,
        ),
        &make_row(
            "SKU-OUTPUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "25.0000000000",
            service,
        ),
        &make_row(
            "SKU-CREAD",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.5000000000",
            service,
        ),
        // 5m write — keep this one
        &make_row(
            "SKU-CWRITE-5M",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "6.2500000000",
            service,
        ),
        // 1h TTL write — skip this one
        &make_row(
            "SKU-CWRITE-1H",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens (1h TTL) - Standard, Global",
            "999.0000000000",
            service,
        ),
    ]);

    let result = parse_price_list_csv(&csv).expect("parse should succeed");
    assert_eq!(result.len(), 1, "expected exactly 1 ModelPricing row");
    // cache_write_rate should be 6.25 (5m), not 999.0 (1h)
    assert!(
        (result[0].cache_write_rate - 6.25).abs() < 1e-9,
        "cache_write_rate should be 5m rate (6.25), got {}",
        result[0].cache_write_rate
    );
}

// -----------------------------------------------------------------------
// Test 4: Model name normalization
// -----------------------------------------------------------------------
#[test]
fn model_name_normalization() {
    // Test the normalization function directly
    assert_eq!(
        normalize_service_name("Claude Opus 4.7 (Amazon Bedrock Edition)"),
        "claude-opus-4-7",
        "Opus 4.7 normalization"
    );
    assert_eq!(
        normalize_service_name("Claude Sonnet 4.6 (Amazon Bedrock Edition)"),
        "claude-sonnet-4-6",
        "Sonnet 4.6 normalization"
    );
    assert_eq!(
        normalize_service_name("Claude Haiku 4.5 (Amazon Bedrock Edition)"),
        "claude-haiku-4-5",
        "Haiku 4.5 normalization"
    );
    // Verify via parse_price_list_csv as well
    let service = "Claude Sonnet 4.6 (Amazon Bedrock Edition)";
    let csv = csv_with_rows(&[
        &make_row(
            "SKU-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "3.0",
            service,
        ),
        &make_row(
            "SKU-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "15.0",
            service,
        ),
        &make_row(
            "SKU-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.3",
            service,
        ),
        &make_row(
            "SKU-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "3.75",
            service,
        ),
    ]);
    let result = parse_price_list_csv(&csv).expect("parse should succeed");
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].model_prefix, "claude-sonnet-4-6",
        "model_prefix for Sonnet 4.6 should be 'claude-sonnet-4-6'"
    );
}

// -----------------------------------------------------------------------
// Test 5: Model with missing dimension is skipped entirely
// -----------------------------------------------------------------------
#[test]
fn skips_model_with_missing_dimension() {
    let service = "Claude Opus 4.7 (Amazon Bedrock Edition)";
    // Only Input and Output — no cache rows → incomplete → skip
    let csv = csv_with_rows(&[
        &make_row(
            "SKU-INPUT",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "5.0000000000",
            service,
        ),
        &make_row(
            "SKU-OUTPUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "25.0000000000",
            service,
        ),
    ]);

    let result = parse_price_list_csv(&csv).expect("parse should succeed");
    assert_eq!(
        result.len(),
        0,
        "model with only 2 of 4 dimensions should be skipped entirely"
    );
}

// -----------------------------------------------------------------------
// Test 6: Multiple models parsed independently
// -----------------------------------------------------------------------
#[test]
fn multiple_models_parsed_independently() {
    let opus = "Claude Opus 4.7 (Amazon Bedrock Edition)";
    let sonnet = "Claude Sonnet 4.6 (Amazon Bedrock Edition)";
    let haiku = "Claude Haiku 4.5 (Amazon Bedrock Edition)";

    let csv = csv_with_rows(&[
        // Opus 4.7
        &make_row(
            "SKU-OP-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "5.0",
            opus,
        ),
        &make_row(
            "SKU-OP-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "25.0",
            opus,
        ),
        &make_row(
            "SKU-OP-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.5",
            opus,
        ),
        &make_row(
            "SKU-OP-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "6.25",
            opus,
        ),
        // Sonnet 4.6
        &make_row(
            "SKU-SO-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "3.0",
            sonnet,
        ),
        &make_row(
            "SKU-SO-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "15.0",
            sonnet,
        ),
        &make_row(
            "SKU-SO-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.3",
            sonnet,
        ),
        &make_row(
            "SKU-SO-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "3.75",
            sonnet,
        ),
        // Haiku 4.5
        &make_row(
            "SKU-HK-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "0.8",
            haiku,
        ),
        &make_row(
            "SKU-HK-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "4.0",
            haiku,
        ),
        &make_row(
            "SKU-HK-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.08",
            haiku,
        ),
        &make_row(
            "SKU-HK-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "1.0",
            haiku,
        ),
    ]);

    let mut result = parse_price_list_csv(&csv).expect("parse should succeed");
    assert_eq!(
        result.len(),
        3,
        "expected 3 ModelPricing rows, one per model"
    );

    // Sort by model_prefix for deterministic assertions
    result.sort_by(|a, b| a.model_prefix.cmp(&b.model_prefix));

    assert_pricing(&result[0], "claude-haiku-4-5", 0.8, 4.0, 0.08, 1.0);
    assert_pricing(&result[1], "claude-opus-4-7", 5.0, 25.0, 0.5, 6.25);
    assert_pricing(&result[2], "claude-sonnet-4-6", 3.0, 15.0, 0.3, 3.75);
}

// -----------------------------------------------------------------------
// Test 7: Extra whitespace and quoting are handled correctly
// -----------------------------------------------------------------------
#[test]
fn handles_extra_whitespace_and_quoting() {
    // Rows with well-quoted fields and decimal prices — sanity-check the csv crate
    // handles normal quoting without issues.
    let service = "Claude Haiku 4.5 (Amazon Bedrock Edition)";
    let csv = csv_with_rows(&[
        &make_row(
            "SKU-WS-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "0.8000000000",
            service,
        ),
        &make_row(
            "SKU-WS-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "4.0000000000",
            service,
        ),
        &make_row(
            "SKU-WS-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.0800000000",
            service,
        ),
        &make_row(
            "SKU-WS-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "1.0000000000",
            service,
        ),
    ]);

    let result =
        parse_price_list_csv(&csv).expect("parse should not fail on well-formed quoted CSV");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model_prefix, "claude-haiku-4-5");
    assert!(
        (result[0].input_rate - 0.8).abs() < 1e-9,
        "input_rate should parse correctly to 0.8"
    );
}

// -----------------------------------------------------------------------
// Test 8: Empty CSV (header only) returns empty Vec
// -----------------------------------------------------------------------
#[test]
fn empty_csv_returns_empty_vec() {
    // Just the header, no data rows
    let csv = format!("{CSV_HEADER}\n");
    let result = parse_price_list_csv(&csv).expect("parse should succeed on header-only CSV");
    assert_eq!(result.len(), 0, "header-only CSV should produce empty Vec");
}

// -----------------------------------------------------------------------
// Test 9: Malformed PricePerUnit causes that model to be skipped
// -----------------------------------------------------------------------
#[test]
fn malformed_price_per_unit_is_skipped() {
    let bad_service = "Claude Opus 4.7 (Amazon Bedrock Edition)";
    let good_service = "Claude Haiku 4.5 (Amazon Bedrock Edition)";

    let csv = csv_with_rows(&[
        // Opus 4.7 has a bad input price → whole model should be skipped
        &make_row(
            "SKU-BAD-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "not-a-number",
            bad_service,
        ),
        &make_row(
            "SKU-BAD-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "25.0",
            bad_service,
        ),
        &make_row(
            "SKU-BAD-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.5",
            bad_service,
        ),
        &make_row(
            "SKU-BAD-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "6.25",
            bad_service,
        ),
        // Haiku 4.5 has all valid prices → should still parse
        &make_row(
            "SKU-GD-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "0.8",
            good_service,
        ),
        &make_row(
            "SKU-GD-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "4.0",
            good_service,
        ),
        &make_row(
            "SKU-GD-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.08",
            good_service,
        ),
        &make_row(
            "SKU-GD-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "1.0",
            good_service,
        ),
    ]);

    let result = parse_price_list_csv(&csv).expect("parse should succeed despite one bad row");
    assert_eq!(
        result.len(),
        1,
        "only the good model (Haiku 4.5) should be returned; Opus with bad price skipped"
    );
    assert_eq!(result[0].model_prefix, "claude-haiku-4-5");
}

// -----------------------------------------------------------------------
// Test 10: Non-Anthropic providers are included if they have all 4 global dims
//
// Design decision: parse_price_list_csv is a generic CSV parser — it returns
// all models with complete Global dimensions, regardless of provider. Filtering
// to Claude-only (if desired) is the caller's responsibility. This keeps the
// parser simple and testable with any model name.
// -----------------------------------------------------------------------
#[test]
fn includes_non_anthropic_models_with_complete_global_dimensions() {
    let llama = "Llama 4 Scout (Amazon Bedrock Edition)";
    let claude = "Claude Haiku 4.5 (Amazon Bedrock Edition)";

    let csv = csv_with_rows(&[
        // Llama 4 (non-Anthropic) with all 4 global dimensions
        &make_row(
            "SKU-LL-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "0.17",
            llama,
        ),
        &make_row(
            "SKU-LL-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "0.6",
            llama,
        ),
        &make_row(
            "SKU-LL-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.017",
            llama,
        ),
        &make_row(
            "SKU-LL-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "0.085",
            llama,
        ),
        // Claude Haiku 4.5 (Anthropic) with all 4 global dimensions
        &make_row(
            "SKU-HK-IN",
            "AWS Marketplace software usage|us-east-1|Input Tokens - Standard, Global",
            "0.8",
            claude,
        ),
        &make_row(
            "SKU-HK-OUT",
            "AWS Marketplace software usage|us-east-1|Output Tokens - Standard, Global",
            "4.0",
            claude,
        ),
        &make_row(
            "SKU-HK-CR",
            "AWS Marketplace software usage|us-east-1|Cache Read Tokens - Standard, Global",
            "0.08",
            claude,
        ),
        &make_row(
            "SKU-HK-CW",
            "AWS Marketplace software usage|us-east-1|Cache Write Tokens - Standard, Global",
            "1.0",
            claude,
        ),
    ]);

    let mut result = parse_price_list_csv(&csv).expect("parse should succeed");
    assert_eq!(
        result.len(),
        2,
        "both Llama and Claude should be returned when all 4 global dimensions are present"
    );

    result.sort_by(|a, b| a.model_prefix.cmp(&b.model_prefix));
    assert_eq!(result[0].model_prefix, "claude-haiku-4-5");
    assert_eq!(result[1].model_prefix, "llama-4-scout");
}
