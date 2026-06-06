//! Cache-aware beta filtering and Bedrock ValidationException parsing.

use crate::endpoint::EndpointClient;

/// Extract beta names from a Bedrock `ValidationException` error message.
///
/// Scans `error_message` for any substring in `candidates` (case-insensitive).
/// Returns the matched candidates — these are the betas Bedrock rejected.
///
/// Intentionally uses substring search rather than a regex so the logic is
/// robust to minor Bedrock error-message format drift.
pub fn parse_rejected_betas(error_message: &str, candidates: &[String]) -> Vec<String> {
    let lower = error_message.to_lowercase();
    candidates
        .iter()
        .filter(|c| lower.contains(&c.to_lowercase()))
        .cloned()
        .collect()
}

/// Result of filtering a beta list through the per-endpoint capability cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BetaFilterResult {
    /// Betas to include in the forwarded request (cache says supported OR unknown).
    pub kept: Vec<String>,
    /// Betas dropped because the cache recorded `Some(false)` for this endpoint+profile.
    pub dropped: Vec<String>,
    /// Subset of `kept` whose cache state was `None` (absent or expired) at filter time.
    /// On a successful response these should be opportunistically marked as supported.
    pub unknown: Vec<String>,
}

/// Filter `incoming` betas through the per-endpoint capability cache.
///
/// - `Some(true)`  → kept, not unknown
/// - `Some(false)` → dropped
/// - `None`        → kept AND placed in `unknown` (optimistic; learn on success)
pub async fn filter_betas_by_cache(
    client: &EndpointClient,
    profile: &str,
    incoming: &[String],
) -> BetaFilterResult {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut unknown = Vec::new();

    for b in incoming {
        match client.is_beta_supported(profile, b).await {
            Some(true) => kept.push(b.clone()),
            Some(false) => dropped.push(b.clone()),
            None => {
                kept.push(b.clone());
                unknown.push(b.clone());
            }
        }
    }

    BetaFilterResult {
        kept,
        dropped,
        unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<String> {
        vec![
            "context-1m-2025-08-07".to_string(),
            "interleaved-thinking-2025-05-14".to_string(),
            "claude-code-x".to_string(),
        ]
    }

    // ── parse_rejected_betas ──────────────────────────────────────────────────

    #[test]
    fn parse_empty_message_returns_empty() {
        let result = parse_rejected_betas("", &candidates());
        assert!(result.is_empty());
    }

    #[test]
    fn parse_no_match_returns_empty() {
        let result = parse_rejected_betas(
            "The request is invalid for some unrelated reason.",
            &candidates(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn parse_single_beta_named_in_error() {
        let msg =
            "ValidationException: The beta feature 'claude-code-x' is not supported on this model.";
        let result = parse_rejected_betas(msg, &candidates());
        assert_eq!(result, vec!["claude-code-x".to_string()]);
    }

    #[test]
    fn parse_multiple_betas_named_in_error() {
        let msg = "ValidationException: features context-1m-2025-08-07 and claude-code-x are not supported.";
        let mut result = parse_rejected_betas(msg, &candidates());
        result.sort();
        let mut expected = vec![
            "context-1m-2025-08-07".to_string(),
            "claude-code-x".to_string(),
        ];
        expected.sort();
        assert_eq!(result, expected);
    }

    #[test]
    fn parse_case_insensitive_match() {
        // Bedrock might capitalise or alter casing in error messages.
        let msg = "Unsupported beta: CONTEXT-1M-2025-08-07";
        let result = parse_rejected_betas(msg, &candidates());
        assert_eq!(result, vec!["context-1m-2025-08-07".to_string()]);
    }

    #[test]
    fn parse_empty_candidates_returns_empty() {
        let result = parse_rejected_betas("context-1m-2025-08-07 is not supported", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_does_not_duplicate_when_beta_appears_twice_in_message() {
        // The beta appears twice but candidates only contains it once — result should have one entry.
        let msg = "context-1m-2025-08-07 context-1m-2025-08-07 rejected";
        let result = parse_rejected_betas(msg, &candidates());
        assert_eq!(result, vec!["context-1m-2025-08-07".to_string()]);
    }
}
