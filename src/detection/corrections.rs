use regex::Regex;
use serde_json::json;
use std::sync::LazyLock;

use crate::translate::request::AnthropicRequest;

use super::{DetectionFlag, Severity};

static NEGATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(no|don't|dont|wrong|not what|instead|stop|undo|revert|shouldn't|actually)\b",
    )
    .unwrap()
});

/// Detect user correction patterns.
pub fn detect(req: &AnthropicRequest) -> Vec<DetectionFlag> {
    let mut flags = Vec::new();

    let mut correction_count: usize = 0;
    let mut negation_keywords: Vec<String> = Vec::new();
    let mut correction_positions: Vec<usize> = Vec::new();
    let mut last_was_assistant_with_tool = false;

    // Track tool calls with error context for repeated_tool_retry
    // Each entry: (tool_name, had_error_in_result)
    let mut tool_error_sequence: Vec<(String, bool)> = Vec::new();
    // Track the last tool_use names per assistant message, to pair with the next user message's results
    let mut pending_tool_names: Vec<String> = Vec::new();

    for (i, msg) in req.messages.iter().enumerate() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = match msg.get("content").and_then(|c| c.as_array()) {
            Some(c) => c,
            None => {
                // Handle string content for user messages
                if role == "user" {
                    if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                        if last_was_assistant_with_tool {
                            for cap in NEGATION_RE.captures_iter(text) {
                                let keyword = cap[1].to_lowercase();
                                if !negation_keywords.contains(&keyword) {
                                    negation_keywords.push(keyword);
                                }
                            }
                            if NEGATION_RE.is_match(text) {
                                correction_count += 1;
                                correction_positions.push(i);
                            }
                        }
                        last_was_assistant_with_tool = false;
                    }
                    continue;
                }
                continue;
            }
        };

        if role == "assistant" {
            let mut has_tool = false;
            pending_tool_names.clear();
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    has_tool = true;
                    if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                        pending_tool_names.push(name.to_string());
                    }
                }
            }
            last_was_assistant_with_tool = has_tool;
        } else if role == "user" {
            let mut has_tool_result = false;
            let mut has_error_result = false;

            for block in content {
                let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if bt == "tool_result" {
                    has_tool_result = true;
                    if block
                        .get("is_error")
                        .and_then(|e| e.as_bool())
                        .unwrap_or(false)
                    {
                        has_error_result = true;
                    }
                }
            }

            // Record tool calls with their error status for retry detection
            if has_tool_result {
                for name in &pending_tool_names {
                    tool_error_sequence.push((name.clone(), has_error_result));
                }
            }

            // negation_after_tool: user text (not tool_result) after assistant tool_use
            // Also check mixed messages (text + tool_result) for negation
            let has_user_text = content
                .iter()
                .any(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"));

            if last_was_assistant_with_tool && has_user_text && !has_tool_result {
                let has_negation = content.iter().any(|block| {
                    block.get("type").and_then(|t| t.as_str()) == Some("text")
                        && block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| NEGATION_RE.is_match(t))
                });
                if has_negation {
                    correction_count += 1;
                    correction_positions.push(i);
                    // Collect keywords
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text")
                            && let Some(text) = block.get("text").and_then(|t| t.as_str())
                        {
                            for cap in NEGATION_RE.captures_iter(text) {
                                let keyword = cap[1].to_lowercase();
                                if !negation_keywords.contains(&keyword) {
                                    negation_keywords.push(keyword);
                                }
                            }
                        }
                    }
                }
            }

            last_was_assistant_with_tool = false;
        }
    }

    // Rule: negation_after_tool
    if !negation_keywords.is_empty() {
        flags.push(DetectionFlag {
            category: "correction".to_string(),
            rule: "negation_after_tool".to_string(),
            severity: Severity::Warning,
            evidence: json!({
                "correction_count": correction_count,
                "negation_keywords": negation_keywords,
                "correction_positions": correction_positions,
            }),
        });
    }

    // Rule: multi_correction
    if correction_count >= 3 {
        flags.push(DetectionFlag {
            category: "correction".to_string(),
            rule: "multi_correction".to_string(),
            severity: Severity::Warning,
            evidence: json!({
                "correction_count": correction_count,
                "correction_positions": correction_positions,
            }),
        });
    }

    // Rule: repeated_tool_retry — same tool called 3+ times in sequence with high error rate.
    // This catches actual retry loops (Edit→Error→Edit→Error→Edit), not normal sequential usage
    // like 20 consecutive Bash calls for AWS ops with occasional errors.
    // Requires: 3+ consecutive calls AND ≥2 errors AND ≥30% error rate within the run.
    if tool_error_sequence.len() >= 3 {
        let mut best_run = 0;
        let mut best_errors = 0;
        let mut current_run = 1;
        let mut current_errors = if tool_error_sequence[0].1 { 1 } else { 0 };
        let mut run_tool = &tool_error_sequence[0].0;
        let mut best_tool = &tool_error_sequence[0].0;

        for entry in &tool_error_sequence[1..] {
            if entry.0 == *run_tool {
                current_run += 1;
                if entry.1 {
                    current_errors += 1;
                }
                // Must have ≥2 errors AND ≥30% error rate to count as a retry loop
                let error_rate = current_errors as f64 / current_run as f64;
                if current_run >= 3
                    && current_errors >= 2
                    && error_rate >= 0.3
                    && current_errors > best_errors
                {
                    best_run = current_run;
                    best_errors = current_errors;
                    best_tool = &entry.0;
                }
            } else {
                current_run = 1;
                current_errors = if entry.1 { 1 } else { 0 };
                run_tool = &entry.0;
            }
        }

        if best_run >= 3 {
            flags.push(DetectionFlag {
                category: "correction".to_string(),
                rule: "repeated_tool_retry".to_string(),
                severity: Severity::Warning,
                evidence: json!({
                    "tool_name": best_tool,
                    "consecutive_count": best_run,
                    "error_count": best_errors,
                }),
            });
        }
    }

    flags
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

    #[test]
    fn negation_after_tool_detected() {
        let req = make_request(vec![
            json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "Edit", "input": {}}]
            }),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "no that's wrong, use the other approach instead"}]
            }),
        ]);
        let flags = detect(&req);
        let flag = flags.iter().find(|f| f.rule == "negation_after_tool");
        assert!(flag.is_some());
        let kw = flag.unwrap().evidence["negation_keywords"]
            .as_array()
            .unwrap();
        assert!(kw.contains(&json!("no")));
        assert!(kw.contains(&json!("wrong")));
        assert!(kw.contains(&json!("instead")));
    }

    #[test]
    fn tool_result_not_flagged_as_correction() {
        let req = make_request(vec![
            json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "Edit", "input": {}}]
            }),
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "no error"}]
            }),
        ]);
        let flags = detect(&req);
        assert!(flags.is_empty());
    }

    #[test]
    fn multi_correction_flagged() {
        let mut msgs = Vec::new();
        for _ in 0..3 {
            msgs.push(json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "Edit", "input": {}}]
            }));
            msgs.push(json!({
                "role": "user",
                "content": [{"type": "text", "text": "wrong, don't do that"}]
            }));
        }
        let req = make_request(msgs);
        let flags = detect(&req);
        assert!(flags.iter().any(|f| f.rule == "multi_correction"));
    }

    #[test]
    fn repeated_tool_retry_with_errors_detected() {
        // Edit→Error→Edit→Error→Edit = genuine retry loop (66% error rate)
        let req = make_request(vec![
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Edit", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "err", "is_error": true}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "Edit", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": "err", "is_error": true}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t3", "name": "Edit", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t3", "content": "ok"}]}),
        ]);
        let flags = detect(&req);
        let flag = flags.iter().find(|f| f.rule == "repeated_tool_retry");
        assert!(flag.is_some());
        assert_eq!(flag.unwrap().evidence["tool_name"], "Edit");
        assert_eq!(flag.unwrap().evidence["consecutive_count"], 3);
        assert_eq!(flag.unwrap().evidence["error_count"], 2);
    }

    #[test]
    fn low_error_rate_bash_not_flagged() {
        // 6 Bash calls with only 1 error = 16% error rate, below 30% threshold
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(json!({"role": "assistant", "content": [{"type": "tool_use", "id": format!("t{i}"), "name": "Bash", "input": {}}]}));
            let is_error = i == 2; // only 1 error
            msgs.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("t{i}"), "content": "result", "is_error": is_error}]}));
        }
        let req = make_request(msgs);
        let flags = detect(&req);
        assert!(
            !flags.iter().any(|f| f.rule == "repeated_tool_retry"),
            "low error rate bash sequence should not be flagged"
        );
    }

    #[test]
    fn consecutive_reads_without_errors_not_flagged() {
        // Read→OK→Read→OK→Read→OK = normal sequential usage, not a retry
        let req = make_request(vec![
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "file content 1"}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "Read", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": "file content 2"}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t3", "name": "Read", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t3", "content": "file content 3"}]}),
        ]);
        let flags = detect(&req);
        assert!(
            !flags.iter().any(|f| f.rule == "repeated_tool_retry"),
            "sequential reads without errors should not be flagged as retries"
        );
    }

    #[test]
    fn no_correction_on_normal_flow() {
        let req = make_request(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "please edit the file"}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Edit", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
        ]);
        let flags = detect(&req);
        assert!(flags.is_empty());
    }

    #[test]
    fn string_content_user_message_correction() {
        let req = make_request(vec![
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Edit", "input": {}}]}),
            json!({"role": "user", "content": "no, revert that"}),
        ]);
        let flags = detect(&req);
        assert!(flags.iter().any(|f| f.rule == "negation_after_tool"));
    }
}
