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
    /// For use in sync contexts (e.g. `anthropic_to_bedrock`) that have a
    /// hardcoded fallback on miss. Use `lookup_forward_blocking` in async
    /// contexts where a spurious None would be fatal.
    pub fn lookup_forward(&self, anthropic_model: &str) -> Option<String> {
        let guard = self.inner.try_read().ok()?;
        guard.get(anthropic_model).map(|m| m.bedrock_suffix.clone())
    }

    /// Async forward lookup: blocks until the read lock is available.
    /// Unlike `lookup_forward`, this never returns `None` due to lock contention.
    /// Use this in async hot paths (e.g. `accept_model`) where a spurious cache
    /// miss causes visible failures.
    pub async fn lookup_forward_blocking(&self, anthropic_model: &str) -> Option<String> {
        let guard = self.inner.read().await;
        guard.get(anthropic_model).map(|m| m.bedrock_suffix.clone())
    }

    /// Forward lookup with a read-time date-suffix fallback.
    ///
    /// 1. Exact match on `model`.
    /// 2. On miss, if `model` carries a date suffix, retry once against the
    ///    date-stripped form — but ONLY when the stripped form is
    ///    minor-version-bearing (ends with two numeric dash-segments, e.g.
    ///    `claude-opus-4-8`), never a bare major (`claude-opus-4`). This guard
    ///    prevents recreating the greedy-prefix bug Slice 1 eliminated.
    ///
    /// Uses `try_read()`. Use `lookup_forward_with_fallback_blocking` in async
    /// contexts where a spurious None would be fatal.
    pub fn lookup_forward_with_fallback(&self, model: &str) -> Option<String> {
        if let Some(hit) = self.lookup_forward(model) {
            return Some(hit);
        }
        let stripped = strip_date_suffix(model);
        if stripped != model && is_minor_version_bearing(stripped) {
            return self.lookup_forward(stripped);
        }
        None
    }

    /// Async forward lookup with date-suffix fallback.
    /// Like `lookup_forward_with_fallback` but uses blocking reads — never
    /// returns `None` due to write-lock contention.
    pub async fn lookup_forward_with_fallback_blocking(&self, model: &str) -> Option<String> {
        if let Some(hit) = self.lookup_forward_blocking(model).await {
            return Some(hit);
        }
        let stripped = strip_date_suffix(model);
        if stripped != model && is_minor_version_bearing(stripped) {
            return self.lookup_forward_blocking(stripped).await;
        }
        None
    }

    /// Reverse lookup: bedrock model ID -> anthropic display name.
    /// First tries an exact-match fast path against known bedrock_suffix values,
    /// then falls back to the existing `contains` scan (for rows where the suffix
    /// doesn't appear verbatim in the profile ID).
    /// Uses `try_read()` to avoid blocking; returns None on contention.
    /// Use `lookup_reverse_blocking` in async hot paths.
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

    /// Async reverse lookup: blocks until the read lock is available.
    /// Unlike `lookup_reverse`, this never returns `None` due to lock contention.
    pub async fn lookup_reverse_blocking(&self, bedrock_model: &str) -> Option<String> {
        let guard = self.inner.read().await;

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

/// True when `s` ends with two numeric dash-segments (`…-<major>-<minor>`),
/// e.g. `claude-opus-4-8` -> true, `claude-opus-4` -> false.
/// Used to gate the date-strip fallback so a dated bare-major input
/// (`claude-opus-4-20250514`) never resolves to a `claude-opus-4` row.
fn is_minor_version_bearing(s: &str) -> bool {
    // Split on '-'; the last two segments must both be all-ASCII-digits.
    let mut segs = s.rsplit('-');
    let last = segs.next();
    let second_last = segs.next();
    matches!(
        (last, second_last),
        (Some(a), Some(b))
            if !a.is_empty() && a.bytes().all(|c| c.is_ascii_digit())
            && !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit())
    )
}

/// Select the best matching inference profile ID using two deterministic passes:
/// 1. Exact stem: profile_id ends with `.anthropic.{stripped}`.
/// 2. Versioned stem: profile_id matches `\.anthropic\.{stripped}-v\d+(:\d+)?$`.
///
/// First match within the earliest non-empty pass wins. Returns just the id.
/// Used in tests; production code uses `select_profile_id_with_via`.
#[allow(dead_code)]
fn select_profile_id<'a>(profile_ids: &'a [String], stripped: &str) -> Option<&'a str> {
    select_profile_id_with_via(profile_ids, stripped).map(|(id, _)| id)
}

/// Like `select_profile_id` but also returns which pass matched (`"pass1"` or `"pass2"`).
fn select_profile_id_with_via<'a>(
    profile_ids: &'a [String],
    stripped: &str,
) -> Option<(&'a str, &'static str)> {
    // Pass 1: exact stem
    let exact = format!(".anthropic.{stripped}");
    for id in profile_ids {
        if id.ends_with(&exact) {
            return Some((id.as_str(), "pass1"));
        }
    }
    // Pass 2: versioned stem  (.anthropic.{stripped}-v<digits>(:<digits>)? at end)
    // Build the regex from an ESCAPED stripped value (stripped contains '-' and could
    // in theory contain regex metachars; escape defensively).
    let pattern = format!(r"\.anthropic\.{}-v\d+(:\d+)?$", regex::escape(stripped));
    if let Ok(re) = regex::Regex::new(&pattern) {
        for id in profile_ids {
            if re.is_match(id) {
                return Some((id.as_str(), "pass2"));
            }
        }
    }
    None
}

/// Build discover_model's return tuple from the chosen profile id.
/// KEY FIX: anthropic_prefix is the EXACT requested model id, NOT the stripped form.
/// Returns (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix).
fn build_mapping_row(
    anthropic_model: &str,
    stripped: &str,
    chosen_profile_id: &str,
) -> (String, String, Option<String>, String) {
    // profile_prefix = before first '.', bedrock_suffix = after first '.'
    let (profile_prefix, bedrock_suffix) = match chosen_profile_id.find('.') {
        Some(i) => (
            chosen_profile_id[..i].to_string(),
            chosen_profile_id[i + 1..].to_string(),
        ),
        None => (chosen_profile_id.to_string(), chosen_profile_id.to_string()),
    };
    let anthropic_prefix = anthropic_model.to_string();
    let anthropic_display = if anthropic_model != stripped {
        Some(anthropic_model.to_string())
    } else {
        None
    };
    (
        anthropic_prefix,
        bedrock_suffix,
        anthropic_display,
        profile_prefix,
    )
}

/// Discover a model by calling Bedrock ListInferenceProfiles and matching.
/// Returns (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix, via) if found.
/// `via` is `"pass1"` or `"pass2"` indicating which selection pass matched.
pub async fn discover_model(
    bedrock_client: &aws_sdk_bedrock::Client,
    anthropic_model: &str,
    _prefix: &str,
) -> Option<(String, String, Option<String>, String, &'static str)> {
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

    // Build profile_ids list for selection
    let profile_ids: Vec<String> = profiles
        .iter()
        .map(|p| p.inference_profile_id().to_string())
        .collect();

    // Two-pass selection: exact stem -> versioned stem
    let (chosen, via) = match select_profile_id_with_via(&profile_ids, stripped) {
        Some(t) => t,
        None => {
            tracing::warn!(
                model = %anthropic_model,
                stripped = %stripped,
                "No matching inference profile found"
            );
            return None;
        }
    };

    // Build the mapping row (anthropic_prefix = exact requested id, NOT stripped)
    let (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix) =
        build_mapping_row(anthropic_model, stripped, chosen);

    tracing::info!(
        anthropic_prefix = %anthropic_prefix,
        bedrock_suffix = %bedrock_suffix,
        profile_id = %chosen,
        via = %via,
        "Discovered new model mapping"
    );

    Some((
        anthropic_prefix,
        bedrock_suffix,
        anthropic_display,
        profile_prefix,
        via,
    ))
}

/// Map Anthropic model IDs to Bedrock inference profile IDs.
///
/// CC sends Anthropic-format model IDs. We map these to Bedrock inference
/// profile IDs using the configured region prefix.
///
/// Checks the dynamic cache first, falls back to hardcoded mappings.
///
/// **Suffix stripping (Slice 4):** if the model ID ends with `[\w+]` (e.g. `[1m]`,
/// `[2m]`, `[batch]`, `[v2_0]`), that suffix is stripped before all other logic.
/// The suffix is discarded — no beta is auto-injected here. Capability for any
/// specific suffix is determined at the `/v1/models` advertising layer via
/// `SUFFIX_BETA_MAP` (defined in `src/endpoint/mod.rs`).
pub fn anthropic_to_bedrock(model: &str, prefix: &str, model_cache: Option<&ModelCache>) -> String {
    // Strip trailing [\w+] suffix (e.g. [1m], [2m], [batch], [v2_0]) before any
    // other logic. Compiled once via OnceLock.
    static SUFFIX_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = SUFFIX_RE.get_or_init(|| regex::Regex::new(r"\[\w+\]$").unwrap());
    let model_owned;
    let model: &str = if re.is_match(model) {
        model_owned = re.replace(model, "").into_owned();
        &model_owned
    } else {
        model
    };

    // If it already looks like a Bedrock ID (contains a dot prefix), pass through
    if model.contains('.') {
        return model.to_string();
    }

    // Try dynamic cache first (non-blocking)
    if let Some(cache) = model_cache
        && let Some(suffix) = cache.lookup_forward_with_fallback(model)
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

/// Parse the Bedrock foundation-model ID from the tail of a foundation-model ARN.
///
/// Expects an ARN of the form:
///   `arn:aws:bedrock:<region>:<account>:foundation-model/<model-id>`
///
/// Returns `Ok(&str)` containing the model ID after `foundation-model/`, or
/// `Err(String)` when the segment is absent or the tail is empty.
pub fn parse_foundation_model_from_arn(arn: &str) -> Result<&str, String> {
    const SEGMENT: &str = "foundation-model/";
    match arn.find(SEGMENT) {
        Some(idx) => {
            let tail = &arn[idx + SEGMENT.len()..];
            if tail.is_empty() {
                Err(format!(
                    "ARN '{}' has an empty tail after 'foundation-model/'",
                    arn
                ))
            } else {
                Ok(tail)
            }
        }
        None => Err(format!(
            "ARN '{}' does not contain a 'foundation-model/' segment",
            arn
        )),
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

    // --- Slice 2: Read-time date-suffix fallback tests ---
    // These tests are within #[cfg(test)] mod tests (defined at line 289)

    /// AC6: Cache contains row for `claude-opus-4-8` (no date).
    /// Lookup of `claude-opus-4-8-20260601` (WITH date) should return the cached suffix
    /// via the fallback (strips date from input, re-tries exact lookup).
    #[tokio::test]
    async fn test_lookup_with_fallback_strips_date_from_input() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4-8".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-8".to_string(),
                anthropic_display: None,
            })
            .await;

        // Dated input strips to "claude-opus-4-8" which matches the cache row
        assert_eq!(
            cache.lookup_forward_with_fallback("claude-opus-4-8-20260601"),
            Some("anthropic.claude-opus-4-8".to_string()),
            "AC6: Dated input should strip and match the no-date cache row"
        );
    }

    /// AC7: Cache contains row for `claude-opus-4-8-20260601` (WITH date).
    /// Lookup of `claude-opus-4-8` (no date) should return None.
    /// The fallback only strips dates from the INPUT; it never adds them.
    #[tokio::test]
    async fn test_lookup_with_fallback_does_not_add_dates() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4-8-20260601".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-8".to_string(),
                anthropic_display: Some("claude-opus-4-8-20260601".to_string()),
            })
            .await;

        // No-date input has no date to strip; exact miss on "claude-opus-4-8" returns None
        assert_eq!(
            cache.lookup_forward_with_fallback("claude-opus-4-8"),
            None,
            "AC7: Fallback must NOT add dates; no-date input against dated cache row returns None"
        );
    }

    /// AC8: The anti-greedy guarantee.
    /// Cache contains ONLY a bare-major row `claude-opus-4`.
    /// Lookup of `claude-opus-4-20250514` (dated bare-major) MUST return None.
    ///
    /// INTENT: The fallback refuses to route a dated bare-major input (like
    /// `claude-opus-4-20250514`) to a bare-major cache row (`claude-opus-4`),
    /// because that would recreate the exact greedy prefix bug.
    ///
    /// IMPLEMENTATION NOTE (builder): The spec's "ends with a digit" guard is
    /// insufficient to distinguish `claude-opus-4` (bare major, ends with `4`)
    /// from `claude-opus-4-8` (minor-version-bearing, ends with `8`). Both end
    /// with digits. You must implement a stricter guard that:
    /// - ALLOWS the fallback for `claude-opus-4-8-20260601` → `claude-opus-4-8` (AC6)
    /// - REJECTS the fallback for `claude-opus-4-20250514` → `claude-opus-4` (AC8)
    ///
    /// Suggested approach: after stripping, check that the stripped form contains
    /// a minor-version segment (e.g., trailing pattern `-\d{1,2}$` indicating a
    /// version like `-8`, NOT just ending at a bare major like `-4` where the `4`
    /// is the major version itself). Alternatively: only allow the fallback when
    /// `stripped.ends_with(char::is_ascii_digit)` AND `stripped` differs from input
    /// by exactly 9 chars (the `-YYYYMMDD` suffix) AND the char before the removed
    /// suffix was also a digit (meaning the stripped form has a trailing version segment).
    #[tokio::test]
    async fn test_lookup_with_fallback_refuses_bare_major_greedy_match() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-20250514-v1:0".to_string(),
                anthropic_display: Some("claude-opus-4-20250514".to_string()),
            })
            .await;

        // AC8: This MUST return None — the fallback must NOT route dated bare-major
        // input to a bare-major cache row (that's the greedy bug we're preventing)
        assert_eq!(
            cache.lookup_forward_with_fallback("claude-opus-4-20250514"),
            None,
            "AC8: Fallback must NOT match 'claude-opus-4-20250514' against bare-major row 'claude-opus-4'"
        );
    }

    /// AC9: Reassert AC1 under the new fallback method.
    /// Cache contains ONLY `claude-opus-4` (bare major).
    /// Lookup of `claude-opus-4-8` (minor-version) MUST return None.
    ///
    /// `claude-opus-4-8` has no date suffix, so the fallback never strips anything
    /// and never attempts a second lookup. Exact miss → None.
    #[tokio::test]
    async fn test_lookup_with_fallback_no_greedy_prefix_match() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-opus-4".to_string(),
                bedrock_suffix: "anthropic.claude-opus-4-20250514-v1:0".to_string(),
                anthropic_display: Some("claude-opus-4-20250514".to_string()),
            })
            .await;

        // AC9: This MUST return None — no date to strip, exact miss on "claude-opus-4-8"
        assert_eq!(
            cache.lookup_forward_with_fallback("claude-opus-4-8"),
            None,
            "AC9: Fallback must NOT match 'claude-opus-4-8' (no date) against bare-major row 'claude-opus-4'"
        );
    }

    // --- Slice 3: discover_model three-pass selection + exact-key write ---
    // Tests within #[cfg(test)] for Slice 3 acceptance criteria.
    //
    // BUILDER CONTRACT: These tests require two new pure functions that you MUST extract from
    // `discover_model` to make the logic testable without a live Bedrock client:
    //
    // 1. `select_profile_id<'a>(profile_ids: &'a [String], stripped: &str) -> Option<&'a str>`
    //    Implements the 3-pass selection logic:
    //    - Pass 1 (exact stem): profile_id ends with `.anthropic.{stripped}`. First match wins.
    //    - Pass 2 (versioned stem): profile_id matches regex `\.anthropic\.{stripped}-v\d+(:\d+)?$`. First match wins.
    //    - Pass 3 (fuzzy contains): existing behavior — first profile_id that contains `stripped`.
    //    Returns the chosen profile_id (full string like "global.anthropic.claude-opus-4-8").
    //
    // 2. `build_mapping_row(anthropic_model: &str, stripped: &str, chosen_profile_id: &str) -> (String, String, Option<String>, String)`
    //    Builds the return tuple (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix).
    //    Key change: `anthropic_prefix` MUST be `anthropic_model` (the exact requested ID), NOT `stripped`.
    //    `anthropic_display` is Some(anthropic_model) when anthropic_model != stripped, else None.
    //    `profile_prefix` extracts the region (everything before first '.').
    //    `bedrock_suffix` extracts everything after first '.'.
    //
    // Your implementation: call `select_profile_id` to pick the winner, then `build_mapping_row` to construct the result.

    /// AC10: Exact stem beats variant.
    /// Profile list contains both "global.anthropic.claude-opus-4-8" and
    /// "global.anthropic.claude-opus-4-8-thinking". Pass 1 (exact stem) MUST win,
    /// returning the base profile, NOT the `-thinking` variant.
    #[test]
    fn test_select_profile_exact_stem_beats_variant() {
        let profiles = vec![
            "global.anthropic.claude-opus-4-8".to_string(),
            "global.anthropic.claude-opus-4-8-thinking".to_string(),
        ];
        let stripped = "claude-opus-4-8";

        let chosen = select_profile_id(&profiles, stripped);
        assert_eq!(
            chosen,
            Some("global.anthropic.claude-opus-4-8"),
            "AC10: Exact stem '.anthropic.claude-opus-4-8' must beat '-thinking' variant"
        );
    }

    /// AC10 (order independence): Exact stem wins regardless of list order.
    /// Reversed profile list — exact stem still wins via pass 1, not position.
    #[test]
    fn test_select_profile_exact_stem_order_independent() {
        let profiles = vec![
            "global.anthropic.claude-opus-4-8-thinking".to_string(),
            "global.anthropic.claude-opus-4-8".to_string(),
        ];
        let stripped = "claude-opus-4-8";

        let chosen = select_profile_id(&profiles, stripped);
        assert_eq!(
            chosen,
            Some("global.anthropic.claude-opus-4-8"),
            "AC10: Exact stem must win even when it appears AFTER the variant in the list"
        );
    }

    /// AC11: Versioned stem match (pass 2).
    /// Profile list contains "global.anthropic.claude-opus-4-6-v1".
    /// No exact stem match exists (`global.anthropic.claude-opus-4-6` is absent),
    /// so pass 2 (versioned-stem regex) fires and returns the `-v1` profile.
    #[test]
    fn test_select_profile_versioned_stem() {
        let profiles = vec!["global.anthropic.claude-opus-4-6-v1".to_string()];
        let stripped = "claude-opus-4-6";

        let chosen = select_profile_id(&profiles, stripped);
        assert_eq!(
            chosen,
            Some("global.anthropic.claude-opus-4-6-v1"),
            "AC11: Versioned-stem match (pass 2) must succeed when exact stem is absent"
        );
    }

    /// AC11 (versioned stem with sub-version): Test `-v1:0` form.
    /// The versioned-stem regex allows optional `:\d+` after `-v\d+`.
    #[test]
    fn test_select_profile_versioned_stem_with_subversion() {
        let profiles = vec!["us.anthropic.claude-sonnet-4-6-v1:0".to_string()];
        let stripped = "claude-sonnet-4-6";

        let chosen = select_profile_id(&profiles, stripped);
        assert_eq!(
            chosen,
            Some("us.anthropic.claude-sonnet-4-6-v1:0"),
            "AC11: Versioned-stem match must accept `-v1:0` form (with sub-version)"
        );
    }

    /// AC11 (pass-order priority): Exact stem beats versioned stem when both exist.
    /// Profiles list contains BOTH `global.anthropic.claude-opus-4-6` (exact stem)
    /// and `global.anthropic.claude-opus-4-6-v2` (versioned stem). Pass 1 must win.
    #[test]
    fn test_select_profile_exact_stem_beats_versioned_stem() {
        let profiles = vec![
            "global.anthropic.claude-opus-4-6-v2".to_string(),
            "global.anthropic.claude-opus-4-6".to_string(),
        ];
        let stripped = "claude-opus-4-6";

        let chosen = select_profile_id(&profiles, stripped);
        assert_eq!(
            chosen,
            Some("global.anthropic.claude-opus-4-6"),
            "Exact stem (pass 1) must beat versioned stem (pass 2) when both exist"
        );
    }

    // Tests below are within #[cfg(test)] mod tests.

    /// AC4.1: Pass-3-only inputs return None after Pass 3 is removed.
    ///
    /// Both profiles contain the stripped form (`claude-opus-4-8`) as a substring, so the
    /// legacy Pass 3 (fuzzy contains) would have matched. Neither satisfies Pass 1 (exact stem:
    /// ends with `.anthropic.claude-opus-4-8`) nor Pass 2 (versioned stem: matches
    /// `\.anthropic\.claude-opus-4-8-v\d+...`). With Pass 3 gone, the function must return None.
    #[test]
    fn test_ac4_1_pass3_only_match_returns_none() {
        let profiles = vec![
            "global.foo.claude-opus-4-8-experimental".to_string(),
            "us.anthropic.claude-opus-4-8-custom".to_string(),
        ];
        // Pass 1 miss: neither ends with `.anthropic.claude-opus-4-8`
        // Pass 2 miss: neither matches `\.anthropic\.claude-opus-4-8-v\d+(:\d+)?$`
        // Pass 3 (removed): would have returned `global.foo.claude-opus-4-8-experimental`
        assert!(
            super::select_profile_id(&profiles, "claude-opus-4-8").is_none(),
            "AC4.1: inputs matching only fuzzy-contains (Pass 3) must return None once Pass 3 is removed"
        );
    }

    /// No match: all three passes miss.
    #[test]
    fn test_select_profile_no_match() {
        let profiles = vec![
            "global.anthropic.claude-sonnet-4-6".to_string(),
            "us.anthropic.claude-haiku-4-5".to_string(),
        ];
        let stripped = "claude-opus-4-8";

        let chosen = select_profile_id(&profiles, stripped);
        assert_eq!(
            chosen, None,
            "select_profile_id must return None when no profile matches"
        );
    }

    /// AC12: THE PRINCIPAL REGRESSION GUARD — persisted prefix is the exact id.
    /// After `discover_model("claude-opus-4-6-20250605", ...)` resolves, the persisted/returned
    /// `anthropic_prefix` MUST equal `"claude-opus-4-6-20250605"` (the exact request), NOT
    /// `"claude-opus-4-6"` (the stripped form). This is the core bug fix.
    ///
    /// Test via the pure `build_mapping_row` function.
    #[test]
    fn test_build_mapping_row_persists_exact_id_with_date() {
        let anthropic_model = "claude-opus-4-6-20250605";
        let stripped = "claude-opus-4-6";
        let chosen_profile_id = "global.anthropic.claude-opus-4-6-v1";

        let (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix) =
            build_mapping_row(anthropic_model, stripped, chosen_profile_id);

        // AC12: The prefix MUST be the exact requested model ID
        assert_eq!(
            anthropic_prefix, "claude-opus-4-6-20250605",
            "AC12: anthropic_prefix must be the EXACT requested model ID (with date), not stripped form"
        );

        // Verify the other fields for completeness
        assert_eq!(
            bedrock_suffix, "anthropic.claude-opus-4-6-v1",
            "bedrock_suffix must extract everything after the first '.'"
        );
        assert_eq!(
            anthropic_display,
            Some("claude-opus-4-6-20250605".to_string()),
            "anthropic_display must be Some(exact_id) when exact_id != stripped"
        );
        assert_eq!(
            profile_prefix, "global",
            "profile_prefix must extract the region before the first '.'"
        );
    }

    /// AC13: No-date input — persisted prefix equals the input (which equals stripped).
    /// When the input has no date suffix (input == stripped), `anthropic_prefix` still
    /// equals the input, and `anthropic_display` is None (since input == stripped).
    #[test]
    fn test_build_mapping_row_persists_exact_id_no_date() {
        let anthropic_model = "claude-opus-4-7";
        let stripped = "claude-opus-4-7"; // No date, so stripped == input
        let chosen_profile_id = "us.anthropic.claude-opus-4-7";

        let (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix) =
            build_mapping_row(anthropic_model, stripped, chosen_profile_id);

        // AC13: anthropic_prefix still equals the exact input (which happens to equal stripped)
        assert_eq!(
            anthropic_prefix, "claude-opus-4-7",
            "AC13: anthropic_prefix must be the exact requested model ID (no date case)"
        );

        // When input == stripped, anthropic_display is None
        assert_eq!(
            anthropic_display, None,
            "AC13: anthropic_display must be None when input == stripped (no date)"
        );

        // Verify the other fields
        assert_eq!(
            bedrock_suffix, "anthropic.claude-opus-4-7",
            "bedrock_suffix must extract everything after the first '.'"
        );
        assert_eq!(
            profile_prefix, "us",
            "profile_prefix must extract the region before the first '.'"
        );
    }

    /// Edge case: profile_id with no dot (malformed, but defensively handled).
    /// `profile_prefix` should be the whole string, `bedrock_suffix` should be the whole string.
    #[test]
    fn test_build_mapping_row_no_dot_in_profile_id() {
        let anthropic_model = "claude-test";
        let stripped = "claude-test";
        let chosen_profile_id = "malformed-profile-id";

        let (anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix) =
            build_mapping_row(anthropic_model, stripped, chosen_profile_id);

        assert_eq!(
            anthropic_prefix, "claude-test",
            "anthropic_prefix is always the exact input"
        );
        assert_eq!(
            bedrock_suffix, "malformed-profile-id",
            "bedrock_suffix fallback: entire string when no dot found"
        );
        assert_eq!(anthropic_display, None, "input == stripped → None");
        assert_eq!(
            profile_prefix, "malformed-profile-id",
            "profile_prefix fallback: entire string when no dot found"
        );
    }
}

/// Task 4 — Generic `[\w+]` suffix stripping in `anthropic_to_bedrock`.
///
/// The function must strip a trailing `[\w+]` (regex `\[\w+\]$`) from the
/// model ID before all other logic (dot-passthrough check, cache lookup,
/// hardcoded match, discovery fallback).
///
/// The suffix is silently discarded — it is not used to inject betas.
/// That remains the responsibility of the client (CC sends the beta header;
/// the gateway forwards it via the Slice 3 mechanism).
#[cfg(test)]
mod tests_t4_suffix_strip {
    use super::*;

    /// T4-1: `[1m]` suffix is stripped; result equals the bare Bedrock ID.
    /// `claude-opus-4-7[1m]` → same as `claude-opus-4-7` (hardcoded mapping).
    #[test]
    fn strips_1m_suffix_from_anthropic_id() {
        let with_suffix = anthropic_to_bedrock("claude-opus-4-7[1m]", "us", None);
        let bare = anthropic_to_bedrock("claude-opus-4-7", "us", None);
        assert_eq!(
            with_suffix, bare,
            "anthropic_to_bedrock must strip [1m] and produce the same result as the bare ID"
        );
        assert_eq!(
            with_suffix, "us.anthropic.claude-opus-4-7",
            "stripped [1m] from claude-opus-4-7 should map to us.anthropic.claude-opus-4-7"
        );
    }

    /// T4-2: `[2m]` suffix is stripped generically — no validation at this layer.
    /// The regex strips any `[\w+]` regardless of whether the suffix is in SUFFIX_BETA_MAP.
    #[test]
    fn strips_2m_suffix_generically() {
        let with_suffix = anthropic_to_bedrock("claude-opus-4-7[2m]", "us", None);
        let bare = anthropic_to_bedrock("claude-opus-4-7", "us", None);
        assert_eq!(
            with_suffix, bare,
            "anthropic_to_bedrock must strip [2m] generically (no validation against SUFFIX_BETA_MAP)"
        );
        assert_eq!(with_suffix, "us.anthropic.claude-opus-4-7");
    }

    /// T4-3: Alphabetic suffix `[batch]` is stripped.
    #[test]
    fn strips_alphanumeric_suffix() {
        let with_suffix = anthropic_to_bedrock("claude-opus-4-7[batch]", "us", None);
        let bare = anthropic_to_bedrock("claude-opus-4-7", "us", None);
        assert_eq!(
            with_suffix, bare,
            "anthropic_to_bedrock must strip [batch] suffix"
        );
    }

    /// T4-4: Underscore suffix `[v2_0]` is stripped.
    /// `\w+` matches `[a-zA-Z0-9_]+`, so underscores are valid inside the brackets.
    #[test]
    fn strips_underscore_suffix() {
        let with_suffix = anthropic_to_bedrock("claude-opus-4-7[v2_0]", "us", None);
        let bare = anthropic_to_bedrock("claude-opus-4-7", "us", None);
        assert_eq!(
            with_suffix, bare,
            "anthropic_to_bedrock must strip [v2_0] suffix (underscore is valid \\w)"
        );
    }

    /// T4-5: Suffix on an already-Bedrock-style ID (`us.anthropic.claude-opus-4-7[1m]`).
    ///
    /// The dot-passthrough check runs AFTER the strip, so:
    ///   1. Strip `[1m]` → `us.anthropic.claude-opus-4-7`
    ///   2. Contains dot → passthrough → `us.anthropic.claude-opus-4-7`
    ///
    /// Result equals the bare Bedrock ID with no suffix.
    #[test]
    fn strips_suffix_on_bedrock_style_id() {
        let with_suffix = anthropic_to_bedrock("us.anthropic.claude-opus-4-7[1m]", "us", None);
        assert_eq!(
            with_suffix, "us.anthropic.claude-opus-4-7",
            "strip must run before the dot-passthrough check so Bedrock-style IDs with suffix also work"
        );
    }

    /// T4-6: No suffix on an unknown/future model ID — behavior unchanged.
    /// `claude-future-9-9` has no hardcoded mapping and no cache, so it passes
    /// through the `other =>` arm and is returned dotless (for discovery).
    #[test]
    fn no_suffix_unchanged_unknown_model() {
        let result = anthropic_to_bedrock("claude-future-9-9", "us", None);
        assert_eq!(
            result, "claude-future-9-9",
            "Unknown model without suffix must still pass through dotless (discovery path)"
        );
    }

    /// T4-7: No suffix on a known model — existing hardcoded mapping is unaffected.
    #[test]
    fn no_suffix_unchanged_known_model() {
        let result = anthropic_to_bedrock("claude-sonnet-4-6-20250514", "us", None);
        assert_eq!(
            result, "us.anthropic.claude-sonnet-4-6",
            "Existing hardcoded mapping must be unaffected after adding strip logic"
        );
    }

    /// T4-8: Empty brackets `[]` must NOT be stripped.
    /// The regex `\[\w+\]$` requires at least one word character inside brackets.
    /// `claude-opus-4-7[]` has no known hardcoded mapping, so it passes through
    /// the `other =>` arm and is returned verbatim (dotless pass-through).
    ///
    /// This is defensive: we don't want accidental stripping of model IDs that
    /// happen to end with `[]`.
    #[test]
    fn empty_brackets_not_stripped() {
        let result = anthropic_to_bedrock("claude-opus-4-7[]", "us", None);
        // `[]` does NOT match `\[\w+\]$` — the empty bracket falls to `other =>` arm
        // and is returned verbatim (dotless). This is intentional: `[]` is not a
        // valid suffix per the spec; `\w+` requires ≥1 word char.
        assert_eq!(
            result, "claude-opus-4-7[]",
            "Empty brackets [] must NOT be stripped (\\w+ requires ≥1 char)"
        );
    }

    /// T4-9: Unbalanced brackets (missing close) must NOT be stripped.
    /// `claude-opus-4-7[1m` has no closing `]`, so the regex does not match.
    /// Falls through to `other =>` arm and is returned verbatim.
    #[test]
    fn unbalanced_brackets_not_stripped() {
        let result = anthropic_to_bedrock("claude-opus-4-7[1m", "us", None);
        assert_eq!(
            result, "claude-opus-4-7[1m",
            "Missing close bracket must NOT be stripped"
        );
    }

    /// T4-10: Bracket not at end-of-string must NOT be stripped.
    /// `claude[1m]-opus-4-7` — the `[1m]` is mid-string, not at the end.
    /// The regex is anchored with `$`, so this must not match.
    /// Falls through to `other =>` arm and is returned verbatim.
    #[test]
    fn bracket_not_at_end_not_stripped() {
        let result = anthropic_to_bedrock("claude[1m]-opus-4-7", "us", None);
        assert_eq!(
            result, "claude[1m]-opus-4-7",
            "Bracket not at end of string must NOT be stripped (regex is end-anchored)"
        );
    }
}

// ── Task 2: ARN-tail parser tests ────────────────────────────────────────────
//
// These tests cover `parse_foundation_model_from_arn`, a pure helper that
// extracts the Bedrock foundation-model id from an inference-profile ARN.
// The function finds the `foundation-model/` segment and returns the
// everything after the `/` (e.g. `"anthropic.claude-sonnet-4-5-20250929-v1:0"`),
// or `Err(String)` when the segment is absent or the tail is empty.
//
// The self-healing migration runner uses this to derive `anthropic_prefix`
// from a legacy `inference_profile_arn` column value, then calls
// `bedrock_to_anthropic(tail, None)` to get the logical Anthropic model name.

#[cfg(test)]
mod tests_task2_arn_parser {
    use super::*;

    // ── 1. Happy-path ARN parsing ─────────────────────────────────────────────

    /// Well-formed foundation-model ARN (Sonnet 4.5) → returns the tail after
    /// `foundation-model/`.
    #[test]
    fn test_parse_foundation_model_from_arn_sonnet_4_5() {
        let arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0";
        let result = parse_foundation_model_from_arn(arn);
        assert!(
            result.is_ok(),
            "parse_foundation_model_from_arn must succeed for a well-formed ARN; got {result:?}"
        );
        assert_eq!(
            result.unwrap(),
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "returned tail must be the foundation-model id exactly as it appears in the ARN"
        );
    }

    /// Well-formed foundation-model ARN (Haiku 4.5) → returns the tail.
    #[test]
    fn test_parse_foundation_model_from_arn_haiku_4_5() {
        let arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-haiku-4-5-20251001-v1:0";
        let result = parse_foundation_model_from_arn(arn);
        assert!(result.is_ok(), "haiku ARN must parse successfully");
        assert_eq!(result.unwrap(), "anthropic.claude-haiku-4-5-20251001-v1:0");
    }

    /// Well-formed foundation-model ARN (Opus 4.7) → returns the tail.
    #[test]
    fn test_parse_foundation_model_from_arn_opus_4_7() {
        let arn =
            "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-opus-4-7";
        let result = parse_foundation_model_from_arn(arn);
        assert!(result.is_ok(), "opus ARN must parse successfully");
        assert_eq!(result.unwrap(), "anthropic.claude-opus-4-7");
    }

    // ── 2. End-to-end: parse → bedrock_to_anthropic ───────────────────────────

    /// Parse a Sonnet 4.5 ARN and map the tail through `bedrock_to_anthropic`.
    /// The expected Anthropic logical model id is the value the hardcoded map
    /// produces for `"us.anthropic.claude-sonnet-4-5-20250929-v1:0"`.
    /// We test against `bedrock_to_anthropic(tail, None)` directly (no cache).
    #[test]
    fn test_arn_to_anthropic_model_sonnet_4_5() {
        let arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0";
        let tail = parse_foundation_model_from_arn(arn)
            .expect("parse must succeed for this well-formed ARN");

        // bedrock_to_anthropic needs the full profile id (with regional prefix) in
        // some code paths, but the hardcoded reverse map uses `contains`, so the
        // bare tail string still matches.
        let anthropic_name = bedrock_to_anthropic(tail, None);
        assert_eq!(
            anthropic_name, "claude-sonnet-4-5-20250929",
            "tail of a Sonnet 4.5 ARN must map to 'claude-sonnet-4-5-20250929' via bedrock_to_anthropic"
        );
    }

    /// Parse a Haiku 4.5 ARN and map the tail through `bedrock_to_anthropic`.
    #[test]
    fn test_arn_to_anthropic_model_haiku_4_5() {
        let arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-haiku-4-5-20251001-v1:0";
        let tail = parse_foundation_model_from_arn(arn)
            .expect("parse must succeed for this well-formed ARN");

        let anthropic_name = bedrock_to_anthropic(tail, None);
        assert_eq!(
            anthropic_name, "claude-haiku-4-5-20251001",
            "tail of a Haiku 4.5 ARN must map to 'claude-haiku-4-5-20251001' via bedrock_to_anthropic"
        );
    }

    // ── 3. Error cases ────────────────────────────────────────────────────────

    /// ARN with no `foundation-model/` segment → `Err`.
    #[test]
    fn test_parse_foundation_model_from_arn_missing_segment() {
        let arn = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-tagged-profile";
        let result = parse_foundation_model_from_arn(arn);
        assert!(
            result.is_err(),
            "ARN without 'foundation-model/' segment must return Err; got {result:?}"
        );
    }

    /// Completely wrong string (not an ARN at all) → `Err`.
    #[test]
    fn test_parse_foundation_model_from_arn_not_an_arn() {
        let result = parse_foundation_model_from_arn("not-an-arn");
        assert!(
            result.is_err(),
            "Non-ARN string must return Err; got {result:?}"
        );
    }

    /// Empty string → `Err`.
    #[test]
    fn test_parse_foundation_model_from_arn_empty_string() {
        let result = parse_foundation_model_from_arn("");
        assert!(
            result.is_err(),
            "Empty string must return Err; got {result:?}"
        );
    }

    /// ARN that ends immediately after `foundation-model/` (empty tail) → `Err`.
    #[test]
    fn test_parse_foundation_model_from_arn_empty_tail() {
        let arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model/";
        let result = parse_foundation_model_from_arn(arn);
        assert!(
            result.is_err(),
            "ARN with empty tail after 'foundation-model/' must return Err; got {result:?}"
        );
    }

    /// Truncated ARN (no trailing portion after the resource type) → `Err`.
    #[test]
    fn test_parse_foundation_model_from_arn_truncated() {
        let arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model";
        let result = parse_foundation_model_from_arn(arn);
        assert!(
            result.is_err(),
            "Truncated ARN (no '/' after resource type) must return Err; got {result:?}"
        );
    }
}

// ── Task 2: Self-healing migration runner unit tests ─────────────────────────
//
// Tests for `migrate_legacy_aip_endpoints` (implemented in
// `src/migrations/aip_legacy.rs`).  This startup function scans endpoints
// that carry a legacy `inference_profile_arn` value but have no AIP override
// rows and auto-inserts overrides by calling `GetInferenceProfile` +
// `parse_foundation_model_from_arn` + `bedrock_to_anthropic`.
//
// The function accepts a `get_foundation_model` closure as a testable seam so
// the pure-logic cases (skip-if-non-empty, idempotency, error-tolerance) can
// be exercised without a real AWS call.  The tests below use this seam.
//
// These tests are unit-style (no DB required) for the decision-logic paths,
// and integration-style (requires `make test-integration`) for the DB paths.
// The module is kept here so it is compiled even without the `integration`
// feature flag.

#[cfg(test)]
mod tests_task2_migration_runner {
    use super::*;

    // ── In-memory stub DB ─────────────────────────────────────────────────────
    //
    // Rather than a real PgPool (which requires Docker), these unit tests use a
    // minimal stub that tracks "what rows exist per endpoint_id" in memory.
    // The production runner takes a `&sqlx::PgPool`; the unit tests below
    // use the *extractable pure logic* path by directly exercising the helper
    // functions the migration runner delegates to.
    //
    // BUILDER NOTE: if you extract the core loop into a helper that accepts a
    // `list_fn` + `insert_fn` pair (function pointers / closures) in addition to
    // the `get_foundation_model` seam, these tests become straightforward.
    // Otherwise, the integration test file covers the full DB path.
    //
    // The tests here verify the *decision logic* through the ARN parser + the
    // `bedrock_to_anthropic` round-trip, keeping them fast and dependency-free.

    // ── ARN-parsing + reverse-mapping round-trips ─────────────────────────────

    /// Full round-trip for Sonnet 4.5:
    ///   AIP ARN → parse tail → bedrock_to_anthropic → "claude-sonnet-4-5-20250929"
    ///
    /// This is the exact computation `migrate_legacy_aip_endpoints` performs
    /// in step 2. The result becomes `model_id` in the inserted row.
    #[test]
    fn test_migration_model_id_derivation_sonnet() {
        let legacy_arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-sonnet-4-5-20250929-v1:0";
        let tail = parse_foundation_model_from_arn(legacy_arn).expect("well-formed ARN must parse");
        let anthropic_model = bedrock_to_anthropic(tail, None);

        assert_eq!(
            anthropic_model, "claude-sonnet-4-5-20250929",
            "migration must derive 'claude-sonnet-4-5-20250929' as the model_id for the Sonnet 4.5 ARN"
        );
    }

    /// Full round-trip for Haiku 4.5:
    ///   AIP ARN → parse tail → bedrock_to_anthropic → "claude-haiku-4-5-20251001"
    #[test]
    fn test_migration_model_id_derivation_haiku() {
        let legacy_arn = "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-haiku-4-5-20251001-v1:0";
        let tail = parse_foundation_model_from_arn(legacy_arn).expect("well-formed ARN must parse");
        let anthropic_model = bedrock_to_anthropic(tail, None);

        assert_eq!(
            anthropic_model, "claude-haiku-4-5-20251001",
            "migration must derive 'claude-haiku-4-5-20251001' as the model_id for the Haiku 4.5 ARN"
        );
    }

    /// An ARN whose resource type is `application-inference-profile` (not
    /// `foundation-model`) must produce an `Err` from `parse_foundation_model_from_arn`,
    /// simulating the `GetInferenceProfile` failure path in the migration runner.
    /// The runner must log a warning and continue — this test verifies the Err
    /// propagation that the runner's error branch receives.
    #[test]
    fn test_migration_bad_arn_produces_err() {
        // An AIP ARN is NOT a foundation-model ARN; the parser must reject it.
        let bad_arn = "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-aip";
        let result = parse_foundation_model_from_arn(bad_arn);
        assert!(
            result.is_err(),
            "parse_foundation_model_from_arn must return Err for an AIP ARN (not a foundation-model ARN); migration runner must handle this without panicking"
        );
    }
} // end #[cfg(test)] mod tests_task2_arn_parser
