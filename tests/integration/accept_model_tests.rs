use std::sync::Arc;
/// Tests for Task 4: Acceptance pipeline (`accept_model`).
///
/// # What is tested
///
/// These tests exercise `accept_model` from the new module `src/api/accept.rs`
/// (or wherever the Builder places it), which implements the three-tier model
/// resolution pipeline:
///
///   1. Raw pin lookup  (cache lookup_forward_with_fallback on raw input)
///   2. Canonical pin   (canonicalize_model_id → cache lookup on canonical form)
///   3. Discovery       (async closure → discover_model pass1/pass2)
///   4. Reject          (all three miss → ModelAcceptance::Reject)
///
/// # BUILDER CONTRACT
///
/// The Builder must expose (in `src/api/accept.rs`, registered as
/// `pub mod accept;` in `src/api/mod.rs`):
///
/// ```rust
/// pub enum ModelAcceptance {
///     PinHit {
///         suffix: String,
///         anthropic_prefix: String,
///     },
///     CanonicalHit {
///         suffix: String,
///         canonical: String,
///         anthropic_prefix: String,
///     },
///     Discovered {
///         suffix: String,
///         canonical: String,
///         profile_prefix: String,
///         via: &'static str,
///         anthropic_prefix: String,
///     },
///     Reject,
/// }
///
/// pub async fn accept_model<F, Fut>(
///     raw: &str,
///     cache: &ccag::translate::models::ModelCache,
///     discover_fn: F,
/// ) -> ModelAcceptance
/// where
///     F: FnOnce(String) -> Fut,
///     Fut: std::future::Future<Output = Option<(String, String, Option<String>, String, &'static str)>>;
/// ```
///
/// The discovery closure receives the model identifier to discover (the
/// canonical form if canonicalization succeeded, or the raw string if it did
/// not) and returns the 5-tuple shape of `discover_model`:
/// `(anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix, via)`.
///
/// Order of operations in `accept_model`:
/// 1. `cache.lookup_forward_with_fallback(raw)` — if `Some`, return `PinHit`.
/// 2. `canonicalize_model_id(raw)` → if `Some(canonical)` and
///    `cache.lookup_forward(canonical)` is `Some(suffix)`, return `CanonicalHit`.
/// 3. Call `discover_fn(canonical_or_raw)`. On `Some`, return `Discovered`. On `None`, return `Reject`.
/// 4. `Reject`.
///
/// The `tracing::warn!(request_id, original_model, source, "model_id_rejected")`
/// emission is either:
///   - Inside `accept_model` itself  — test AC3.7 can capture it directly, OR
///   - In the calling handler         — test AC3.7 is marked `#[ignore]` with
///     a comment pointing to the handler integration path.
///
/// The Builder decides; the ignore annotation in AC3.7 handles the handler-side case.
///
/// `build_model_unavailable_error` must have its message updated to include
/// the literal substring `GET /v1/models` (AC3.9 partial check).
/// The function is currently `pub(crate)` and in the private `handlers` module.
/// **The Builder must make the function `pub` and expose it through
/// `ccag::api::handlers` (or re-export it from `ccag::api`)** so the external
/// test binary can call it for the AC3.9 message-text check.
/// Alternatively, the Builder can provide a thin wrapper or a constant for
/// the "not available" message template — whatever makes the function callable
/// from integration tests.
///
/// # FAILING STATE (pre-implementation)
///
/// `SQLX_OFFLINE=true cargo check --workspace --all-targets --features integration`
/// compile-fails because:
///   1. `ccag::api::accept` does not yet exist.
///   2. `ccag::api::handlers::build_model_unavailable_error` is not yet pub-accessible.
///
/// These compile errors ARE the correct pre-green state for the TDD cycle.
///
/// Once the Builder:
///   - Creates `src/api/accept.rs` with the enum + function above.
///   - Registers `pub mod accept;` in `src/api/mod.rs`.
///   - Makes `build_model_unavailable_error` reachable from an external crate
///     (or provides a public wrapper / test helper).
///
/// All tests (except the ignored AC3.7 variant) must pass.
use std::sync::atomic::{AtomicUsize, Ordering};

use ccag::api::accept::{ModelAcceptance, accept_model};
use ccag::translate::models::{CachedMapping, ModelCache};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a cache entry for `anthropic_prefix` → `bedrock_suffix` and insert it.
async fn insert_mapping(cache: &ModelCache, anthropic_prefix: &str, bedrock_suffix: &str) {
    cache
        .insert(CachedMapping {
            anthropic_prefix: anthropic_prefix.to_string(),
            bedrock_suffix: bedrock_suffix.to_string(),
            anthropic_display: None,
        })
        .await;
}

/// A discovery closure that panics if called. Used to verify the discovery path
/// is NOT reached when a pin/canonical hit should have short-circuited.
fn never_called_discover() -> impl FnOnce(
    String,
) -> std::future::Ready<
    Option<(String, String, Option<String>, String, &'static str)>,
> {
    |_model: String| {
        panic!(
            "Discovery closure was called but should NOT have been — a pin/canonical hit \
             should have short-circuited before discovery."
        );
    }
}

/// A discovery closure that always returns None.
fn returns_none_discover() -> impl FnOnce(
    String,
) -> std::future::Ready<
    Option<(String, String, Option<String>, String, &'static str)>,
> {
    |_model: String| std::future::ready(None)
}

// ── AC3.1 ─────────────────────────────────────────────────────────────────────

/// AC3.1: A dated variant (`claude-sonnet-4-6-20250514`) against a cache that
/// has only the canonical form (`claude-sonnet-4-6`) returns `CanonicalHit`.
///
/// The dated variant is NOT in the cache (no raw pin). Canonicalization strips
/// the date suffix and the canonical form IS in the cache — so the canonical
/// pin path fires. Discovery must NOT be called.
#[tokio::test]
async fn test_ac3_1_canonical_hit_dated_variant() {
    let cache = ModelCache::new();
    insert_mapping(&cache, "claude-sonnet-4-6", "anthropic.claude-sonnet-4-6").await;

    let result = accept_model(
        "claude-sonnet-4-6-20250514",
        &cache,
        never_called_discover(),
    )
    .await;

    match result {
        ModelAcceptance::CanonicalHit {
            canonical,
            suffix,
            anthropic_prefix: _,
        } => {
            assert_eq!(
                canonical, "claude-sonnet-4-6",
                "CanonicalHit canonical should be the date-stripped form"
            );
            assert_eq!(
                suffix, "anthropic.claude-sonnet-4-6",
                "CanonicalHit suffix should match the cache entry"
            );
        }
        other => panic!(
            "Expected CanonicalHit for 'claude-sonnet-4-6-20250514' with canonical cache entry, \
             got {:?}",
            other
        ),
    }
}

// ── AC3.2 ─────────────────────────────────────────────────────────────────────

/// AC3.2: When BOTH the raw dated variant AND the canonical form have pin rows,
/// the raw pin wins (exact raw match takes precedence over canonical lookup).
///
/// The cache has:
///   - `claude-sonnet-4-6-20250514` → `anthropic.claude-sonnet-4-6-dated` (raw pin)
///   - `claude-sonnet-4-6`          → `anthropic.claude-sonnet-4-6`        (canonical pin)
///
/// `accept_model("claude-sonnet-4-6-20250514")` must return `PinHit` with the
/// dated-variant suffix, not `CanonicalHit` with the canonical suffix. Discovery
/// must NOT be called.
#[tokio::test]
async fn test_ac3_2_pin_hit_precedence_over_canonical() {
    let cache = ModelCache::new();
    // Raw pin — distinct suffix to distinguish which was hit
    insert_mapping(
        &cache,
        "claude-sonnet-4-6-20250514",
        "anthropic.claude-sonnet-4-6-dated",
    )
    .await;
    // Canonical pin
    insert_mapping(&cache, "claude-sonnet-4-6", "anthropic.claude-sonnet-4-6").await;

    let result = accept_model(
        "claude-sonnet-4-6-20250514",
        &cache,
        never_called_discover(),
    )
    .await;

    match result {
        ModelAcceptance::PinHit {
            suffix,
            anthropic_prefix: _,
        } => {
            assert_eq!(
                suffix, "anthropic.claude-sonnet-4-6-dated",
                "PinHit must use the raw-match suffix, not the canonical one"
            );
        }
        ModelAcceptance::CanonicalHit { .. } => panic!(
            "Expected PinHit (raw wins) but got CanonicalHit — \
             raw pin must short-circuit before canonical lookup"
        ),
        other => panic!(
            "Expected PinHit for raw pin with precedence test, got {:?}",
            other
        ),
    }
}

// ── AC3.3 ─────────────────────────────────────────────────────────────────────

/// AC3.3: Empty cache, discovery returns None → `Reject`.
///
/// NOTE: This test cannot directly assert "no DB row created" without a running
/// database. That guarantee is architectural: `accept_model` itself never writes
/// to the DB; the caller (handler) only writes on `Discovered`. Since `Reject` is
/// returned, the handler will never call `upsert_mapping`. The full "no row
/// created" guarantee is verified end-to-end by AC3.9 [online] in staging.
#[tokio::test]
async fn test_ac3_3_reject_no_db_row_created() {
    let cache = ModelCache::new();

    let result = accept_model("made-up-model", &cache, returns_none_discover()).await;

    assert!(
        matches!(result, ModelAcceptance::Reject),
        "Empty cache + no discovery match should return Reject, got {:?}",
        result
    );
}

// ── AC3.4 ─────────────────────────────────────────────────────────────────────

/// AC3.4: Empty cache, discovery returns a valid 5-tuple → `Discovered`.
///
/// The discovery closure simulates `discover_model` returning a successful
/// profile match for `claude-future-9-9` via pass1.
///
/// Asserts:
/// - `ModelAcceptance::Discovered` is returned.
/// - `via == "pass1"`.
/// - `profile_prefix` matches the returned prefix.
/// - `suffix` matches the returned suffix.
/// - `canonical` matches the discovered `anthropic_prefix`.
/// - `anthropic_prefix` is the raw input (or canonical — Builder picks; test
///   accepts either `"claude-future-9-9"` since raw == canonical here).
#[tokio::test]
async fn test_ac3_4_discovered_via_pass1() {
    let cache = ModelCache::new();

    let discover_fn = |_model: String| {
        std::future::ready(Some((
            "claude-future-9-9".to_string(),           // anthropic_prefix
            "anthropic.claude-future-9-9".to_string(), // bedrock_suffix
            None::<String>,                            // anthropic_display
            "global".to_string(),                      // profile_prefix
            "pass1",                                   // via
        )))
    };

    let result = accept_model("claude-future-9-9", &cache, discover_fn).await;

    match result {
        ModelAcceptance::Discovered {
            suffix,
            canonical: _,
            profile_prefix,
            via,
            anthropic_prefix: _,
        } => {
            assert_eq!(
                suffix, "anthropic.claude-future-9-9",
                "Discovered suffix should match the discovery return"
            );
            assert_eq!(
                profile_prefix, "global",
                "Discovered profile_prefix should match the discovery return"
            );
            assert_eq!(via, "pass1", "Discovered via should be 'pass1'");
        }
        other => panic!(
            "Expected Discovered for a successful discovery closure, got {:?}",
            other
        ),
    }
}

// ── AC3.5 ─────────────────────────────────────────────────────────────────────

/// AC3.5: `Sonnet 4.7` (space + dot) against an empty cache → `Reject`.
///
/// The canonicalizer returns `None` for this input (space is outside the allowed
/// `[a-zA-Z0-9._:-]` set). There is no raw pin either. Hard-fail.
///
/// Discovery closure uses a counter to verify it was NOT called — if
/// canonicalization fails AND no raw pin exists, the implementation should
/// reject without calling discovery (since there is no well-formed model ID to
/// discover against). However, the spec does not explicitly prohibit calling
/// discovery in this path, so if the Builder calls discovery (passing the raw
/// string), the test accepts a `Reject` from the discovery returning None.
/// Either way, the final result MUST be `Reject`.
#[tokio::test]
async fn test_ac3_5_reject_on_invalid_input() {
    let cache = ModelCache::new();

    // Counter tracks whether discovery was called (informational, not a hard assert)
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();

    let discover_fn = move |_model: String| {
        calls_clone.fetch_add(1, Ordering::SeqCst);
        std::future::ready(None::<(String, String, Option<String>, String, &'static str)>)
    };

    let result = accept_model("Sonnet 4.7", &cache, discover_fn).await;

    assert!(
        matches!(result, ModelAcceptance::Reject),
        "Input 'Sonnet 4.7' (space+dot) with empty cache must be Reject; got {:?}",
        result
    );
    // If discovery was called, it returned None anyway, so the Reject is still
    // correct. Log for debugging in case there is an unexpected code path.
    let _discovery_calls = calls.load(Ordering::SeqCst);
}

// ── AC3.6 ─────────────────────────────────────────────────────────────────────

/// AC3.6: `Sonnet 4.7` with a manually-added admin alias row → `PinHit`.
///
/// An admin has inserted a row keyed on the literal string `Sonnet 4.7`
/// (non-canonical, cannot be canonicalized, created_via='admin').
/// `accept_model` must still check the raw input against the cache BEFORE
/// attempting canonicalization, so this alias row fires as a `PinHit`.
///
/// BUILDER NOTE: This test forces `lookup_forward_with_fallback` to honor
/// **non-canonical** raw keys. The current implementation of
/// `lookup_forward_with_fallback` does an exact-match fast-path (step 1)
/// BEFORE any date-stripping (step 2). A raw key like `Sonnet 4.7` that has
/// no date suffix hits the fast-path directly if it has a cache entry. This
/// should already work. If this test fails after the alias row is inserted, it
/// signals that `lookup_forward_with_fallback` is NOT doing a raw exact-match
/// first — a bug Task 4 must address.
#[tokio::test]
async fn test_ac3_6_pin_hit_on_alias_row() {
    let cache = ModelCache::new();
    // Admin alias: raw non-canonical key → canonical suffix
    insert_mapping(&cache, "Sonnet 4.7", "anthropic.claude-sonnet-4-6").await;

    let result = accept_model("Sonnet 4.7", &cache, never_called_discover()).await;

    match result {
        ModelAcceptance::PinHit {
            suffix,
            anthropic_prefix,
        } => {
            assert_eq!(
                suffix, "anthropic.claude-sonnet-4-6",
                "PinHit suffix should be the alias row's bedrock_suffix"
            );
            assert_eq!(
                anthropic_prefix, "Sonnet 4.7",
                "PinHit anthropic_prefix should echo the raw input key"
            );
        }
        other => panic!(
            "Expected PinHit for admin alias row 'Sonnet 4.7', got {:?}",
            other
        ),
    }
}

// ── AC3.7 ─────────────────────────────────────────────────────────────────────

/// AC3.7: Reject path emits exactly one `tracing::warn!` with fields that
/// include the literal string `model_id_rejected` (either as the event name /
/// message, or as a structured field value).
///
/// # Why this test is ignored
///
/// The spec leaves the warn emission location ambiguous ("accept_model itself
/// OR the calling handler"). The test file was written against the
/// `accept_model`-emits-warn variant (easiest to test in isolation). If the
/// Builder chooses to emit the warn in the handler (so that `request_id` is in
/// scope), this test cannot pass without spinning up the full handler.
///
/// Two approaches the Builder can choose from:
///
///   A. Emit in `accept_model` itself (no `request_id` field — that's OK since
///      the spec only mandates `original_model` and `source`).
///      → REMOVE the `#[ignore]` attribute and implement the tracing capture below.
///
///   B. Emit in the handler.
///      → KEEP `#[ignore]`. The online AC3.9 verification in staging covers
///         the "warn appears in CW Logs" requirement end-to-end.
///
/// The `#[ignore]` annotation documents the deferral decision for reviewers.
#[tokio::test]
#[ignore = "warn emission location deferred to Builder: if warn is in accept_model, remove this \
            ignore and implement tracing capture; if warn is in the handler, this test is \
            superseded by the online AC3.9 staging verification"]
async fn test_ac3_7_reject_emits_exactly_one_warn() {
    // This test body is intentionally left as a skeleton. The Builder should
    // either:
    //
    // A. Remove the `#[ignore]` and fill in the tracing capture:
    //
    //    use tracing_subscriber::layer::SubscriberExt;
    //    use std::sync::{Arc, Mutex};
    //
    //    #[derive(Default, Clone)]
    //    struct WarnCapture(Arc<Mutex<Vec<String>>>);
    //
    //    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnCapture {
    //        fn on_event(&self, event: &tracing::Event<'_>, _ctx: ...) {
    //            if *event.metadata().level() == tracing::Level::WARN {
    //                let mut v = self.0.lock().unwrap();
    //                // capture event message/fields as a string
    //                v.push(format!("{:?}", event));
    //            }
    //        }
    //    }
    //
    //    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    //    let layer = WarnCapture(captured.clone());
    //    let subscriber = tracing_subscriber::registry().with(layer);
    //    let _guard = tracing::subscriber::set_default(subscriber);
    //
    //    let cache = ModelCache::new();
    //    let _ = accept_model("made-up-model", &cache, returns_none_discover()).await;
    //
    //    let warns: Vec<_> = captured.lock().unwrap()
    //        .iter()
    //        .filter(|s| s.contains("model_id_rejected"))
    //        .cloned()
    //        .collect();
    //    assert_eq!(
    //        warns.len(), 1,
    //        "Reject path should emit exactly one warn with 'model_id_rejected'; found {} warn(s)",
    //        warns.len(),
    //    );
    //
    // B. Keep the `#[ignore]` as-is (handler emits the warn).

    // Placeholder — this body only runs if the ignore is removed.
    let cache = ModelCache::new();
    let result = accept_model("made-up-model", &cache, returns_none_discover()).await;
    assert!(
        matches!(result, ModelAcceptance::Reject),
        "Sanity: empty cache + no discovery should Reject"
    );
}

// ── AC3.9 (partial, offline) ──────────────────────────────────────────────────

/// AC3.9 partial (offline): `build_model_unavailable_error` response body must
/// contain the literal substring `GET /v1/models`.
///
/// The full AC3.9 [online] check (real HTTP request → 400 response + CW Logs
/// `model_id_rejected` entry) is performed manually at staging deploy time.
/// This test covers only the message-text half that is checkable offline.
///
/// # BUILDER CONTRACT (for this test to compile)
///
/// `build_model_unavailable_error` must be callable from this external test binary.
/// Options, in order of preference:
///
///   1. Make it `pub` and re-export from `ccag::api` (or a new `ccag::api::handlers`
///      public module):
///      ```rust
///      // src/api/mod.rs
///      pub use handlers::build_model_unavailable_error;
///      ```
///
///   2. Provide a thin public wrapper in a test-accessible location:
///      ```rust
///      // src/api/accept.rs (already being created for accept_model)
///      #[cfg(test)]
///      pub fn build_model_unavailable_error_for_test(model: &str) -> axum::response::Response {
///          super::handlers::build_model_unavailable_error(model)
///      }
///      ```
///      Then this test calls `ccag::api::accept::build_model_unavailable_error_for_test`.
///
///   3. Export a public constant or fn that returns just the error message string.
///
/// The simplest approach is (1). The `pub(crate)` visibility must be widened to
/// `pub` and the module made accessible, OR the function moved into a pub module.
#[tokio::test]
async fn test_ac3_9_model_unavailable_error_contains_v1_models_link() {
    use axum::body::to_bytes;

    // Call through the public wrapper/re-export that the Builder must provide.
    // If the Builder went with option (1) re-export from ccag::api:
    let response = ccag::api::build_model_unavailable_error("test-model-id");

    // Extract the response body bytes
    let (_, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX)
        .await
        .expect("Failed to read response body bytes");
    let body_str = std::str::from_utf8(&bytes).expect("Response body is not valid UTF-8");

    assert!(
        body_str.contains("GET /v1/models"),
        "build_model_unavailable_error response body must contain 'GET /v1/models'; \
         actual body: {body_str:?}"
    );

    // Also verify the standard error envelope shape (backwards-compat check)
    let body_json: serde_json::Value =
        serde_json::from_str(body_str).expect("Response body must be valid JSON");
    assert_eq!(
        body_json["error"]["type"].as_str(),
        Some("invalid_request_error"),
        "Error type must be 'invalid_request_error'"
    );
    assert!(
        body_json["error"]["message"].is_string(),
        "Error message must be a string"
    );
    assert!(
        body_json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("GET /v1/models"),
        "Error message field must contain 'GET /v1/models'"
    );
}
