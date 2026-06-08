/// Conservative canonicalizer for client-supplied model IDs.
///
/// Returns `Some(canonical)` when the input can be coerced into a canonical
/// shape, else `None`. Pipeline: trim whitespace, auto-prepend `claude-`
/// if missing, strip trailing `-YYYYMMDD`, reject any character outside
/// `[a-zA-Z0-9.:-]`, reject empty / `claude-` (empty family).
/// Case is preserved.
pub fn canonicalize_model_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Allowed character set: [a-zA-Z0-9.:-]. Underscore is excluded per the
    // corpus row `claude_sonnet_4_6` → None (corpus is authoritative).
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-'))
    {
        return None;
    }
    // Auto-prefix `claude-` if absent.
    let with_prefix: std::borrow::Cow<'_, str> = if trimmed.starts_with("claude-") {
        std::borrow::Cow::Borrowed(trimmed)
    } else {
        std::borrow::Cow::Owned(format!("claude-{trimmed}"))
    };
    // Reject empty family (`claude-` with nothing after).
    if with_prefix.as_ref() == "claude-" {
        return None;
    }
    // Date-strip via existing helper.
    let stripped = crate::translate::models::strip_date_suffix(with_prefix.as_ref());
    Some(stripped.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_already_canonical() {
        assert_eq!(
            canonicalize_model_id("claude-sonnet-4-6"),
            Some("claude-sonnet-4-6".to_string()),
        );
    }
}
