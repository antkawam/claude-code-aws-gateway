//! Small string helpers.
//!
//! `str` indices are bytes, not characters: `&s[..n]` panics whenever byte `n`
//! lands inside a multi-byte character. The gateway cuts user-, model- and
//! web-supplied text down to preview lengths in several places; route those cuts
//! through here.

/// Truncate `s` to at most `max_bytes`, always cutting on a character boundary.
///
/// Rounds the cut down to the nearest boundary and clamps to the string's length,
/// so it is safe for any input and any `max_bytes`. The result can therefore be up
/// to 3 bytes shorter than `max_bytes`.
pub fn truncate_bytes(s: &str, max_bytes: usize) -> &str {
    &s[..s.floor_char_boundary(max_bytes)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorter_than_limit_is_unchanged() {
        assert_eq!(truncate_bytes("ENOENT", 100), "ENOENT");
        assert_eq!(truncate_bytes("", 100), "");
    }

    #[test]
    fn ascii_cuts_exactly_at_the_limit() {
        let s = "e".repeat(150);
        assert_eq!(truncate_bytes(&s, 100).len(), 100);
        assert_eq!(truncate_bytes(&s, 150).len(), 150);
    }

    #[test]
    fn multi_byte_char_straddling_the_limit_is_dropped_whole() {
        // U+202F occupies bytes 98..101.
        let s = format!("{}\u{202f}tail", "e".repeat(98));
        assert_eq!(truncate_bytes(&s, 100), "e".repeat(98));
    }

    #[test]
    fn four_byte_char_straddling_the_limit_is_dropped_whole() {
        // A non-BMP emoji is 4 bytes, so every offset inside it must round down.
        let s = format!("{}\u{1f4a5}tail", "e".repeat(98));
        for limit in 99..=101 {
            assert_eq!(truncate_bytes(&s, limit), "e".repeat(98));
        }
        assert_eq!(
            truncate_bytes(&s, 102),
            format!("{}\u{1f4a5}", "e".repeat(98))
        );
    }

    #[test]
    fn limit_inside_the_very_first_char_yields_empty() {
        assert_eq!(truncate_bytes("\u{2605}abc", 1), "");
        assert_eq!(truncate_bytes("abc", 0), "");
    }

    #[test]
    fn limit_past_the_end_yields_the_whole_string() {
        assert_eq!(
            truncate_bytes("\u{2605}\u{2605}", usize::MAX),
            "\u{2605}\u{2605}"
        );
    }
}
