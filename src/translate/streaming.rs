use serde_json::Value;

use super::models;

/// Format a JSON event as an SSE line pair.
///
/// Anthropic SSE format:
/// ```text
/// event: <event_type>
/// data: <json>
///
/// ```
pub fn format_sse_event(event_type: &str, data: &Value) -> String {
    format!("event: {event_type}\ndata: {}\n\n", data)
}

/// Normalize a streaming event from Bedrock to Anthropic SSE format.
///
/// Bedrock streaming responses use the same JSON event structure as Anthropic,
/// but delivered via AWS event stream binary protocol instead of SSE.
/// Once we've parsed the binary frame into JSON, we just need to:
/// - Map model IDs back to Anthropic format
/// - Ensure cache usage fields exist
/// - Re-emit as SSE text
pub fn normalize_stream_event(
    mut event: Value,
    original_model: &str,
    model_cache: Option<&models::ModelCache>,
) -> Value {
    // Normalize model ID in message_start events
    if let Some(message) = event.get_mut("message")
        && let Some(obj) = message.as_object_mut()
    {
        if let Some(model) = obj.get("model").and_then(|m| m.as_str()) {
            obj.insert(
                "model".to_string(),
                Value::String(models::bedrock_to_anthropic(model, model_cache)),
            );
        }

        // Ensure cache fields in usage
        if let Some(usage) = obj.get_mut("usage").and_then(|u| u.as_object_mut()) {
            usage
                .entry("cache_creation_input_tokens")
                .or_insert(Value::Number(0.into()));
            usage
                .entry("cache_read_input_tokens")
                .or_insert(Value::Number(0.into()));
        }
    }

    // Also handle usage in message_delta events
    if let Some(usage) = event.get_mut("usage").and_then(|u| u.as_object_mut()) {
        usage
            .entry("cache_creation_input_tokens")
            .or_insert(Value::Number(0.into()));
        usage
            .entry("cache_read_input_tokens")
            .or_insert(Value::Number(0.into()));
    }

    // Strip Bedrock-specific fields that aren't part of the Anthropic SSE spec
    if let Some(obj) = event.as_object_mut() {
        obj.remove("amazon-bedrock-invocationMetrics");
    }

    let _ = original_model; // used in message_start branch above

    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_format_sse() {
        let data = json!({"type": "message_stop"});
        let sse = format_sse_event("message_stop", &data);
        assert!(sse.starts_with("event: message_stop\n"));
        assert!(sse.contains("data: "));
        assert!(sse.ends_with("\n\n"));
    }

    #[test]
    fn test_normalize_message_start() {
        let event = json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "model": "us.anthropic.claude-sonnet-4-6-v1",
                "usage": {"input_tokens": 10, "output_tokens": 1}
            }
        });

        let normalized = normalize_stream_event(event, "claude-sonnet-4-6-20250514", None);
        assert_eq!(normalized["message"]["model"], "claude-sonnet-4-6-20250514");
        assert_eq!(
            normalized["message"]["usage"]["cache_creation_input_tokens"],
            0
        );
    }
}
