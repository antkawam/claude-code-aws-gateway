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

/// Translate an Anthropic Messages API request into a Bedrock InvokeModel request.
///
/// Returns (bedrock_model_id, bedrock_request_body, web_search_context).
/// If a `web_search_*` server tool is present, it is replaced with a regular tool
/// definition that Bedrock can handle, and the context is returned for the handler
/// to orchestrate search execution.
pub fn translate(
    req: AnthropicRequest,
    beta_header: Option<&str>,
    model_prefix: &str,
    model_cache: Option<&models::ModelCache>,
) -> (String, BedrockRequest, Option<websearch::WebSearchContext>) {
    let bedrock_model = models::anthropic_to_bedrock(&req.model, model_prefix, model_cache);

    let betas = beta_header.map(models::filter_betas).unwrap_or_default();

    // Extract web_search server tool (if present) and replace with regular tool definition
    let (tools, web_search_ctx) = websearch::extract_web_search_tool(req.tools);

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
        let (model, body, ws_ctx) = translate(req, None, "us", None);
        assert_eq!(model, "us.anthropic.claude-sonnet-4-6");
        assert_eq!(body.anthropic_version, "bedrock-2023-05-31");
        assert_eq!(body.max_tokens, 1024);
        assert!(ws_ctx.is_none());
    }

    #[test]
    fn test_translate_au_prefix() {
        let req = make_request("claude-sonnet-4-6-20250514");
        let (model, _, _) = translate(req, None, "au", None);
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

        let (_, body, _) = translate(req, None, "us", None);
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

        let (_, body, ws_ctx) = translate(req, None, "us", None);
        let ctx = ws_ctx.unwrap();
        assert_eq!(ctx.tool_name, "web_search");
        assert_eq!(ctx.max_uses, 3);
        // web_search should be replaced with regular tool, read_file unchanged
        let tools = body.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "web_search");
        assert!(tools[0].get("input_schema").is_some());
    }
}
