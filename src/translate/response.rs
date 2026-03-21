use serde_json::Value;

use super::models;

/// Normalize a Bedrock response to look like an Anthropic Messages API response.
///
/// Key changes:
/// - Map model ID back to Anthropic format
/// - Ensure cache token fields exist (CC may expect them)
pub fn normalize_response(
    mut response: Value,
    original_model: &str,
    model_cache: Option<&models::ModelCache>,
) -> Value {
    if let Some(obj) = response.as_object_mut() {
        // Map model ID back to Anthropic format
        obj.insert(
            "model".to_string(),
            Value::String(models::bedrock_to_anthropic(
                obj.get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or(original_model),
                model_cache,
            )),
        );

        // Ensure usage has cache fields (CC may check for these)
        if let Some(usage) = obj.get_mut("usage").and_then(|u| u.as_object_mut()) {
            usage
                .entry("cache_creation_input_tokens")
                .or_insert(Value::Number(0.into()));
            usage
                .entry("cache_read_input_tokens")
                .or_insert(Value::Number(0.into()));
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_adds_cache_fields() {
        let resp = json!({
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "us.anthropic.claude-sonnet-4-6-v1",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        });

        let normalized = normalize_response(resp, "claude-sonnet-4-6-20250514", None);
        let usage = normalized["usage"].as_object().unwrap();
        assert_eq!(usage["cache_creation_input_tokens"], 0);
        assert_eq!(usage["cache_read_input_tokens"], 0);
        assert_eq!(normalized["model"], "claude-sonnet-4-6-20250514");
    }
}
