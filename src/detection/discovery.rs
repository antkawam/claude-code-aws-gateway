use serde_json::json;
use std::collections::HashMap;

use crate::translate::request::AnthropicRequest;

use super::{DetectionFlag, Severity};

/// Detect unnecessary/repeated codebase exploration patterns.
pub fn detect(req: &AnthropicRequest) -> Vec<DetectionFlag> {
    let mut flags = Vec::new();

    let mut file_paths: HashMap<String, usize> = HashMap::new();
    let mut discovery_count: usize = 0;
    let mut action_count: usize = 0;
    let mut broad_globs: Vec<String> = Vec::new();

    for msg in &req.messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role != "assistant" {
            continue;
        }
        let content = match msg.get("content").and_then(|c| c.as_array()) {
            Some(c) => c,
            None => continue,
        };

        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = block.get("input");

            match name {
                "Read" => {
                    discovery_count += 1;
                    if let Some(path) = input
                        .and_then(|i| i.get("file_path"))
                        .and_then(|p| p.as_str())
                    {
                        *file_paths.entry(path.to_string()).or_default() += 1;
                    }
                }
                "Glob" => {
                    discovery_count += 1;
                    if let Some(pattern) = input
                        .and_then(|i| i.get("pattern"))
                        .and_then(|p| p.as_str())
                        && is_broad_glob(pattern)
                    {
                        broad_globs.push(pattern.to_string());
                    }
                }
                "Grep" => {
                    discovery_count += 1;
                }
                "Edit" | "Write" => {
                    action_count += 1;
                }
                _ => {}
            }
        }
    }

    // Rule: repeated_read_paths
    let repeated: Vec<String> = file_paths
        .iter()
        .filter(|(_, count)| **count >= 2)
        .map(|(path, _)| path.clone())
        .collect();
    if !repeated.is_empty() {
        flags.push(DetectionFlag {
            category: "discovery".to_string(),
            rule: "repeated_read_paths".to_string(),
            severity: Severity::Info,
            evidence: json!({
                "repeated_paths": repeated,
            }),
        });
    }

    // Rule: high_discovery_ratio
    let turn_count = req.messages.len();
    if turn_count > 10 && action_count > 0 {
        let ratio = discovery_count as f64 / action_count as f64;
        if ratio > 5.0 {
            flags.push(DetectionFlag {
                category: "discovery".to_string(),
                rule: "high_discovery_ratio".to_string(),
                severity: Severity::Warning,
                evidence: json!({
                    "discovery_count": discovery_count,
                    "action_count": action_count,
                    "ratio": (ratio * 10.0).round() / 10.0,
                }),
            });
        }
    } else if turn_count > 10 && discovery_count > 5 && action_count == 0 {
        // All discovery, no actions
        flags.push(DetectionFlag {
            category: "discovery".to_string(),
            rule: "high_discovery_ratio".to_string(),
            severity: Severity::Warning,
            evidence: json!({
                "discovery_count": discovery_count,
                "action_count": 0,
                "ratio": "infinite",
            }),
        });
    }

    // Rule: broad_glob_patterns
    if !broad_globs.is_empty() {
        flags.push(DetectionFlag {
            category: "discovery".to_string(),
            rule: "broad_glob_patterns".to_string(),
            severity: Severity::Info,
            evidence: json!({
                "patterns": broad_globs,
            }),
        });
    }

    flags
}

fn is_broad_glob(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    trimmed == "**/*"
        || trimmed == "**/.*"
        || trimmed == "**/**"
        || trimmed == "*"
        || trimmed == "**"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn make_request(messages: Vec<Value>) -> AnthropicRequest {
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

    fn assistant_tool(name: &str, input: Value) -> Value {
        json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": name, "input": input}]
        })
    }

    fn user_result() -> Value {
        json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]
        })
    }

    #[test]
    fn repeated_read_paths_detected() {
        let req = make_request(vec![
            assistant_tool("Read", json!({"file_path": "/src/main.rs"})),
            user_result(),
            assistant_tool("Read", json!({"file_path": "/src/main.rs"})),
            user_result(),
        ]);
        let flags = detect(&req);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].rule, "repeated_read_paths");
        let paths = flags[0].evidence["repeated_paths"].as_array().unwrap();
        assert!(paths.contains(&json!("/src/main.rs")));
    }

    #[test]
    fn no_repeated_reads_no_flag() {
        let req = make_request(vec![
            assistant_tool("Read", json!({"file_path": "/src/main.rs"})),
            user_result(),
            assistant_tool("Read", json!({"file_path": "/src/lib.rs"})),
            user_result(),
        ]);
        let flags = detect(&req);
        assert!(flags.is_empty());
    }

    #[test]
    fn high_discovery_ratio() {
        // 12 messages (> 10 threshold), 6 Reads, 1 Edit => ratio 6:1
        let mut msgs = Vec::new();
        for _ in 0..6 {
            msgs.push(assistant_tool("Read", json!({"file_path": "/a.rs"})));
            msgs.push(user_result());
        }
        // Change last read to different path so we don't just test repeated_read_paths
        msgs.push(assistant_tool("Edit", json!({"file_path": "/a.rs"})));
        msgs.push(user_result());

        let req = make_request(msgs);
        let flags = detect(&req);
        let ratio_flag = flags.iter().find(|f| f.rule == "high_discovery_ratio");
        assert!(ratio_flag.is_some(), "should flag high discovery ratio");
    }

    #[test]
    fn broad_glob_detected() {
        let req = make_request(vec![
            assistant_tool("Glob", json!({"pattern": "**/*"})),
            user_result(),
        ]);
        let flags = detect(&req);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].rule, "broad_glob_patterns");
    }

    #[test]
    fn specific_glob_not_flagged() {
        let req = make_request(vec![
            assistant_tool("Glob", json!({"pattern": "src/**/*.rs"})),
            user_result(),
        ]);
        let flags = detect(&req);
        assert!(flags.is_empty());
    }

    #[test]
    fn no_flags_on_balanced_usage() {
        // Under 10 messages, balanced read/edit
        let req = make_request(vec![
            assistant_tool("Read", json!({"file_path": "/a.rs"})),
            user_result(),
            assistant_tool("Edit", json!({"file_path": "/a.rs"})),
            user_result(),
        ]);
        let flags = detect(&req);
        assert!(flags.is_empty());
    }
}
