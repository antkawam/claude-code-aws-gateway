use std::sync::Arc;

use tokio::sync::RwLock;

/// A single cached model mapping entry.
#[derive(Debug, Clone)]
pub struct CachedMapping {
    pub anthropic_prefix: String,
    pub bedrock_suffix: String,
    pub anthropic_display: Option<String>,
}

/// In-memory model mapping cache. Entries are ordered by prefix length descending
/// (longest first) so that `starts_with` matching picks the most specific prefix.
#[derive(Clone)]
pub struct ModelCache {
    inner: Arc<RwLock<Vec<CachedMapping>>>,
}

impl Default for ModelCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Load all mappings from DB into cache (replaces existing entries).
    pub async fn load_from_db(&self, pool: &sqlx::PgPool) -> Result<usize, sqlx::Error> {
        let rows = crate::db::model_mappings::get_all_mappings(pool).await?;
        let count = rows.len();
        let mappings: Vec<CachedMapping> = rows
            .into_iter()
            .map(|r| CachedMapping {
                anthropic_prefix: r.anthropic_prefix,
                bedrock_suffix: r.bedrock_suffix,
                anthropic_display: r.anthropic_display,
            })
            .collect();
        // Already ordered by length DESC from DB query
        *self.inner.write().await = mappings;
        Ok(count)
    }

    /// Forward lookup: anthropic model ID -> bedrock suffix.
    /// Uses `try_read()` to avoid blocking; returns None on contention.
    pub fn lookup_forward(&self, anthropic_model: &str) -> Option<String> {
        let guard = self.inner.try_read().ok()?;
        for m in guard.iter() {
            if anthropic_model.starts_with(&m.anthropic_prefix) {
                return Some(m.bedrock_suffix.clone());
            }
        }
        None
    }

    /// Reverse lookup: bedrock model ID -> anthropic display name.
    /// Uses `try_read()` to avoid blocking; returns None on contention.
    pub fn lookup_reverse(&self, bedrock_model: &str) -> Option<String> {
        let guard = self.inner.try_read().ok()?;
        for m in guard.iter() {
            if bedrock_model.contains(&m.bedrock_suffix)
                || bedrock_model.contains(&m.anthropic_prefix)
            {
                return m.anthropic_display.clone();
            }
        }
        None
    }

    /// Insert a mapping into the cache in sorted position (by prefix length desc).
    pub async fn insert(&self, mapping: CachedMapping) {
        let mut guard = self.inner.write().await;
        // Remove existing entry with same prefix
        guard.retain(|m| m.anthropic_prefix != mapping.anthropic_prefix);
        // Find insertion point (maintain descending length order)
        let pos = guard
            .iter()
            .position(|m| m.anthropic_prefix.len() < mapping.anthropic_prefix.len())
            .unwrap_or(guard.len());
        guard.insert(pos, mapping);
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
/// Returns (anthropic_prefix, bedrock_suffix, anthropic_display) if found.
pub async fn discover_model(
    bedrock_client: &aws_sdk_bedrock::Client,
    anthropic_model: &str,
    _prefix: &str,
) -> Option<(String, String, Option<String>)> {
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

            return Some((anthropic_prefix, bedrock_suffix, anthropic_display));
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
        let corrected = correct_prefix_for_model(model, prefix);
        return format!("{corrected}.{suffix}");
    }

    // Fall back to hardcoded mappings
    hardcoded_anthropic_to_bedrock(model, prefix)
}

/// Correct the regional prefix for models that don't have profiles under
/// certain regional prefixes. E.g. Sonnet 4 has no `au.` profile (use `apac.`),
/// Opus 4.5 has no `au.`/`apac.`/`jp.` profiles (use `global.`).
fn correct_prefix_for_model<'a>(model: &str, prefix: &'a str) -> &'a str {
    // Opus 4.5: only us, eu, global
    if model.starts_with("claude-opus-4-5") {
        return match prefix {
            "au" | "apac" | "jp" => "global",
            _ => prefix,
        };
    }
    // Opus 4.6: us, eu, au, global (not apac, jp)
    if model.starts_with("claude-opus-4-6") {
        return match prefix {
            "apac" | "jp" => "global",
            _ => prefix,
        };
    }
    // Sonnet 4 (not 4.5/4.6): us, eu, apac, global (not au, jp)
    if model.starts_with("claude-sonnet-4-")
        && !model.starts_with("claude-sonnet-4-5")
        && !model.starts_with("claude-sonnet-4-6")
    {
        return match prefix {
            "au" | "jp" => "apac",
            _ => prefix,
        };
    }
    // Newer models (4.5+, 4.6): available in us, eu, au, jp, global — NOT apac
    if model.starts_with("claude-sonnet-4-5")
        || model.starts_with("claude-sonnet-4-6")
        || model.starts_with("claude-haiku-4-5")
    {
        return match prefix {
            "apac" => "global",
            _ => prefix,
        };
    }
    prefix
}

/// Hardcoded forward mapping (no-DB fallback).
fn hardcoded_anthropic_to_bedrock(model: &str, prefix: &str) -> String {
    let p = correct_prefix_for_model(model, prefix);
    match model {
        s if s.starts_with("claude-opus-4-6") => {
            format!("{p}.anthropic.claude-opus-4-6-v1")
        }
        s if s.starts_with("claude-sonnet-4-6") => {
            format!("{p}.anthropic.claude-sonnet-4-6")
        }
        s if s.starts_with("claude-opus-4-5") => {
            format!("{p}.anthropic.claude-opus-4-5-20251101-v1:0")
        }
        s if s.starts_with("claude-sonnet-4-5") => {
            format!("{p}.anthropic.claude-sonnet-4-5-20250929-v1:0")
        }
        s if s.starts_with("claude-sonnet-4-") => {
            format!("{p}.anthropic.claude-sonnet-4-20250514-v1:0")
        }
        s if s.starts_with("claude-haiku-4-5") => {
            format!("{p}.anthropic.claude-haiku-4-5-20251001-v1:0")
        }
        // Fallback: pass through as-is
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
        s if s.contains("claude-opus-4-6") => "claude-opus-4-6-20250605".to_string(),
        s if s.contains("claude-sonnet-4-6") => "claude-sonnet-4-6-20250514".to_string(),
        s if s.contains("claude-opus-4-5") => "claude-opus-4-5-20251101".to_string(),
        s if s.contains("claude-sonnet-4-5") => "claude-sonnet-4-5-20250929".to_string(),
        s if s.contains("claude-sonnet-4-2025") => "claude-sonnet-4-20250514".to_string(),
        s if s.contains("claude-haiku-4-5") => "claude-haiku-4-5-20251001".to_string(),
        other => other.to_string(),
    }
}

/// Beta flags known to work on Bedrock. We use an allowlist because CC sends
/// many Anthropic-specific betas (claude-code-*, adaptive-thinking-*, etc.)
/// that Bedrock rejects with "invalid beta flag".
const ALLOWED_BEDROCK_BETAS: &[&str] = &[
    "interleaved-thinking-2025-05-14",
    "context-1m-2025-08-07",
    "token-counting-2024-11-01",
    "tool-search-tool-2025-10-19",
    // Prompt caching betas — Bedrock may accept or ignore these
    "prompt-caching-2024-07-31",
];

/// Parse the `anthropic-beta` header (comma-separated) and return
/// only the betas that Bedrock is known to support.
pub fn filter_betas(anthropic_beta_header: &str) -> Vec<String> {
    anthropic_beta_header
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| ALLOWED_BEDROCK_BETAS.contains(&s.as_str()))
        .collect()
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
    }

    #[test]
    fn test_all_hardcoded_mappings() {
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

    #[tokio::test]
    async fn test_cache_forward_lookup() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-6".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-6".to_string(),
                anthropic_display: Some("claude-sonnet-4-6-20250514".to_string()),
            })
            .await;

        assert_eq!(
            cache.lookup_forward("claude-sonnet-4-6-20250514"),
            Some("anthropic.claude-sonnet-4-6".to_string())
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

    #[tokio::test]
    async fn test_cache_prefix_ordering() {
        let cache = ModelCache::new();
        // Insert in wrong order — cache should sort by length desc
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-20250514-v1:0".to_string(),
                anthropic_display: Some("claude-sonnet-4-20250514".to_string()),
            })
            .await;
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-5".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
                anthropic_display: Some("claude-sonnet-4-5-20250929".to_string()),
            })
            .await;

        // Specific prefix should match before catch-all
        assert_eq!(
            cache.lookup_forward("claude-sonnet-4-5-20250929"),
            Some("anthropic.claude-sonnet-4-5-20250929-v1:0".to_string())
        );
        // Catch-all should still work for other sonnet-4 variants
        assert_eq!(
            cache.lookup_forward("claude-sonnet-4-20250514"),
            Some("anthropic.claude-sonnet-4-20250514-v1:0".to_string())
        );
    }

    #[tokio::test]
    async fn test_cache_with_anthropic_to_bedrock() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-future-5-0".to_string(),
                bedrock_suffix: "anthropic.claude-future-5-0-v1".to_string(),
                anthropic_display: Some("claude-future-5-0-20260601".to_string()),
            })
            .await;

        // Dynamic cache hit
        assert_eq!(
            anthropic_to_bedrock("claude-future-5-0-20260601", "us", Some(&cache)),
            "us.anthropic.claude-future-5-0-v1"
        );

        // Hardcoded still works for known models via fallback
        assert_eq!(
            anthropic_to_bedrock("claude-sonnet-4-6-20250514", "us", Some(&cache)),
            "us.anthropic.claude-sonnet-4-6"
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

    // --- Beta filter tests ---

    #[test]
    fn test_beta_filtering_allowlist() {
        let betas = filter_betas(
            "interleaved-thinking-2025-05-14,advanced-tool-use-2025-11-20,claude-code-20250219",
        );
        assert_eq!(betas, vec!["interleaved-thinking-2025-05-14"]);
    }

    #[test]
    fn test_strips_all_unknown_betas() {
        let betas =
            filter_betas("claude-code-20250219,adaptive-thinking-2026-01-28,effort-2025-11-24");
        assert!(betas.is_empty());
    }

    #[test]
    fn test_allows_known_betas() {
        let betas = filter_betas("interleaved-thinking-2025-05-14,context-1m-2025-08-07");
        assert_eq!(
            betas,
            vec!["interleaved-thinking-2025-05-14", "context-1m-2025-08-07"]
        );
    }
}
