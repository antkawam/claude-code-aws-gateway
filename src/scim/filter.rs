/// Parse SCIM filter expressions (RFC 7644 Section 3.4.2.2).
///
/// Supported grammar subset:
/// ```text
/// filter     = attrExpr / filter SP "and" SP filter
/// attrExpr   = attrPath SP compareOp SP compValue
/// compareOp  = "eq" / "co" / "sw"
/// compValue  = DQUOTE *CHAR DQUOTE / "true" / "false"
/// ```
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum ScimFilter {
    /// Exact equality: `attribute eq "value"` or `attribute eq true`
    Eq(String, String),
    /// Contains substring: `attribute co "value"`
    Contains(String, String),
    /// Starts with: `attribute sw "value"`
    StartsWith(String, String),
    /// Logical AND of two filters.
    And(Box<ScimFilter>, Box<ScimFilter>),
}

impl fmt::Display for ScimFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eq(attr, val) => write!(f, "{attr} eq \"{val}\""),
            Self::Contains(attr, val) => write!(f, "{attr} co \"{val}\""),
            Self::StartsWith(attr, val) => write!(f, "{attr} sw \"{val}\""),
            Self::And(left, right) => write!(f, "({left}) and ({right})"),
        }
    }
}

/// Parse a SCIM filter string into a `ScimFilter`.
///
/// Returns `Err` with a human-readable message on invalid input.
pub fn parse_filter(input: &str) -> Result<ScimFilter, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Filter expression is empty".to_string());
    }

    // Try splitting on " and " (case-insensitive).
    // We scan left-to-right for the first occurrence not inside quotes.
    if let Some((left, right)) = split_and(input) {
        let left_filter = parse_filter(left.trim())?;
        let right_filter = parse_filter(right.trim())?;
        return Ok(ScimFilter::And(
            Box::new(left_filter),
            Box::new(right_filter),
        ));
    }

    parse_attr_expr(input)
}

/// Split `input` on the first ` and ` (case-insensitive) that is not inside quotes.
fn split_and(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut in_quotes = false;

    while i < len {
        if bytes[i] == b'"' {
            in_quotes = !in_quotes;
            i += 1;
            continue;
        }
        if !in_quotes {
            // Check for " and " (5 bytes: space + a + n + d + space)
            // We need at least 5 chars remaining starting at i.
            if i + 5 <= len {
                let candidate = &input[i..i + 5];
                if candidate.eq_ignore_ascii_case(" and ") {
                    return Some((&input[..i], &input[i + 5..]));
                }
            }
        }
        i += 1;
    }
    None
}

/// Parse a single `attrPath compareOp compValue` expression.
fn parse_attr_expr(input: &str) -> Result<ScimFilter, String> {
    // Tokenize: attrPath  compareOp  compValue
    // Use split_whitespace to handle runs of whitespace, but we need at most 3 tokens
    // and the third token (compValue) may contain spaces (e.g., quoted strings).
    // Strategy: skip leading whitespace, grab attr, skip whitespace, grab op,
    // skip whitespace, everything remaining is the compValue.
    let input = input.trim();

    // Find the first whitespace boundary to extract `attr`.
    let attr_end = input
        .find(char::is_whitespace)
        .ok_or("Missing operator: expected 'attrPath op value'")?;
    let attr = &input[..attr_end];
    if attr.is_empty() {
        return Err("Attribute name is empty".to_string());
    }

    // Skip whitespace after attr.
    let rest = input[attr_end..].trim_start();

    // Find the next whitespace boundary to extract `op`.
    let op_end = rest
        .find(char::is_whitespace)
        .ok_or_else(|| format!("Missing value: expected 'op value' after attribute '{attr}'"))?;
    let op = &rest[..op_end];

    // Skip whitespace after op; the rest is the compValue.
    let value_part = rest[op_end..].trim();
    if value_part.is_empty() {
        return Err(format!(
            "Missing value after operator '{op}' for attribute '{attr}'"
        ));
    }

    let value = parse_comp_value(value_part)?;

    match op.to_lowercase().as_str() {
        "eq" => Ok(ScimFilter::Eq(attr.to_string(), value)),
        "co" => Ok(ScimFilter::Contains(attr.to_string(), value)),
        "sw" => Ok(ScimFilter::StartsWith(attr.to_string(), value)),
        other => Err(format!(
            "Unknown operator '{other}'. Supported operators: eq, co, sw"
        )),
    }
}

/// Parse a comparison value: either a quoted string or an unquoted boolean.
fn parse_comp_value(input: &str) -> Result<String, String> {
    let input = input.trim();

    // Unquoted booleans.
    if input.eq_ignore_ascii_case("true") {
        return Ok("true".to_string());
    }
    if input.eq_ignore_ascii_case("false") {
        return Ok("false".to_string());
    }

    // Quoted string: must start and end with `"`.
    if input.starts_with('"') {
        if !input.ends_with('"') || input.len() < 2 {
            return Err(format!("Unterminated string literal: {input}"));
        }
        // Strip surrounding quotes and handle escaped quotes inside.
        let inner = &input[1..input.len() - 1];
        let unescaped = inner.replace("\\\"", "\"");
        return Ok(unescaped);
    }

    Err(format!(
        "Invalid comparison value '{input}'. Expected quoted string or boolean"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_eq_string() {
        let f = parse_filter(r#"userName eq "alice@example.com""#).unwrap();
        assert_eq!(
            f,
            ScimFilter::Eq("userName".to_string(), "alice@example.com".to_string())
        );
    }

    #[test]
    fn parse_eq_boolean_true() {
        let f = parse_filter("active eq true").unwrap();
        assert_eq!(f, ScimFilter::Eq("active".to_string(), "true".to_string()));
    }

    #[test]
    fn parse_eq_boolean_false() {
        let f = parse_filter("active eq false").unwrap();
        assert_eq!(f, ScimFilter::Eq("active".to_string(), "false".to_string()));
    }

    #[test]
    fn parse_contains() {
        let f = parse_filter(r#"userName co "alice""#).unwrap();
        assert_eq!(
            f,
            ScimFilter::Contains("userName".to_string(), "alice".to_string())
        );
    }

    #[test]
    fn parse_starts_with() {
        let f = parse_filter(r#"userName sw "alice""#).unwrap();
        assert_eq!(
            f,
            ScimFilter::StartsWith("userName".to_string(), "alice".to_string())
        );
    }

    #[test]
    fn parse_operator_case_insensitive() {
        let f = parse_filter(r#"userName EQ "alice""#).unwrap();
        assert!(matches!(f, ScimFilter::Eq(_, _)));

        let f2 = parse_filter(r#"userName CO "alice""#).unwrap();
        assert!(matches!(f2, ScimFilter::Contains(_, _)));
    }

    #[test]
    fn parse_and_filter() {
        let f = parse_filter(r#"userName eq "alice@example.com" and active eq true"#).unwrap();
        match f {
            ScimFilter::And(left, right) => {
                assert_eq!(
                    *left,
                    ScimFilter::Eq("userName".to_string(), "alice@example.com".to_string())
                );
                assert_eq!(
                    *right,
                    ScimFilter::Eq("active".to_string(), "true".to_string())
                );
            }
            _ => panic!("Expected And filter"),
        }
    }

    #[test]
    fn parse_and_case_insensitive() {
        let f = parse_filter(r#"userName eq "alice" AND active eq true"#).unwrap();
        assert!(matches!(f, ScimFilter::And(_, _)));
    }

    #[test]
    fn parse_empty_returns_error() {
        assert!(parse_filter("").is_err());
        assert!(parse_filter("   ").is_err());
    }

    #[test]
    fn parse_unknown_operator_returns_error() {
        let err = parse_filter(r#"userName ne "alice""#).unwrap_err();
        assert!(err.contains("ne"));
    }

    #[test]
    fn parse_unterminated_string_returns_error() {
        let err = parse_filter(r#"userName eq "alice"#).unwrap_err();
        assert!(err.contains("Unterminated"));
    }

    #[test]
    fn parse_quoted_value_with_escaped_quotes() {
        let f = parse_filter(r#"displayName eq "O\"Brien""#).unwrap();
        assert_eq!(
            f,
            ScimFilter::Eq("displayName".to_string(), r#"O"Brien"#.to_string())
        );
    }

    #[test]
    fn split_and_not_in_quotes() {
        // The " and " inside a quoted value should not split.
        let input = r#"userName eq "alice and bob""#;
        assert!(split_and(input).is_none());
    }

    #[test]
    fn parse_trims_whitespace() {
        let f = parse_filter(r#"  userName  eq  "alice"  "#);
        // splitn(3) on whitespace handles extra spaces around op, but the value might have trailing space.
        // This is a best-effort trim.
        assert!(f.is_ok());
    }

    // --- Additional edge-case tests (from test-agent) ---

    /// Missing value after operator (only attr + op, no value) returns an error.
    #[test]
    fn parse_invalid_filter_no_value() {
        let err = parse_filter("userName eq").unwrap_err();
        assert!(
            !err.is_empty(),
            "Should produce an error message when value is missing after operator"
        );
    }

    /// Extra internal spaces between tokens still parse correctly.
    #[test]
    fn parse_filter_extra_spaces() {
        let f = parse_filter(r#"userName  eq  "alice""#).unwrap();
        assert_eq!(
            f,
            ScimFilter::Eq("userName".to_string(), "alice".to_string())
        );
    }

    /// Unknown operator returns error with the operator name in the message.
    #[test]
    fn parse_invalid_filter_unknown_op_detail() {
        let err = parse_filter(r#"userName xx "alice""#).unwrap_err();
        assert!(
            err.contains("xx"),
            "Error message should name the unknown operator"
        );
    }
}
// end #[cfg(test)]
