use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::models;
use crate::websearch;

/// Incoming request from Claude Code (Anthropic Messages API format).
#[derive(Debug, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub thinking: Option<Value>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u64>,
    // MCP connector (Anthropic-only beta feature) — accepted but not forwarded to Bedrock
    #[serde(default)]
    #[allow(dead_code)]
    pub mcp_servers: Option<Vec<Value>>,
}

/// Request body for Bedrock InvokeModel.
#[derive(Debug, Serialize)]
pub struct BedrockRequest {
    pub anthropic_version: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub anthropic_beta: Vec<String>,
    pub max_tokens: u64,
    pub messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
}

/// Allowed fields inside a `cache_control` object for Bedrock.
/// Bedrock only accepts `type` and `ttl`; extra fields (e.g. `scope`) cause 400 errors.
const ALLOWED_CACHE_CONTROL_FIELDS: &[&str] = &["type", "ttl"];

/// Recursively walk a JSON value tree and strip unknown fields from any
/// `cache_control` objects. Bedrock rejects extra fields like `scope`.
pub fn sanitize_cache_control(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(cc) = map.get_mut("cache_control")
                && let Some(cc_obj) = cc.as_object_mut()
            {
                let before = cc_obj.len();
                cc_obj.retain(|key, _| ALLOWED_CACHE_CONTROL_FIELDS.contains(&key.as_str()));
                if cc_obj.len() < before {
                    tracing::debug!(
                        stripped = before - cc_obj.len(),
                        "Stripped unknown fields from cache_control"
                    );
                }
            }
            for v in map.values_mut() {
                sanitize_cache_control(v);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                sanitize_cache_control(item);
            }
        }
        _ => {}
    }
}

/// Translate an Anthropic Messages API request into a Bedrock InvokeModel request.
///
/// Returns (bedrock_model_id, bedrock_request_body, web_search_context).
/// If a `web_search_*` server tool is present, it is replaced with a regular tool
/// definition that Bedrock can handle, and the context is returned for the handler
/// to orchestrate search execution.
pub fn translate(
    mut req: AnthropicRequest,
    model_prefix: &str,
    model_cache: Option<&models::ModelCache>,
    websearch_mode: &str,
) -> (String, BedrockRequest, Option<websearch::WebSearchContext>) {
    let bedrock_model = models::anthropic_to_bedrock(&req.model, model_prefix, model_cache);

    let betas: Vec<String> = Vec::new();

    // Extract web_search server tool (if present) and replace with regular tool definition.
    // The mode controls behavior: "disabled" strips tools, "enabled"/"global" processes them.
    let (tools, web_search_ctx) =
        websearch::extract_web_search_tool_with_mode(req.tools, websearch_mode);

    // Sanitize cache_control in all Value trees — Bedrock rejects unknown fields.
    for msg in &mut req.messages {
        sanitize_cache_control(msg);
    }
    if let Some(ref mut sys) = req.system {
        sanitize_cache_control(sys);
    }
    let tools = tools.map(|mut t| {
        for tool in &mut t {
            sanitize_cache_control(tool);
        }
        t
    });

    let bedrock_req = BedrockRequest {
        anthropic_version: "bedrock-2023-05-31".to_string(),
        anthropic_beta: betas,
        max_tokens: req.max_tokens.unwrap_or(8096),
        messages: req.messages,
        system: req.system,
        thinking: req.thinking,
        tools,
        tool_choice: req.tool_choice,
        metadata: req.metadata,
        stop_sequences: req.stop_sequences,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
    };

    (bedrock_model, bedrock_req, web_search_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_request(model: &str) -> AnthropicRequest {
        AnthropicRequest {
            model: model.to_string(),
            max_tokens: Some(1024),
            messages: vec![json!({"role": "user", "content": "hello"})],
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
    fn test_translate_basic() {
        let req = make_request("claude-sonnet-4-6-20250514");
        let (model, body, ws_ctx) = translate(req, "us", None, "enabled");
        assert_eq!(model, "us.anthropic.claude-sonnet-4-6");
        assert_eq!(body.anthropic_version, "bedrock-2023-05-31");
        assert_eq!(body.max_tokens, 1024);
        assert!(ws_ctx.is_none());
    }

    #[test]
    fn test_translate_au_prefix() {
        let req = make_request("claude-sonnet-4-6-20250514");
        let (model, _, _) = translate(req, "au", None, "enabled");
        assert_eq!(model, "au.anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn test_preserves_cache_control() {
        let req = AnthropicRequest {
            messages: vec![json!({
                "role": "user",
                "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]
            })],
            system: Some(
                json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}]),
            ),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_, body, _) = translate(req, "us", None, "enabled");
        let msg_str = serde_json::to_string(&body.messages).unwrap();
        assert!(msg_str.contains("cache_control"));
        let sys_str = serde_json::to_string(&body.system).unwrap();
        assert!(sys_str.contains("cache_control"));
    }

    #[test]
    fn test_translate_with_web_search_tool() {
        let req = AnthropicRequest {
            tools: Some(vec![
                json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
                json!({"name": "read_file", "input_schema": {"type": "object"}}),
            ]),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_, body, ws_ctx) = translate(req, "us", None, "enabled");
        let ctx = ws_ctx.unwrap();
        assert_eq!(ctx.tool_name, "web_search");
        assert_eq!(ctx.max_uses, 3);
        // web_search should be replaced with regular tool, read_file unchanged
        let tools = body.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "web_search");
        assert!(tools[0].get("input_schema").is_some());
    }

    // ============================================================
    // Round 5: Wiring websearch mode into translate()
    // ============================================================
    // These tests call translate() with a 5th parameter `websearch_mode`
    // that controls whether web_search tools are processed, stripped, or
    // handled via the global provider. The current translate() signature
    // does NOT accept this parameter, so these tests will fail to compile.

    #[test]
    fn test_translate_strips_websearch_when_disabled() {
        // When websearch_mode is "disabled", the web_search server tool should
        // be stripped entirely and WebSearchContext should be None.
        let req = AnthropicRequest {
            tools: Some(vec![
                json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5}),
                json!({"name": "read_file", "input_schema": {"type": "object"}}),
            ]),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_model, body, ws_ctx) = translate(req, "us", None, "disabled");

        // Disabled mode: no web search context
        assert!(
            ws_ctx.is_none(),
            "translate with mode 'disabled' should return None WebSearchContext"
        );

        // The web_search tool should be stripped from the tools list
        let tools = body.tools.unwrap();
        assert_eq!(
            tools.len(),
            1,
            "disabled mode should strip web_search tool, leaving only read_file"
        );
        assert_eq!(tools[0]["name"], "read_file");
    }

    #[test]
    fn test_translate_preserves_websearch_when_enabled() {
        // When websearch_mode is "enabled", web_search should be processed
        // normally (replaced with regular tool def, context returned).
        let req = AnthropicRequest {
            tools: Some(vec![
                json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
                json!({"name": "bash", "input_schema": {"type": "object"}}),
            ]),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_model, body, ws_ctx) = translate(req, "us", None, "enabled");

        // Enabled mode: web search context should be present
        let ctx =
            ws_ctx.expect("translate with mode 'enabled' should return Some WebSearchContext");
        assert_eq!(ctx.tool_name, "web_search");
        assert_eq!(ctx.max_uses, 3);

        // Both tools should be present (web_search replaced with regular tool def)
        let tools = body.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "web_search");
        assert!(
            tools[0].get("input_schema").is_some(),
            "web_search should be replaced with regular tool definition"
        );
    }

    #[test]
    fn test_translate_preserves_websearch_when_global() {
        // When websearch_mode is "global", web_search should be processed
        // the same as "enabled" — the difference is in provider resolution
        // (handled in handlers.rs), not in translation.
        let req = AnthropicRequest {
            tools: Some(vec![
                json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 10}),
                json!({"name": "editor", "input_schema": {"type": "object"}}),
            ]),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_model, body, ws_ctx) = translate(req, "us", None, "global");

        // Global mode: web search context should be present
        let ctx = ws_ctx.expect("translate with mode 'global' should return Some WebSearchContext");
        assert_eq!(ctx.tool_name, "web_search");
        assert_eq!(ctx.max_uses, 10);

        // Both tools should be present
        let tools = body.tools.unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn test_translate_no_websearch_tool_with_disabled_mode_is_noop() {
        // When there's no web_search tool in the request and mode is "disabled",
        // translation should work normally (no tools to strip).
        let req = AnthropicRequest {
            tools: Some(vec![
                json!({"name": "read_file", "input_schema": {"type": "object"}}),
            ]),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_model, body, ws_ctx) = translate(req, "us", None, "disabled");

        assert!(
            ws_ctx.is_none(),
            "No web_search tool means no context regardless of mode"
        );
        let tools = body.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read_file");
    }

    // ============================================================
    // sanitize_cache_control — unit tests (added to #[cfg(test)] mod tests)
    // These tests call sanitize_cache_control() directly.
    // The function does not exist yet (TDD); tests will fail to compile
    // until @builder implements it.
    // ============================================================

    #[test]
    fn test_sanitize_preserves_valid_cache_control() {
        // {"type": "ephemeral"} contains only an allowed field and must pass through unchanged.
        let mut value =
            json!({"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}});
        sanitize_cache_control(&mut value);
        assert_eq!(
            value["cache_control"],
            json!({"type": "ephemeral"}),
            "cache_control with only 'type' should be preserved unchanged"
        );
    }

    #[test]
    fn test_sanitize_preserves_cache_control_with_ttl() {
        // {"type": "ephemeral", "ttl": "5m"} uses only allowed fields; must pass through unchanged.
        let mut value = json!({"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "ttl": "5m"}});
        sanitize_cache_control(&mut value);
        assert_eq!(
            value["cache_control"],
            json!({"type": "ephemeral", "ttl": "5m"}),
            "cache_control with 'type' and 'ttl' should be preserved unchanged"
        );
    }

    #[test]
    fn test_sanitize_strips_scope_from_cache_control() {
        // {"type": "ephemeral", "scope": "turn"} — "scope" is not in Bedrock's allowlist.
        let mut value = json!({"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "scope": "turn"}});
        sanitize_cache_control(&mut value);
        assert_eq!(
            value["cache_control"],
            json!({"type": "ephemeral"}),
            "cache_control 'scope' field should be stripped"
        );
    }

    #[test]
    fn test_sanitize_strips_multiple_unknown_fields() {
        // Multiple unknown fields should all be removed; only "type" survives.
        let mut value = json!({
            "type": "text",
            "text": "hi",
            "cache_control": {"type": "ephemeral", "scope": "turn", "priority": "high", "foo": 42}
        });
        sanitize_cache_control(&mut value);
        assert_eq!(
            value["cache_control"],
            json!({"type": "ephemeral"}),
            "all unknown cache_control fields should be stripped, leaving only 'type'"
        );
    }

    #[test]
    fn test_sanitize_preserves_type_and_ttl_strips_rest() {
        // Both "type" and "ttl" are allowed; "scope" should be removed.
        let mut value = json!({
            "type": "text",
            "text": "hi",
            "cache_control": {"type": "ephemeral", "ttl": "1h", "scope": "turn"}
        });
        sanitize_cache_control(&mut value);
        assert_eq!(
            value["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"}),
            "'type' and 'ttl' should be preserved; 'scope' should be stripped"
        );
    }

    #[test]
    fn test_sanitize_no_cache_control() {
        // A content block without cache_control should pass through completely unchanged.
        let mut value = json!({"type": "text", "text": "hello"});
        let original = value.clone();
        sanitize_cache_control(&mut value);
        assert_eq!(
            value, original,
            "value without cache_control should be unchanged"
        );
    }

    #[test]
    fn test_sanitize_null_cache_control() {
        // cache_control: null is technically valid JSON — should not crash and pass through as-is.
        let mut value = json!({"type": "text", "text": "hi", "cache_control": null});
        sanitize_cache_control(&mut value);
        assert_eq!(
            value["cache_control"],
            json!(null),
            "null cache_control should pass through without crashing"
        );
    }

    #[test]
    fn test_sanitize_non_object_cache_control() {
        // cache_control: "ephemeral" (a string, not an object) — malformed input should not crash
        // and should pass through unchanged (no fields to strip from a non-object).
        let mut value = json!({"type": "text", "text": "hi", "cache_control": "ephemeral"});
        sanitize_cache_control(&mut value);
        assert_eq!(
            value["cache_control"],
            json!("ephemeral"),
            "non-object cache_control should pass through without crashing"
        );
    }

    #[test]
    fn test_sanitize_nested_content_blocks() {
        // A tool_result block contains a content array whose inner blocks may have cache_control.
        // sanitize_cache_control must recurse into nested structures.
        let mut value = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_01",
            "content": [
                {
                    "type": "text",
                    "text": "result",
                    "cache_control": {"type": "ephemeral", "scope": "turn"}
                }
            ]
        });
        sanitize_cache_control(&mut value);
        let inner_cc = &value["content"][0]["cache_control"];
        assert_eq!(
            *inner_cc,
            json!({"type": "ephemeral"}),
            "scope should be stripped from cache_control nested inside tool_result content"
        );
        assert!(
            inner_cc.get("scope").is_none(),
            "scope must not survive recursion into nested content arrays"
        );
    }

    // ============================================================
    // sanitize_cache_control — integration tests through translate()
    // ============================================================

    #[test]
    fn test_translate_sanitizes_cache_control_in_messages() {
        // A message content block with an unknown "scope" field in cache_control should have
        // that field stripped in the translated BedrockRequest.
        let req = AnthropicRequest {
            messages: vec![json!({
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "hello",
                        "cache_control": {"type": "ephemeral", "scope": "turn"}
                    }
                ]
            })],
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_, body, _) = translate(req, "us", None, "enabled");
        let msg_str = serde_json::to_string(&body.messages).unwrap();

        assert!(
            msg_str.contains("cache_control"),
            "cache_control should still be present in translated messages"
        );
        assert!(
            !msg_str.contains("\"scope\""),
            "unknown 'scope' field should be stripped from cache_control in messages; got: {msg_str}"
        );
        assert!(
            msg_str.contains("\"type\":\"ephemeral\""),
            "allowed 'type' field should be preserved in messages cache_control"
        );
    }

    #[test]
    fn test_translate_sanitizes_cache_control_in_system() {
        // A system content block with an unknown "scope" field should have that field stripped.
        let req = AnthropicRequest {
            system: Some(json!([
                {
                    "type": "text",
                    "text": "You are a helpful assistant.",
                    "cache_control": {"type": "ephemeral", "scope": "turn"}
                }
            ])),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_, body, _) = translate(req, "us", None, "enabled");
        let sys_str = serde_json::to_string(&body.system).unwrap();

        assert!(
            sys_str.contains("cache_control"),
            "cache_control should still be present in translated system"
        );
        assert!(
            !sys_str.contains("\"scope\""),
            "unknown 'scope' field should be stripped from cache_control in system; got: {sys_str}"
        );
        assert!(
            sys_str.contains("\"type\":\"ephemeral\""),
            "allowed 'type' field should be preserved in system cache_control"
        );
    }

    #[test]
    fn test_translate_sanitizes_cache_control_in_tools() {
        // A tool definition with a cache_control containing an unknown field should be sanitized.
        let req = AnthropicRequest {
            tools: Some(vec![json!({
                "name": "bash",
                "description": "Run a bash command",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral", "scope": "turn"}
            })]),
            ..make_request("claude-sonnet-4-6-20250514")
        };

        let (_, body, _) = translate(req, "us", None, "enabled");
        let tools_str = serde_json::to_string(&body.tools).unwrap();

        assert!(
            tools_str.contains("cache_control"),
            "cache_control should still be present in translated tools"
        );
        assert!(
            !tools_str.contains("\"scope\""),
            "unknown 'scope' field should be stripped from cache_control in tools; got: {tools_str}"
        );
        assert!(
            tools_str.contains("\"type\":\"ephemeral\""),
            "allowed 'type' field should be preserved in tools cache_control"
        );
    }
}
