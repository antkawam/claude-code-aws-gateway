use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::translate::request::AnthropicRequest;

mod corrections;
mod dangerous;
mod discovery;
mod secrets;

/// Severity of a detection flag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// A single detection flag raised by a rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionFlag {
    pub category: String,
    pub rule: String,
    pub severity: Severity,
    pub evidence: Value,
}

/// Run all detection categories against a request. Pure function, no IO.
pub fn detect(req: &AnthropicRequest) -> Vec<DetectionFlag> {
    let mut flags = Vec::new();
    flags.extend(discovery::detect(req));
    flags.extend(corrections::detect(req));
    flags.extend(secrets::detect(req));
    flags.extend(dangerous::detect(req));
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_request(messages: Vec<Value>) -> AnthropicRequest {
        AnthropicRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: Some(4096),
            messages,
            system: None,
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
    fn empty_request_no_flags() {
        let req = minimal_request(vec![]);
        let flags = detect(&req);
        assert!(flags.is_empty());
    }

    /// Load a real CC session JSONL and run detection on it.
    /// Set CC_SESSION_JSONL env var to point at a session file, e.g.:
    ///   CC_SESSION_JSONL=~/.claude/projects/.../session.jsonl cargo test --lib detection::tests::detect_real_session -- --nocapture
    #[test]
    fn detect_real_session() {
        let path = match std::env::var("CC_SESSION_JSONL") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("CC_SESSION_JSONL not set, skipping real session test");
                return;
            }
        };

        let data = std::fs::read_to_string(&path).expect("Failed to read session JSONL");
        let mut messages = Vec::new();
        for line in data.lines() {
            let obj: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if msg_type != "user" && msg_type != "assistant" {
                continue;
            }
            let msg = match obj.get("message") {
                Some(m) => m,
                None => continue,
            };
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let content = msg.get("content");
            if role.is_empty() || content.is_none() {
                continue;
            }
            messages.push(serde_json::json!({
                "role": role,
                "content": content.unwrap()
            }));
        }

        eprintln!("\n=== Real Session Detection ===");
        eprintln!("File: {}", path);
        eprintln!("Messages: {}", messages.len());

        let req = minimal_request(messages);
        let flags = detect(&req);

        eprintln!("Flags raised: {}\n", flags.len());
        for flag in &flags {
            eprintln!(
                "  [{:?}] {}/{}: {}",
                flag.severity, flag.category, flag.rule, flag.evidence
            );
        }
        eprintln!();

        // No assertion — this is observational
    }

    #[test]
    fn detect_returns_flags_from_all_categories() {
        // Request with: repeated reads, negation, AWS key, and destructive command
        let req = minimal_request(vec![
            // Turn 1: assistant reads a file
            json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/src/main.rs"}},
                ]
            }),
            // Turn 2: user returns result
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}"}
                ]
            }),
            // Turn 3: assistant reads same file again
            json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "/src/main.rs"}},
                ]
            }),
            // Turn 4: user returns result
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t2", "content": "fn main() {}"}
                ]
            }),
            // Turn 5: assistant does something
            json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t3", "name": "Bash", "input": {"command": "git push --force origin main"}},
                ]
            }),
            // Turn 6: user says "no wrong"
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "no that's wrong, don't do that"}
                ]
            }),
        ]);

        let flags = detect(&req);
        let categories: Vec<&str> = flags.iter().map(|f| f.category.as_str()).collect();
        assert!(
            categories.contains(&"discovery"),
            "should detect repeated reads"
        );
        assert!(categories.contains(&"correction"), "should detect negation");
        assert!(
            categories.contains(&"dangerous"),
            "should detect git destructive"
        );
    }
}
