use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

use crate::translate::request::AnthropicRequest;

use super::{DetectionFlag, Severity};

struct SecretRule {
    name: &'static str,
    pattern: &'static LazyLock<Regex>,
    severity: Severity,
}

static AWS_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").unwrap());

static GENERIC_SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(key|token|secret|password|api.key)\s*[=:]\s*['"][A-Za-z0-9_+/=\-]{20,}"#)
        .unwrap()
});

static PRIVATE_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----").unwrap());

static CONNECTION_STRING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(postgres|mysql|mongodb|redis)://[^@]+@").unwrap());

const RULES: &[SecretRule] = &[
    SecretRule {
        name: "aws_key",
        pattern: &AWS_KEY_RE,
        severity: Severity::Critical,
    },
    SecretRule {
        name: "generic_secret",
        pattern: &GENERIC_SECRET_RE,
        severity: Severity::Critical,
    },
    SecretRule {
        name: "private_key",
        pattern: &PRIVATE_KEY_RE,
        severity: Severity::Critical,
    },
    SecretRule {
        name: "connection_string",
        pattern: &CONNECTION_STRING_RE,
        severity: Severity::Warning,
    },
];

/// Match type + location for a secret detection. NEVER stores the actual secret.
#[derive(Debug)]
struct SecretMatch {
    rule_name: String,
    location: String,
    count: usize,
}

/// Detect credentials/PII in plaintext.
pub fn detect(req: &AnthropicRequest) -> Vec<DetectionFlag> {
    let mut matches: Vec<SecretMatch> = Vec::new();

    // Scan system prompt
    if let Some(system) = &req.system {
        let texts = extract_text_from_value(system);
        for text in &texts {
            scan_text(text, "system_prompt", &mut matches);
        }
    }

    // Scan messages
    for msg in &req.messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let location = match role {
            "user" => "user_message",
            "assistant" => "assistant_message",
            _ => "message",
        };

        if let Some(content) = msg.get("content") {
            match content {
                Value::String(s) => {
                    scan_text(s, location, &mut matches);
                }
                Value::Array(blocks) => {
                    for block in blocks {
                        let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match bt {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    scan_text(text, location, &mut matches);
                                }
                            }
                            "tool_result" => {
                                if let Some(text) = block.get("content").and_then(|c| c.as_str()) {
                                    scan_text(text, "tool_result", &mut matches);
                                }
                                // tool_result content can also be an array of blocks
                                if let Some(arr) = block.get("content").and_then(|c| c.as_array()) {
                                    for sub in arr {
                                        if let Some(text) = sub.get("text").and_then(|t| t.as_str())
                                        {
                                            scan_text(text, "tool_result", &mut matches);
                                        }
                                    }
                                }
                            }
                            "tool_use" => {
                                // Scan tool input for secrets
                                if let Some(input) = block.get("input") {
                                    let texts = extract_text_from_value(input);
                                    for text in &texts {
                                        scan_text(text, "tool_input", &mut matches);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if matches.is_empty() {
        return Vec::new();
    }

    // Group by rule name
    let mut grouped: std::collections::HashMap<String, Vec<&SecretMatch>> =
        std::collections::HashMap::new();
    for m in &matches {
        grouped.entry(m.rule_name.clone()).or_default().push(m);
    }

    let match_entries: Vec<Value> = grouped
        .iter()
        .map(|(rule, ms)| {
            let total: usize = ms.iter().map(|m| m.count).sum();
            let locations: Vec<&str> = ms.iter().map(|m| m.location.as_str()).collect();
            json!({
                "type": rule,
                "location": locations.first().unwrap_or(&"unknown"),
                "count": total,
            })
        })
        .collect();

    // Use highest severity found
    let severity = if matches.iter().any(|m| {
        RULES
            .iter()
            .any(|r| r.name == m.rule_name && r.severity == Severity::Critical)
    }) {
        Severity::Critical
    } else {
        Severity::Warning
    };

    vec![DetectionFlag {
        category: "secrets".to_string(),
        rule: grouped
            .keys()
            .next()
            .unwrap_or(&"unknown".to_string())
            .clone(),
        severity,
        evidence: json!({ "matches": match_entries }),
    }]
}

fn scan_text(text: &str, location: &str, matches: &mut Vec<SecretMatch>) {
    for rule in RULES {
        let count = rule.pattern.find_iter(text).count();
        if count > 0 {
            matches.push(SecretMatch {
                rule_name: rule.name.to_string(),
                location: location.to_string(),
                count,
            });
        }
    }
}

/// Recursively extract all string values from a JSON value.
fn extract_text_from_value(value: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    match value {
        Value::String(s) => texts.push(s.clone()),
        Value::Array(arr) => {
            for item in arr {
                texts.extend(extract_text_from_value(item));
            }
        }
        Value::Object(obj) => {
            for v in obj.values() {
                texts.extend(extract_text_from_value(v));
            }
        }
        _ => {}
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_request(messages: Vec<Value>, system: Option<Value>) -> AnthropicRequest {
        AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: Some(4096),
            messages,
            system,
            stream: None,
            thinking: None,
            tools: None,
            tool_choice: None,
            metadata: None,
            stop_sequences: None,
            temperature: None,
            top_p: None,
            top_k: None,
            mcp_servers: None,
        }
    }

    #[test]
    fn aws_key_in_tool_result() {
        let req = make_request(
            vec![json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "AWS_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE"}
                ]
            })],
            None,
        );
        let flags = detect(&req);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].category, "secrets");
        let matches = flags[0].evidence["matches"].as_array().unwrap();
        assert!(matches.iter().any(|m| m["type"] == "aws_key"));
    }

    #[test]
    fn private_key_in_message() {
        let req = make_request(
            vec![json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Here is a private key I found:\n-----BEGIN RSA PRIV\u{0041}TE KEY-----\ntest\n-----END RSA KEY-----"}
                ]
            })],
            None,
        );
        let flags = detect(&req);
        assert!(!flags.is_empty());
    }

    #[test]
    fn connection_string_detected() {
        let req = make_request(
            vec![json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "postgres://admin:password123@db.example.com/mydb"}}
                ]
            })],
            None,
        );
        let flags = detect(&req);
        assert!(!flags.is_empty());
    }

    #[test]
    fn generic_secret_in_system_prompt() {
        let req = make_request(
            vec![],
            Some(json!(
                "Configure with api_key = 'secret_key_example_0123456789ab'"
            )),
        );
        let flags = detect(&req);
        assert!(!flags.is_empty());
    }

    #[test]
    fn no_secrets_clean_request() {
        let req = make_request(
            vec![json!({
                "role": "user",
                "content": [{"type": "text", "text": "Please help me write a function"}]
            })],
            None,
        );
        let flags = detect(&req);
        assert!(flags.is_empty());
    }

    #[test]
    fn never_stores_actual_secret() {
        let req = make_request(
            vec![json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "AKIAIOSFODNN7EXAMPLE"}
                ]
            })],
            None,
        );
        let flags = detect(&req);
        let evidence_str = serde_json::to_string(&flags[0].evidence).unwrap();
        // Should NOT contain the actual key
        assert!(!evidence_str.contains("AKIAIOSFODNN7EXAMPLE"));
    }
}
