---
title: "Enabling Web Search for Claude Code on Amazon Bedrock"
description: "Bedrock doesn't implement Anthropic's web_search server tool. CCAG intercepts web search requests and executes them via DuckDuckGo, Tavily, Serper, or a custom provider."
---

# Enabling Web Search for Claude Code on Bedrock

When Claude Code runs through the Anthropic API, it can invoke a `web_search` tool to look up documentation, check error messages, or verify facts. This is a server-side tool — the Anthropic API executes the search and returns results to the model.

Amazon Bedrock does not implement this server-side tool. When Claude Code connects to Bedrock directly, web search is not available.

## How CCAG Implements Web Search

CCAG intercepts requests containing `web_search` tool use and executes the search itself. The flow:

1. Claude Code sends a request with a `web_search` tool invocation
2. CCAG detects the tool use in the response stream
3. CCAG executes the search query against the configured provider
4. Results are formatted as Anthropic's `web_search_tool_result` and injected into the conversation
5. Claude Code receives the results as if they came from the Anthropic API

This is transparent to Claude Code — it sends the same requests and receives the same response format as it would with the Anthropic API.

## Supported Search Providers

| Provider | Config | Notes |
|---|---|---|
| DuckDuckGo | Default (no config needed) | Free, no API key required |
| Tavily | `TAVILY_API_KEY` | AI-optimized search, better relevance for technical queries |
| Serper | `SERPER_API_KEY` | Google Search results via API |
| Custom | Per-user via portal | Any URL that returns results in the expected format |

The default provider is DuckDuckGo, which requires no API keys. For better result quality on technical queries, configure Tavily or Serper.

### Setting the Default Provider

```bash
# Environment variable (gateway-wide default)
WEB_SEARCH_PROVIDER=tavily
TAVILY_API_KEY=tvly-xxxxx
```

### Per-User Provider Override

Admins can configure different search providers per user or team through the admin portal. This is useful when:
- Some users need higher-quality results (Tavily/Serper) while others are fine with DuckDuckGo
- You want to control API costs for search providers
- Specific teams need access to internal search endpoints

## How It Looks in Practice

When a developer uses Claude Code through CCAG and Claude decides it needs to search the web:

```
Developer: "What's the latest syntax for Axum's Router::new() in 0.8?"

Claude Code internally:
  → Invokes web_search tool with query "axum 0.8 Router::new syntax"
  → CCAG intercepts, searches via DuckDuckGo
  → Returns results from docs.rs, GitHub, and Stack Overflow
  → Claude synthesizes the answer from search results

Developer sees: A grounded answer citing current documentation
```

Without CCAG (direct Bedrock), Claude Code would not invoke web search at all. It would answer from training data, which may be outdated for fast-moving libraries.

## When Web Search Activates

Claude Code decides when to use web search based on the conversation context. Common triggers:

- Looking up current documentation or API references
- Checking error messages or stack traces
- Verifying version-specific behavior
- Finding recent discussions or issues about a library

CCAG does not control when searches happen — that is Claude Code's decision. CCAG only provides the search execution layer that Bedrock lacks.

## Rate Limiting

Web search requests count toward the user's rate limit in CCAG. If you are using a paid search provider (Tavily, Serper), you may want to set per-user rate limits to control costs:

```bash
# Create a key with a rate limit
curl -X POST https://ccag.example.com/admin/keys \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"name": "dev-team", "rate_limit_rpm": 120}'
```

DuckDuckGo has no per-query cost, so rate limiting for search is only relevant if you are using paid providers.

## See Also

- [Extended Thinking on Bedrock](bedrock-extended-thinking.md): the other major feature Bedrock mode disables
- [Configuration](../configuration.md): search provider environment variables
- [Comparison](../comparison.md): web search support across CCAG, LiteLLM, and Direct Bedrock
