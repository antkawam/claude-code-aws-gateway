/// Tests for Task 2: Canonicalizer module.
///
/// # What is tested
///
/// These tests exercise `canonicalize_model_id` from the new module
/// `src/translate/canonicalize.rs`, which will be wired into
/// `src/translate/mod.rs` as `pub mod canonicalize` by the Builder.
///
/// # BUILDER CONTRACT
///
/// The Builder must expose:
///
/// ```rust
/// // src/translate/canonicalize.rs
/// pub fn canonicalize_model_id(input: &str) -> Option<String>;
/// ```
///
/// And register it in `src/translate/mod.rs`:
/// ```rust
/// pub mod canonicalize;
/// ```
///
/// The implementation must satisfy the conservative pipeline described in the spec
/// (Section 2):
/// 1. Trim leading/trailing whitespace.
/// 2. If the trimmed input does NOT start with `"claude-"`, prepend it.
/// 3. Strip a trailing `-YYYYMMDD` date suffix via `strip_date_suffix`.
/// 4. Reject inputs containing characters outside `[a-zA-Z0-9._:-]`.
/// 5. Reject the empty-family case (`"claude-"` with nothing after).
///
/// Case is preserved; no lowercasing. Beta-bracket suffixes (`[1m]`) are out of
/// scope for this function and must be stripped upstream before calling.
///
/// # FAILING STATE (pre-implementation)
///
/// These tests compile-fail because `ccag::translate::canonicalize` does not yet
/// exist. The compile failure IS the correct pre-green state for the TDD cycle.
///
/// To observe the compile failure:
/// ```
/// SQLX_OFFLINE=true cargo check --workspace --all-targets --features integration
/// ```
/// (The integration test binary is gated behind `#![cfg(feature = "integration")]`
/// in `tests/integration_tests.rs`, so the plain `--all-targets` invocation skips
/// this file. The `--features integration` flag is required to see the error.)
///
/// Once the Builder implements `src/translate/canonicalize.rs` and registers it in
/// `src/translate/mod.rs`, all tests in this file must pass.
use ccag::translate::canonicalize::canonicalize_model_id;

// ---------------------------------------------------------------------------
// AC2.1 — Test corpus (every row from spec Section 2 table)
// ---------------------------------------------------------------------------

/// Tests every row in the spec Section 2 test corpus table.
///
/// One loop iteration per row; on failure the assertion message identifies
/// the exact row (index + input) so the Builder knows which case is broken.
///
/// NOTE on `"claude-sonnet-4-6[1m]"` (last row, index 15):
/// The spec's table shows `Some("claude-sonnet-4-6")` as the EFFECTIVE output
/// *after* the upstream bracket-strip in `anthropic_to_bedrock`. However, the
/// spec also states: "Reject inputs containing characters outside
/// `[a-zA-Z0-9._:-]`." The `[` character is outside that allowed set.
/// Therefore, `canonicalize_model_id` called directly with `"claude-sonnet-4-6[1m]"`
/// must return `None`. The corpus row's `Some(...)` describes end-to-end behavior
/// (bracket stripped before this function is called), not this function's direct
/// output. This test asserts `None` for that exact input and documents why.
#[test]
fn test_ac2_1_corpus() {
    let cases: &[(&str, Option<&str>)] = &[
        // Input already canonical — passes through unchanged
        ("claude-sonnet-4-6", Some("claude-sonnet-4-6")),
        // Date-strip
        ("claude-sonnet-4-6-20250514", Some("claude-sonnet-4-6")),
        // Already canonical
        ("claude-opus-4-7", Some("claude-opus-4-7")),
        // Bare major (no minor) — still a valid canonical shape
        ("claude-opus-4", Some("claude-opus-4")),
        // Date-strip on haiku variant
        ("claude-haiku-4-5-20251001", Some("claude-haiku-4-5")),
        // Auto-prefix: "opus-4-6" → "claude-opus-4-6"
        ("opus-4-6", Some("claude-opus-4-6")),
        // Auto-prefix + date-strip
        ("opus-4-6-20250605", Some("claude-opus-4-6")),
        // Auto-prefix: "sonnet-4-6" → "claude-sonnet-4-6"
        ("sonnet-4-6", Some("claude-sonnet-4-6")),
        // Trim leading/trailing whitespace
        ("  claude-sonnet-4-6  ", Some("claude-sonnet-4-6")),
        // Space and dot → hard-fail (must not match; admin alias needed)
        ("Sonnet 4.7", None),
        // Underscore-form → hard-fail (must not match; admin alias needed)
        ("claude_sonnet_4_6", None),
        // Auto-prefix on arbitrary token: caller's job to check membership
        ("made-up-model", Some("claude-made-up-model")),
        // Empty input → None
        ("", None),
        // Empty family after "claude-" → None
        ("claude-", None),
        // Beta-suffix variant that is NOT a YYYYMMDD — passes through unchanged
        ("claude-sonnet-4-6-1m", Some("claude-sonnet-4-6-1m")),
        // Bracket suffix: '[' is outside [a-zA-Z0-9._:-], so canonicalizer
        // sees an invalid character and returns None.
        // (The spec table's Some("claude-sonnet-4-6") represents the effective
        // behavior AFTER anthropic_to_bedrock strips the bracket — this function
        // itself rejects the raw bracket form.)
        ("claude-sonnet-4-6[1m]", None),
    ];

    for (idx, (input, expected)) in cases.iter().enumerate() {
        let actual = canonicalize_model_id(input);
        let expected_owned: Option<String> = expected.map(|s| s.to_string());
        assert_eq!(
            actual, expected_owned,
            "row {idx}: input {:?} — expected {:?}, got {:?}",
            input, expected, actual,
        );
    }
}

// ---------------------------------------------------------------------------
// AC2.3 — Future-dated variant is date-stripped deterministically
// ---------------------------------------------------------------------------

/// Proves that date-stripping is shape-based (8 digit suffix), not list-based.
/// A date that hasn't been released yet (`20260101`) must strip identically to
/// a known past date, demonstrating Pass 1/2 forward-compatibility.
#[test]
fn test_ac2_3_future_dated_variant() {
    let result = canonicalize_model_id("claude-sonnet-4-6-20260101");
    assert_eq!(
        result,
        Some("claude-sonnet-4-6".to_string()),
        "future-dated variant 'claude-sonnet-4-6-20260101' should strip to 'claude-sonnet-4-6', got {:?}",
        result,
    );
}

// ---------------------------------------------------------------------------
// AC2.2 — Already-canonical input returns unchanged
// ---------------------------------------------------------------------------

/// Verifies that an already-canonical model ID passes through without
/// modification.
///
/// NOTE: AC2.2 also specifies "no allocations beyond the returned String".
/// This property cannot be directly asserted in a `#[test]` without
/// instrumented allocators, so it is documented here as a design intent rather
/// than a programmatic assertion. The Builder must ensure the implementation
/// avoids any intermediate allocations for the already-canonical code path
/// (e.g. avoid cloning or formatting strings unnecessarily before the
/// character-validation pass).
#[test]
fn test_ac2_2_canonical_input_returns_unchanged() {
    let input = "claude-sonnet-4-6";
    let result = canonicalize_model_id(input);
    assert_eq!(
        result,
        Some("claude-sonnet-4-6".to_string()),
        "canonical input {:?} should be returned unchanged as Some({:?}), got {:?}",
        input,
        input,
        result,
    );
}
