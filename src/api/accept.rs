use crate::translate::canonicalize::canonicalize_model_id;
use crate::translate::models::ModelCache;

/// The result of the three-tier model acceptance pipeline.
#[derive(Debug, Clone)]
pub enum ModelAcceptance {
    /// The raw model ID had a direct exact-match pin row in the cache.
    PinHit {
        suffix: String,
        anthropic_prefix: String,
    },
    /// The raw model ID had no exact pin row, but after canonicalization the
    /// canonical form was found in the cache.
    CanonicalHit {
        suffix: String,
        canonical: String,
        anthropic_prefix: String,
    },
    /// Neither pin nor canonical hit; a dynamic discovery call found the model.
    Discovered {
        suffix: String,
        canonical: String,
        profile_prefix: String,
        via: &'static str,
        anthropic_prefix: String,
    },
    /// All three tiers failed; the model ID is not usable.
    Reject,
}

/// Three-tier model acceptance pipeline.
///
/// Given a raw client-supplied model ID:
/// 1. Try an exact cache lookup (`lookup_forward`). This catches explicit pin
///    rows, including admin-added aliases for non-canonical keys.
/// 2. Canonicalize via `canonicalize_model_id` (trim, strip date suffix,
///    auto-prepend `claude-`). If the canonical form differs from the raw
///    input, try the cache again with an exact lookup.
/// 3. Call the async `discover_fn` closure against the canonical form (or the
///    raw form when canonicalization failed).
/// 4. Return `Reject` when all three tiers miss.
///
/// The discovery closure signature mirrors `discover_model`:
/// `(anthropic_prefix, bedrock_suffix, anthropic_display, profile_prefix, via)`.
pub async fn accept_model<F, Fut>(raw: &str, cache: &ModelCache, discover_fn: F) -> ModelAcceptance
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Option<(String, String, Option<String>, String, &'static str)>>,
{
    // Tier 1: exact raw pin. Uses lookup_forward (no date-strip fallback) so
    // that date-stripped matches are attributed to the canonicalize path in
    // Tier 2 (CanonicalHit) rather than a raw pin (PinHit). Admin-added alias
    // rows with non-canonical keys (e.g. "Sonnet 4.7") still fire here because
    // the key is stored verbatim in the cache.
    if let Some(suffix) = cache.lookup_forward(raw) {
        return ModelAcceptance::PinHit {
            suffix,
            anthropic_prefix: raw.to_string(),
        };
    }

    // Tier 2: canonicalize, then try the cache on the canonical form.
    // If canonicalization returns None we skip the cache re-check and fall
    // directly to discovery (Tier 3) using the raw string.
    let canonical = canonicalize_model_id(raw);

    if let Some(ref c) = canonical {
        // Only re-check if the canonical form is actually different from the
        // raw input; otherwise we'd just repeat the miss from Tier 1.
        if c != raw
            && let Some(suffix) = cache.lookup_forward(c)
        {
            return ModelAcceptance::CanonicalHit {
                suffix,
                canonical: c.clone(),
                anthropic_prefix: raw.to_string(),
            };
        }
    }

    // Tier 3: dynamic discovery.  Pass the canonical form when available,
    // otherwise fall back to the raw string so the caller still has a chance
    // to discover via ListInferenceProfiles.
    let discover_input = canonical.clone().unwrap_or_else(|| raw.to_string());

    if let Some((anthropic_prefix, suffix, _display, profile_prefix, via)) =
        discover_fn(discover_input).await
    {
        let canonical_str = canonical.unwrap_or_else(|| raw.to_string());
        return ModelAcceptance::Discovered {
            suffix,
            canonical: canonical_str,
            profile_prefix,
            via,
            anthropic_prefix,
        };
    }

    ModelAcceptance::Reject
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::models::{CachedMapping, ModelCache};

    /// Smoke test: empty cache + discovery returning None → Reject.
    #[tokio::test]
    async fn smoke_reject_on_empty_cache_and_no_discovery() {
        let cache = ModelCache::new();
        let result = accept_model("made-up-model", &cache, |_| std::future::ready(None)).await;
        assert!(
            matches!(result, ModelAcceptance::Reject),
            "Expected Reject with empty cache and None discovery"
        );
    }

    /// Smoke test: cache hit on exact key → PinHit.
    #[tokio::test]
    async fn smoke_pin_hit() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-6".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-6".to_string(),
                anthropic_display: None,
            })
            .await;

        let result = accept_model("claude-sonnet-4-6", &cache, |_| {
            panic!("discovery should not be called");
            #[allow(unreachable_code)]
            std::future::ready(None::<(String, String, Option<String>, String, &'static str)>)
        })
        .await;

        assert!(
            matches!(result, ModelAcceptance::PinHit { .. }),
            "Expected PinHit for exact cache entry"
        );
    }

    /// Smoke test: dated variant with only canonical form in cache → CanonicalHit.
    #[tokio::test]
    async fn smoke_canonical_hit_dated_variant() {
        let cache = ModelCache::new();
        cache
            .insert(CachedMapping {
                anthropic_prefix: "claude-sonnet-4-6".to_string(),
                bedrock_suffix: "anthropic.claude-sonnet-4-6".to_string(),
                anthropic_display: None,
            })
            .await;

        let result = accept_model("claude-sonnet-4-6-20250514", &cache, |_| {
            panic!("discovery should not be called");
            #[allow(unreachable_code)]
            std::future::ready(None::<(String, String, Option<String>, String, &'static str)>)
        })
        .await;

        assert!(
            matches!(result, ModelAcceptance::CanonicalHit { .. }),
            "Expected CanonicalHit when only canonical form is in cache"
        );
    }
}
