pub mod providers;

use serde_json::{Value, json};

pub use providers::SearchProvider;

/// A single web search result.
#[derive(Debug, Clone)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Context extracted from the request when a web_search server tool is present.
#[derive(Debug, Clone)]
pub struct WebSearchContext {
    /// The tool name (usually "web_search")
    pub tool_name: String,
    /// Maximum number of search invocations allowed per request
    pub max_uses: u32,
}

/// Search DuckDuckGo HTML endpoint and extract results.
///
/// Uses the lite HTML interface at `html.duckduckgo.com/html/` which requires
/// no API key. Results are parsed from the HTML response.
pub async fn search_duckduckgo(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<WebSearchResult>> {
    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("Referer", "https://html.duckduckgo.com/")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Sec-Fetch-Mode", "navigate")
        .form(&[("q", query)])
        .send()
        .await?;

    // DDG returns HTTP 202 when rate-limiting
    if resp.status().as_u16() == 202 {
        tracing::warn!(query = %query, "DuckDuckGo rate limited (HTTP 202)");
        anyhow::bail!("DuckDuckGo rate limited — try again shortly");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        tracing::warn!(query = %query, %status, "DuckDuckGo returned error");
        anyhow::bail!("DuckDuckGo returned HTTP {}", status);
    }

    let html = resp.text().await?;
    let results = parse_ddg_html(&html, max_results);

    if results.is_empty() {
        tracing::warn!(query = %query, html_len = html.len(), "DuckDuckGo returned no results");
    } else {
        tracing::debug!(query = %query, count = results.len(), "DuckDuckGo search completed");
    }

    Ok(results)
}

/// Parse DuckDuckGo HTML search results.
///
/// The HTML structure uses:
/// - `<a class="result__a" href="...">Title</a>` for result links
/// - `<a class="result__snippet">Snippet text</a>` for descriptions
fn parse_ddg_html(html: &str, max_results: usize) -> Vec<WebSearchResult> {
    let mut results = Vec::new();

    // Split by result blocks — each result is in a div with class "result"
    // We look for the pattern of result__a (title+url) followed by result__snippet
    let mut search_pos = 0;

    while results.len() < max_results {
        // Find next result link
        let anchor_marker = "class=\"result__a\"";
        let anchor_pos = match html[search_pos..].find(anchor_marker) {
            Some(p) => search_pos + p,
            None => break,
        };

        // Extract href from the anchor tag
        // Look backwards from the class marker to find href="..."
        let tag_start = html[..anchor_pos].rfind('<').unwrap_or(anchor_pos);
        // Ad hrefs can be very long (2000+ chars), so grab enough to find the closing >
        let tag_end = html[tag_start..]
            .find('>')
            .map(|p| tag_start + p + 1)
            .unwrap_or(html.len());
        let tag_section = &html[tag_start..tag_end];

        let raw_href = extract_attr(tag_section, "href").unwrap_or_default();

        // Skip DDG ad results (href points to duckduckgo.com/y.js with ad params)
        if raw_href.contains("duckduckgo.com/y.js") || raw_href.contains("ad_provider") {
            search_pos = anchor_pos + anchor_marker.len();
            continue;
        }

        // DDG wraps URLs in a redirect; extract the actual URL
        let url = extract_ddg_url(&raw_href);

        // Extract title (text between > and </a>)
        let title_start = match html[anchor_pos..].find('>') {
            Some(p) => anchor_pos + p + 1,
            None => {
                search_pos = anchor_pos + 1;
                continue;
            }
        };
        let title_end = match html[title_start..].find("</a>") {
            Some(p) => title_start + p,
            None => {
                search_pos = anchor_pos + 1;
                continue;
            }
        };
        let title = strip_html_tags(&html[title_start..title_end]);

        // Find snippet (next result__snippet after this result__a)
        let snippet_marker = "class=\"result__snippet\"";
        let snippet = if let Some(snippet_pos) = html[title_end..].find(snippet_marker) {
            let abs_pos = title_end + snippet_pos;
            let content_start = match html[abs_pos..].find('>') {
                Some(p) => abs_pos + p + 1,
                None => abs_pos,
            };
            let content_end = match html[content_start..].find("</a>") {
                Some(p) => content_start + p,
                None => match html[content_start..].find("</span>") {
                    Some(p) => content_start + p,
                    None => content_start,
                },
            };
            strip_html_tags(&html[content_start..content_end])
        } else {
            String::new()
        };

        search_pos = title_end;

        // Skip empty/invalid results
        if url.is_empty() || title.is_empty() {
            continue;
        }

        results.push(WebSearchResult {
            title,
            url,
            snippet,
        });
    }

    results
}

/// Extract a URL from DDG's redirect wrapper.
/// DDG links look like: `//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&rut=...`
fn extract_ddg_url(url: &str) -> String {
    if let Some(uddg_pos) = url.find("uddg=") {
        let encoded = &url[uddg_pos + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        if let Ok(decoded) = urlencoding::decode(&encoded[..end]) {
            return decoded.into_owned();
        }
    }
    // If not a redirect, clean up the URL
    let url = url.trim_start_matches("//");
    if !url.starts_with("http") {
        format!("https://{}", url)
    } else {
        url.to_string()
    }
}

/// Strip HTML tags from a string, decode common entities.
fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    if let Some(start) = tag.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = tag[value_start..].find('"') {
            return Some(tag[value_start..value_start + end].to_string());
        }
    }
    // Try single quotes
    let pattern = format!("{}='", attr);
    if let Some(start) = tag.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = tag[value_start..].find('\'') {
            return Some(tag[value_start..value_start + end].to_string());
        }
    }
    None
}

/// Convert search results to the `web_search_tool_result` content block format.
pub fn results_to_content_block(tool_use_id: &str, results: &[WebSearchResult]) -> Value {
    let search_results: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "type": "web_search_result",
                "url": r.url,
                "title": r.title,
                "encrypted_content": r.snippet,
            })
        })
        .collect();

    json!({
        "type": "web_search_tool_result",
        "tool_use_id": tool_use_id,
        "content": search_results,
    })
}

/// Convert search results to a plain text tool_result for the Bedrock follow-up.
pub fn results_to_tool_result_text(results: &[WebSearchResult]) -> String {
    if results.is_empty() {
        return "No search results found.".to_string();
    }
    results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("[{}] {}\nURL: {}\n{}", i + 1, r.title, r.url, r.snippet))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Extract web_search server tool from the tools array and return context + replacement tool.
///
/// Returns (modified_tools, web_search_context).
/// If no web_search server tool is found, tools are returned unchanged.
pub fn extract_web_search_tool(
    tools: Option<Vec<Value>>,
) -> (Option<Vec<Value>>, Option<WebSearchContext>) {
    let Some(tools) = tools else {
        return (None, None);
    };

    let mut filtered = Vec::new();
    let mut ctx = None;

    for tool in tools {
        let tool_type = tool.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if tool_type.starts_with("web_search_") {
            let name = tool
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("web_search")
                .to_string();
            let max_uses = tool.get("max_uses").and_then(|m| m.as_u64()).unwrap_or(5) as u32;

            ctx = Some(WebSearchContext {
                tool_name: name.clone(),
                max_uses,
            });

            // Inject a regular tool definition that Bedrock understands
            filtered.push(json!({
                "name": name,
                "description": "Search the web for current information. Use this when you need up-to-date information that may not be in your training data. Returns a list of search results with titles, URLs, and content snippets.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query to look up"
                        }
                    },
                    "required": ["query"]
                }
            }));
        } else {
            filtered.push(tool);
        }
    }

    let tools = if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    };
    (tools, ctx)
}

/// Mode-aware variant of `extract_web_search_tool`.
///
/// - `"disabled"`: strips any `web_search_*` tools entirely, returns `None` context.
/// - `"enabled"` / `"global"` (or any other value): delegates to `extract_web_search_tool`.
pub fn extract_web_search_tool_with_mode(
    tools: Option<Vec<Value>>,
    mode: &str,
) -> (Option<Vec<Value>>, Option<WebSearchContext>) {
    if mode == "disabled" {
        let Some(tools) = tools else {
            return (None, None);
        };
        let filtered: Vec<Value> = tools
            .into_iter()
            .filter(|tool| {
                let tool_type = tool.get("type").and_then(|t| t.as_str()).unwrap_or("");
                !tool_type.starts_with("web_search_")
            })
            .collect();
        let tools = if filtered.is_empty() {
            None
        } else {
            Some(filtered)
        };
        (tools, None)
    } else {
        extract_web_search_tool(tools)
    }
}

/// Check if a Bedrock response contains a web_search tool_use that we need to handle.
/// Returns Vec of (tool_use_id, query) pairs.
pub fn find_web_search_tool_uses(content: &[Value], tool_name: &str) -> Vec<(String, String)> {
    content
        .iter()
        .filter_map(|block| {
            if block.get("type")?.as_str()? == "tool_use"
                && block.get("name")?.as_str()? == tool_name
            {
                let id = block.get("id")?.as_str()?.to_string();
                let query = block
                    .get("input")
                    .and_then(|i| i.get("query"))
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string();
                Some((id, query))
            } else {
                None
            }
        })
        .collect()
}

/// Rewrite the final response content: convert web_search tool_use blocks to
/// server_tool_use and insert web_search_tool_result blocks after each.
pub fn rewrite_response_content(
    content: &mut Vec<Value>,
    search_results: &std::collections::HashMap<String, Vec<WebSearchResult>>,
    tool_name: &str,
) {
    let mut i = 0;
    while i < content.len() {
        let is_web_search = content[i].get("type").and_then(|t| t.as_str()) == Some("tool_use")
            && content[i].get("name").and_then(|n| n.as_str()) == Some(tool_name);

        if is_web_search {
            // Rewrite type to server_tool_use
            if let Some(obj) = content[i].as_object_mut() {
                obj.insert("type".to_string(), json!("server_tool_use"));
            }

            // Insert web_search_tool_result after this block
            if let Some(tool_use_id) = content[i].get("id").and_then(|id| id.as_str()) {
                let results = search_results.get(tool_use_id).cloned().unwrap_or_default();
                let result_block = results_to_content_block(tool_use_id, &results);
                i += 1;
                content.insert(i, result_block);
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_html_tags() {
        assert_eq!(strip_html_tags("<b>hello</b> world"), "hello world");
        assert_eq!(strip_html_tags("a &amp; b"), "a & b");
    }

    #[test]
    fn test_extract_ddg_url() {
        let url = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
        assert_eq!(extract_ddg_url(url), "https://example.com/page");
    }

    #[test]
    fn test_extract_web_search_tool() {
        let tools = vec![
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
            json!({"name": "read_file", "input_schema": {"type": "object"}}),
        ];
        let (filtered, ctx) = extract_web_search_tool(Some(tools));
        let ctx = ctx.unwrap();
        assert_eq!(ctx.tool_name, "web_search");
        assert_eq!(ctx.max_uses, 3);
        let filtered = filtered.unwrap();
        assert_eq!(filtered.len(), 2);
        // First tool should be the replacement regular tool
        assert_eq!(filtered[0]["name"], "web_search");
        assert!(filtered[0].get("input_schema").is_some());
        assert!(filtered[0].get("type").is_none()); // regular tool, no type field
        // Second tool unchanged
        assert_eq!(filtered[1]["name"], "read_file");
    }

    #[test]
    fn test_find_web_search_tool_uses() {
        let content = vec![
            json!({"type": "text", "text": "Let me search for that."}),
            json!({"type": "tool_use", "id": "toolu_123", "name": "web_search", "input": {"query": "rust async"}}),
            json!({"type": "tool_use", "id": "toolu_456", "name": "read_file", "input": {"path": "foo.rs"}}),
        ];
        let results = find_web_search_tool_uses(&content, "web_search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "toolu_123");
        assert_eq!(results[0].1, "rust async");
    }

    #[test]
    fn test_results_to_tool_result_text() {
        let results = vec![WebSearchResult {
            title: "Rust Book".to_string(),
            url: "https://doc.rust-lang.org".to_string(),
            snippet: "The Rust Programming Language".to_string(),
        }];
        let text = results_to_tool_result_text(&results);
        assert!(text.contains("[1] Rust Book"));
        assert!(text.contains("https://doc.rust-lang.org"));
    }

    #[test]
    fn test_parse_real_ddg_html() {
        // Realistic DDG HTML snippet (simplified from actual response)
        let html = r#"
        <div class="result">
            <h2><a rel="nofollow" class="result__a" href="https://rust-lang.org/">Rust Programming Language</a></h2>
            <a class="result__snippet" href="https://rust-lang.org/"><b>Rust</b> is a fast, reliable, and productive programming language.</a>
        </div>
        <div class="result">
            <h2><a rel="nofollow" class="result__a" href="https://en.wikipedia.org/wiki/Rust_(programming_language)">Rust (programming language) - Wikipedia</a></h2>
            <a class="result__snippet" href="https://en.wikipedia.org/wiki/Rust_(programming_language)"><b>Rust</b> is a general-purpose programming language noted for its emphasis on performance.</a>
        </div>
        "#;
        let results = parse_ddg_html(html, 5);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert!(results[0].snippet.contains("fast, reliable"));
        assert_eq!(results[1].title, "Rust (programming language) - Wikipedia");
        assert!(results[1].snippet.contains("general-purpose"));
    }

    #[tokio::test]
    async fn test_live_duckduckgo_search() {
        let client = reqwest::Client::new();
        let results = search_duckduckgo(&client, "rust programming language", 3).await;
        match results {
            Ok(results) => {
                assert!(!results.is_empty(), "Expected at least 1 result");
                for r in &results {
                    assert!(!r.title.is_empty(), "Title should not be empty");
                    assert!(
                        r.url.starts_with("http"),
                        "URL should start with http: {}",
                        r.url
                    );
                }
                println!("Live DDG test: {} results", results.len());
                for r in &results {
                    println!(
                        "  [{}] {} - {}",
                        r.url,
                        r.title,
                        &r.snippet[..r.snippet.len().min(80)]
                    );
                }
            }
            Err(e) => {
                // Rate limiting is acceptable in CI — don't fail the test
                if e.to_string().contains("rate limited") {
                    println!("DDG rate limited (acceptable in test): {}", e);
                } else {
                    panic!("Unexpected error: {}", e);
                }
            }
        }
    }

    // ============================================================
    // extract_web_search_tool_with_mode — admin control round 2
    // ============================================================

    #[test]
    fn test_extract_strips_tool_when_disabled() {
        let tools = vec![
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
            json!({"name": "read_file", "input_schema": {"type": "object"}}),
        ];
        let (filtered, ctx) = extract_web_search_tool_with_mode(Some(tools), "disabled");
        // When mode is "disabled", web search context should be None (tool is stripped)
        assert!(ctx.is_none(), "disabled mode should strip web_search context");
        // Only the non-web-search tool should remain
        let filtered = filtered.unwrap();
        assert_eq!(filtered.len(), 1, "disabled mode should remove web_search tool");
        assert_eq!(filtered[0]["name"], "read_file");
    }

    #[test]
    fn test_extract_preserves_tool_when_enabled() {
        let tools = vec![
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5}),
            json!({"name": "read_file", "input_schema": {"type": "object"}}),
        ];
        let (filtered, ctx) = extract_web_search_tool_with_mode(Some(tools), "enabled");
        // When mode is "enabled", web search context should be present
        assert!(ctx.is_some(), "enabled mode should preserve web_search context");
        let ctx = ctx.unwrap();
        assert_eq!(ctx.tool_name, "web_search");
        assert_eq!(ctx.max_uses, 5);
        let filtered = filtered.unwrap();
        assert_eq!(filtered.len(), 2, "enabled mode should keep both tools");
    }

    #[test]
    fn test_extract_preserves_tool_when_global() {
        let tools = vec![
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 10}),
            json!({"name": "bash", "input_schema": {"type": "object"}}),
        ];
        let (filtered, ctx) = extract_web_search_tool_with_mode(Some(tools), "global");
        // When mode is "global", web search context should be present (global provider used)
        assert!(ctx.is_some(), "global mode should preserve web_search context");
        let ctx = ctx.unwrap();
        assert_eq!(ctx.tool_name, "web_search");
        assert_eq!(ctx.max_uses, 10);
        let filtered = filtered.unwrap();
        assert_eq!(filtered.len(), 2, "global mode should keep both tools");
    }

    #[test]
    fn test_rewrite_response_content() {
        use std::collections::HashMap;

        let mut content = vec![
            json!({"type": "text", "text": "Searching..."}),
            json!({"type": "tool_use", "id": "toolu_1", "name": "web_search", "input": {"query": "test"}}),
            json!({"type": "text", "text": "Here are the results."}),
        ];

        let mut search_results = HashMap::new();
        search_results.insert(
            "toolu_1".to_string(),
            vec![WebSearchResult {
                title: "Test".to_string(),
                url: "https://test.com".to_string(),
                snippet: "A test page".to_string(),
            }],
        );

        rewrite_response_content(&mut content, &search_results, "web_search");

        assert_eq!(content.len(), 4); // original 3 + 1 inserted result block
        assert_eq!(content[1]["type"], "server_tool_use");
        assert_eq!(content[2]["type"], "web_search_tool_result");
        assert_eq!(content[2]["tool_use_id"], "toolu_1");
    }
}
