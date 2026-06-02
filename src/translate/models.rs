use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

/// A single cached model mapping entry.
#[derive(Debug, Clone)]
pub struct CachedMapping {
    pub anthropic_prefix: String,
    pub bedrock_suffix: String,
    pub anthropic_display: Option<String>,
}

/// In-memory model mapping cache. Uses exact-match lookup by anthropic_prefix
/// (the column name is retained for DB compatibility but semantics changed from
/// "prefix to match" to "exact key").
#[derive(Clone)]
pub struct ModelCache {
    inner: Arc<RwLock<HashMap<String, CachedMapping>>>,
}

impl Default for ModelCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load all mappings from DB into cache (replaces existing entries).
    pub async fn load_from_db(&self, pool: &sqlx::PgPool) -> Result<usize, sqlx::Error> {
        let rows = crate::db::model_mappings::get_all_mappings(pool).await?;
        let count = rows.len();
        let mut map = HashMap::new();
        for r in rows {
            let key = r.anthropic_prefix.clone();
            let mapping = CachedMapping {
                anthropic_prefix: r.anthropic_prefix,
                bedrock_suffix: r.bedrock_suffix,
                anthropic_display: r.anthropic_display,
            };
            map.insert(key, mapping);
        }
        *self.inner.write().await = map;
        Ok(count)
    }

    /// Forward lookup: anthropic model ID -> bedrock suffix.
    /// Uses exact-match lookup against the cache. No prefix matching.
    /// Uses `try_read()` to avoid blocking; returns None on contention.
    pub fn lookup_forward(&self, anthropic_model: &str) -> Option<String> {
        let guard = self.inner.try_read().ok()?;
        guard.get(anthropic_model).map(|m| m.bedrock_suffix.clone())
    }

    /// Reverse lookup: bedrock model ID -> anthropic display name.
    /// First tries an exact-match fast path against known bedrock_suffix values,
    /// then falls back to the existing `contains` scan (for rows where the suffix
    /// doesn't appear verbatim in the profile ID).
    /// Uses `try_read()` to avoid blocking; returns None on contention.
    pub fn lookup_reverse(&self, bedrock_model: &str) -> Option<String> {
        let guard = self.inner.try_read().ok()?;

        // Fast path: exact match against bedrock_suffix values (zero-allocation)
        for m in guard.values() {
            let suffix = &m.bedrock_suffix;
            let dotted_match = bedrock_model.len() > suffix.len()
                && bedrock_model.ends_with(suffix.as_str())
                && bedrock_model.as_bytes()[bedrock_model.len() - suffix.len() - 1] == b'.';
            if bedrock_model == suffix.as_str() || dotted_match {
                return m.anthropic_display.clone();
            }
        }

        // Fallback: contains scan (existing behaviour)
        for m in guard.values() {
            if bedrock_model.contains(&m.bedrock_suffix)
                || bedrock_model.contains(&m.anthropic_prefix)
            {
                return m.anthropic_display.clone();
            }
        }
        None
    }

    /// Insert a mapping into the cache (upsert by anthropic_prefix).
    pub async fn insert(&self, mapping: CachedMapping) {
        let mut guard = self.inner.write().await;
        guard.insert(mapping.anthropic_prefix.clone(), mapping);
    }
}

/// Strip a YYYYMMDD date suffix from a model ID.
/// e.g. "claude-sonnet-5-0-20260601" -> "claude-sonnet-5-0"
pub fn strip_date_suffix(model: &str) -> &str {
    // Look for pattern: -{8 digits} at the end
    if model.len() >= 9 {
        let candidate = &model[model.len() - 9..];
        if candidate.starts_with('-') && candidate[1..].chars().all(|c| c.is_ascii_digit()) {
            return &model[..model.len() - 9];
        }
    }
    model
}

/// Discover a model by calling Bedrock ListInferenceProfiles and fuzzy-matching.
/// Returns (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix) if found.
pub async fn discover_model(
    bedrock_client: &aws_sdk_bedrock::Client,
    anthropic_model: &str,
    _prefix: &str,
) -> Option<(String, String, Option<String>, String)> {
    let stripped = strip_date_suffix(anthropic_model);
    tracing::info!(
        model = %anthropic_model,
        stripped = %stripped,
        "Discovering unknown model via ListInferenceProfiles"
    );

    // List all inference profiles — paginate to get all
    let mut profiles = Vec::new();
    let mut next_token: Option<String> = None;
    loop {
        let mut req = bedrock_client.list_inference_profiles();
        if let Some(token) = next_token.take() {
            req = req.next_token(token);
        }
        match req.send().await {
            Ok(output) => {
                if let Some(summaries) = output.inference_profile_summaries {
                    profiles.extend(summaries);
                }
                next_token = output.next_token;
                if next_token.is_none() {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(%e, "ListInferenceProfiles failed");
                return None;
            }
        }
    }

    tracing::debug!(count = profiles.len(), "Listed inference profiles");

    // Find a profile whose ID contains the stripped prefix
    // e.g. stripped="claude-sonnet-5-0" matches "us.anthropic.claude-sonnet-5-0-v1"
    for profile in &profiles {
        let profile_id = profile.inference_profile_id();
        if profile_id.contains(stripped) {
            let profile_id = profile_id.to_string();

            // Extract the profile prefix (region): everything before the first '.'
            // e.g. "global.anthropic.claude-opus-4-7" -> "global"
            let profile_prefix = profile_id
                .find('.')
                .map(|i| &profile_id[..i])
                .unwrap_or(&profile_id)
                .to_string();

            // Extract the bedrock_suffix: everything after the first '.'
            // e.g. "us.anthropic.claude-sonnet-5-0-v1" -> "anthropic.claude-sonnet-5-0-v1"
            let bedrock_suffix = profile_id
                .find('.')
                .map(|i| &profile_id[i + 1..])
                .unwrap_or(&profile_id)
                .to_string();

            // anthropic_prefix is the stripped model name (without date)
            let anthropic_prefix = stripped.to_string();

            // anthropic_display is the full model ID CC sent (with date)
            let anthropic_display = if anthropic_model != stripped {
                Some(anthropic_model.to_string())
            } else {
                None
            };

            tracing::info!(
                anthropic_prefix = %anthropic_prefix,
                bedrock_suffix = %bedrock_suffix,
                profile_id = %profile_id,
                "Discovered new model mapping"
            );

            return Some((
                anthropic_prefix,
                bedrock_suffix,
                anthropic_display,
                profile_prefix,
            ));
        }
    }

    tracing::warn!(
        model = %anthropic_model,
        stripped = %stripped,
        "No matching inference profile found"
    );
    None
}

/// Map Anthropic model IDs to Bedrock inference profile IDs.
///
/// CC sends Anthropic-format model IDs. We map these to Bedrock inference
/// profile IDs using the configured region prefix.
///
/// Checks the dynamic cache first, falls back to hardcoded mappings.
pub fn anthropic_to_bedrock(model: &str, prefix: &str, model_cache: Option<&ModelCache>) -> String {
    // If it already looks like a Bedrock ID (contains a dot prefix), pass through
    if model.contains('.') {
        return model.to_string();
    }

    // Try dynamic cache first (non-blocking)
    if let Some(cache) = model_cache
        && let Some(suffix) = cache.lookup_forward(model)
    {
        return format!("{prefix}.{suffix}");
    }

    // Fall back to hardcoded mappings
    hardcoded_anthropic_to_bedrock(model, prefix)
}

/// Hardcoded forward mapping (no-DB fallback).
fn hardcoded_anthropic_to_bedrock(model: &str, prefix: &str) -> String {
    match model {
        "claude-opus-4-7" => format!("{prefix}.anthropic.claude-opus-4-7"),
        "claude-opus-4-6" | "claude-opus-4-6-20250605" => {
            format!("{prefix}.anthropic.claude-opus-4-6-v1")
        }
        "claude-sonnet-4-6" | "claude-sonnet-4-6-20250514" => {
            format!("{prefix}.anthropic.claude-sonnet-4-6")
        }
        "claude-opus-4-5" | "claude-opus-4-5-20251101" => {
            format!("{prefix}.anthropic.claude-opus-4-5-20251101-v1:0")
        }
        "claude-sonnet-4-5" | "claude-sonnet-4-5-20250929" => {
            format!("{prefix}.anthropic.claude-sonnet-4-5-20250929-v1:0")
        }
        "claude-sonnet-4" | "claude-sonnet-4-20250514" => {
            format!("{prefix}.anthropic.claude-sonnet-4-20250514-v1:0")
        }
        "claude-haiku-4-5" | "claude-haiku-4-5-20251001" => {
            format!("{prefix}.anthropic.claude-haiku-4-5-20251001-v1:0")
        }
        // Unknown / future variants (e.g. claude-sonnet-4-8): pass through dotless
        // so the discovery-on-miss path in handlers.rs resolves them via Bedrock.
        other => other.to_string(),
    }
}

/// Map Bedrock model IDs back to Anthropic-format IDs for responses.
///
/// Checks the dynamic cache first, falls back to hardcoded mappings.
pub fn bedrock_to_anthropic(model: &str, model_cache: Option<&ModelCache>) -> String {
    // Try dynamic cache first (non-blocking)
    if let Some(cache) = model_cache
        && let Some(display) = cache.lookup_reverse(model)
    {
        return display;
    }

    // Fall back to hardcoded mappings
    hardcoded_bedrock_to_anthropic(model)
}

/// Hardcoded reverse mapping (no-DB fallback).
fn hardcoded_bedrock_to_anthropic(model: &str) -> String {
    match model {
        s if s.contains("claude-opus-4-7") => "claude-opus-4-7".to_string(),
        s if s.contains("claude-opus-4-6") => "claude-opus-4-6-20250605".to_string(),
        s if s.contains("claude-sonnet-4-6") => "claude-sonnet-4-6-20250514".to_string(),
        s if s.contains("claude-opus-4-5") => "claude-opus-4-5-20251101".to_string(),
        s if s.contains("claude-sonnet-4-5") => "claude-sonnet-4-5-20250929".to_string(),
        s if s.contains("claude-sonnet-4-2025") => "claude-sonnet-4-20250514".to_string(),
        s if s.contains("claude-haiku-4-5") => "claude-haiku-4-5-20251001".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Forward mapping tests (cache path + hardcoded fallback) ---

    #[test]
    fn test_model_mapping_us() {
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6-20250514", "us", None),
            "us.anthropic.claude-sonnet-4-6"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-6-20250605", "us", None),
            "us.anthropic.claude-opus-4-6-v1"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-7", "us", None),
            "us.anthropic.claude-opus-4-7"
        );
    }

    #[test]
    fn test_model_mapping_au() {
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6-20250514", "au", None),
            "au.anthropic.claude-sonnet-4-6"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-6-20250605", "au", None),
            "au.anthropic.claude-opus-4-6-v1"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-7", "au", None),
            "au.anthropic.claude-opus-4-7"
        );
    }

    #[test]
    fn test_all_hardcoded_mappings() {
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-7", "us", None),
            "us.anthropic.claude-opus-4-7"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-6-20250605", "us", None),
            "us.anthropic.claude-opus-4-6-v1"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6-20250514", "us", None),
            "us.anthropic.claude-sonnet-4-6"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-5-20250929", "us", None),
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-20250514", "us", None),
            "us.anthropic.claude-sonnet-4-20250514-v1:0"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-haiku-4-5-20251001", "us", None),
            "us.anthropic.claude-haiku-4-5-20251001-v1:0"
        );
    }

    #[test]
    fn test_passthrough_bedrock_id() {
        assert_eq!(
            anthropic_to_bedrock("au.anthropic.claude-sonnet-4-6", "us", None),
            "au.anthropic.claude-sonnet-4-6"
        );
    }

    #[test]
    fn test_reverse_mapping() {
        assert_eq!(
            bedrock_to_anthropic("au.anthropic.claude-sonnet-4-6", None),
            "claude-sonnet-4-6-20250514"
        );
        assert_eq!(
            bedrock_to_anthropic("global.anthropic.claude-opus-4-7", None),
            "claude-opus-4-7"
        );
    }

    #[test]
    fn test_cache_miss_fallback() {
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6-20250514", "us", None),
            "us.anthropic.claude-sonnet-4-6"
        );
        assert_eq!(
            bedrock_to_anthropic("us.anthropic.claude-sonnet-4-6", None),
            "claude-sonnet-4-6-20250514"
        );
    }

    // --- ModelCache tests ---
    // #[cfg(test)] - all tests below are within the test module

    /// Validates exact-match lookup semantics: a cache row keyed by one variant
    /// will ONLY match an identical lookup key, not similar model IDs.
    #[tokio::test]
    async fn test_cache_forward_lookup() {
        let cache = ModelCache::new();
        // Insert a bare-stem model ID as the key
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-6".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-6".to_string(),
                anthropic_display: Some("claude-sonnet-4-6-20250514".to_string()),
            })
            .await;

        // Exact match: lookup with the same bare-stem key succeeds
        assert_eq!(
            cache.lookup_forward("claude-sonnet-4-6"),
            Some("anthropic.claude-sonnet-4-6".to_string()),
            "Exact-match lookup with bare-stem key should succeed"
        );

        // Non-exact match: lookup with dated variant MUST fail under exact-match semantics
        assert_eq!(
            cache.lookup_forward("claude-sonnet-4-6-20250514"),
            None,
            "Exact-match lookup must NOT match dated variant against bare-stem key"
        );
    }

    #[tokio::test]
    async fn test_cache_reverse_lookup() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-6".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-6".to_string(),
                anthropic_display: Some("claude-sonnet-4-6-20250514".to_string()),
            })
            .await;

        assert_eq!(
            cache.lookup_reverse("us.anthropic.claude-sonnet-4-6"),
            Some("claude-sonnet-4-6-20250514".to_string())
        );
    }

    // #[cfg(test)] - test_cache_prefix_ordering deleted (obsolete under exact-match semantics)

    /// Validates the integration of ModelCache exact-match semantics with anthropic_to_bedrock.
    /// Cache hits require exact key match; cache misses fall back to hardcoded mappings.
    #[tokio::test]
    async fn test_cache_with_anthropic_to_bedrock() {
        let cache = ModelCache::new();
        // Insert a dated model ID as the cache key
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-future-5-0-20260601".to_string(),
                bedrock_suffix: "anthropic.claude-future-5-0-v1".to_string(),
                anthropic_display: Some("claude-future-5-0-20260601".to_string()),
            })
            .await;

        // Cache hit path: exact-match lookup succeeds when key matches exactly
        assert_eq!(
            anthropic_to_bedrock("claude-future-5-0-20260601", "us", Some(&cache)),
            "us.anthropic.claude-future-5-0-v1",
            "Cache hit with exact key match should return cached suffix"
        );

        // Cache miss path: non-exact lookup falls back to hardcoded mapping
        // (bare-stem "claude-future-5-0" doesn't match dated key "claude-future-5-0-20260601")
        // Since there's no hardcoded mapping for claude-future-5-0, it passes through as-is
        assert_eq!(
            anthropic_to_bedrock("claude-future-5-0", "us", Some(&cache)),
            "claude-future-5-0",
            "Cache miss (non-exact key) with no hardcoded mapping should pass through"
        );

        // Cache miss path: known model falls back to hardcoded mapping
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6-20250514", "us", Some(&cache)),
            "us.anthropic.claude-sonnet-4-6",
            "Cache miss (model not in cache) should fall back to hardcoded mapping"
        );
    }

    // --- strip_date_suffix tests ---

    #[test]
    fn test_strip_date_suffix() {
        assert_eq!(
            strip_date_suffix("claude-sonnet-5-0-20260601"),
            "claude-sonnet-5-0"
        );
        assert_eq!(
            strip_date_suffix("claude-opus-4-6-20250605"),
            "claude-opus-4-6"
        );
        assert_eq!(
            strip_date_suffix("claude-sonnet-4-6-20250514"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            strip_date_suffix("claude-sonnet-4-5-20250929"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            strip_date_suffix("claude-sonnet-4-20250514"),
            "claude-sonnet-4"
        );
        assert_eq!(
            strip_date_suffix("claude-haiku-4-5-20251001"),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn test_strip_date_suffix_no_date() {
        assert_eq!(strip_date_suffix("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(strip_date_suffix("claude-opus"), "claude-opus");
    }

    #[test]
    fn test_strip_date_suffix_short_string() {
        assert_eq!(strip_date_suffix("short"), "short");
        assert_eq!(strip_date_suffix(""), "");
        assert_eq!(strip_date_suffix("12345678"), "12345678");
    }

    // --- Slice 1: AC1b - Exact-match hardcoded fallback (no greedy prefix) ---

    /// AC1b: The hardcoded fallback MUST NOT use greedy prefix matching.
    /// Future model IDs like 'claude-sonnet-4-8' MUST NOT match the greedy
    /// 'starts_with("claude-sonnet-4-")' catch-all that routes to retired Sonnet 4.0.
    /// This test MUST FAIL on the current code (line 247-249) to prove the bug.
    #[test]
    fn test_hardcoded_fallback_no_greedy_prefix() {
        // AC1b guard: Future variant must NOT route to retired Sonnet 4.0 profile
        let result = anthropic_to_bedrock("claude-sonnet-4-8", "us", None);
        assert_ne!(
            result, "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "GREEDY BUG: 'claude-sonnet-4-8' matched catch-all and routed to RETIRED Sonnet 4.0"
        );
        // Expected: dotless pass-through so discovery can fire
        assert_eq!(
            result, "claude-sonnet-4-8",
            "Unknown future variant must pass through dotless for discovery"
        );
    }

    /// AC1b regression safety: Canonical models still resolve after switching to exact-match.
    /// These assertions should PASS before and after the builder's fix.
    #[test]
    fn test_hardcoded_fallback_canonical_mappings_survive() {
        // Bare stem forms
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6", "us", None),
            "us.anthropic.claude-sonnet-4-6",
            "Bare stem 'claude-sonnet-4-6' must resolve"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-7", "us", None),
            "us.anthropic.claude-opus-4-7",
            "Bare stem 'claude-opus-4-7' must resolve"
        );

        // Canonical dated forms
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6-20250514", "us", None),
            "us.anthropic.claude-sonnet-4-6",
            "Canonical dated 'claude-sonnet-4-6-20250514' must resolve"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-20250514", "us", None),
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "Canonical dated 'claude-sonnet-4-20250514' (retired Sonnet 4.0) must still resolve via EXACT match"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-opus-4-6-20250605", "us", None),
            "us.anthropic.claude-opus-4-6-v1",
            "Canonical dated 'claude-opus-4-6-20250605' must resolve"
        );
        assert_eq!(
            anthropic_to_bedrock("claude-haiku-4-5-20251001", "us", None),
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "Canonical dated 'claude-haiku-4-5-20251001' must resolve"
        );
    }

    // --- Slice 1: Exact-match cache lookup tests (#[cfg(test)]) ---

    /// AC1: Regression test for the greedy prefix bug.
    /// Given cache row ('claude-opus-4', ...), lookup of 'claude-opus-4-8' MUST return None.
    /// This test MUST FAIL on the current starts_with code (it would incorrectly match).
    #[tokio::test]
    async fn test_cache_exact_match_no_greedy_prefix() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-20250514-v1:0".to_string(),
                anthropic_display: Some("claude-opus-4-20250514".to_string()),
            })
            .await;

        // AC1: This MUST return None under exact-match semantics
        // (currently fails because starts_with matches 'claude-opus-4')
        assert_eq!(
            cache.lookup_forward("claude-opus-4-8"),
            None,
            "lookup_forward must NOT match 'claude-opus-4-8' against row keyed 'claude-opus-4'"
        );
    }

    /// AC2: Exact match returns the correct suffix.
    #[tokio::test]
    async fn test_cache_exact_match_found() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4-8".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-8".to_string(),
                anthropic_display: None,
            })
            .await;

        assert_eq!(
            cache.lookup_forward("claude-opus-4-8"),
            Some("anthropic.claude-opus-4-8".to_string())
        );
    }

    /// AC3: Exact match on a dated model ID returns the correct suffix.
    #[tokio::test]
    async fn test_cache_exact_match_dated_model() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4-20250514".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-20250514-v1:0".to_string(),
                anthropic_display: None,
            })
            .await;

        assert_eq!(
            cache.lookup_forward("claude-opus-4-20250514"),
            Some("anthropic.claude-opus-4-20250514-v1:0".to_string())
        );
    }

    /// AC4 (partial): Legacy rows are inert under exact-match.
    /// A legacy row keyed 'claude-sonnet-4-' is never matched by a specific request
    /// unless the request is exactly 'claude-sonnet-4-'.
    #[tokio::test]
    async fn test_cache_legacy_row_inert() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-20250514-v1:0".to_string(),
                anthropic_display: Some("claude-sonnet-4-20250514".to_string()),
            })
            .await;

        // Under exact-match, 'claude-sonnet-4-5-20250929' does NOT match 'claude-sonnet-4-'
        assert_eq!(
            cache.lookup_forward("claude-sonnet-4-5-20250929"),
            None,
            "lookup_forward must NOT match 'claude-sonnet-4-5-20250929' against row keyed 'claude-sonnet-4-'"
        );
    }

    /// AC5: Reverse lookup with exact suffix match.
    #[tokio::test]
    async fn test_cache_reverse_exact_suffix() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4-8".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-8".to_string(),
                anthropic_display: Some("claude-opus-4-8".to_string()),
            })
            .await;

        assert_eq!(
            cache.lookup_reverse("global.anthropic.claude-opus-4-8"),
            Some("claude-opus-4-8".to_string())
        );
    }

    /// Empty cache: lookup returns None.
    #[tokio::test]
    async fn test_cache_empty_lookup() {
        let cache = ModelCache::new();
        assert_eq!(cache.lookup_forward("claude-opus-4-8"), None);
        assert_eq!(
            cache.lookup_reverse("global.anthropic.claude-opus-4-8"),
            None
        );
    }
}
