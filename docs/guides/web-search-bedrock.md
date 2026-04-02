---
title: "Web Search for Claude Code on Amazon Bedrock"
description: "Amazon Bedrock does not implement Anthropic's web_search server tool. CCAG intercepts web search tool use and executes searches via DuckDuckGo, Tavily, Serper, or a custom provider — transparently to Claude Code."
---

# Web Search for Claude Code on Amazon Bedrock

When Claude Code is connected to the Anthropic API directly, it can invoke a `web_search` tool to look up current documentation, check error messages, or research unfamiliar APIs. This is a server-side tool: Anthropic's infrastructure executes the search and returns results to Claude.

Amazon Bedrock does not implement this server-side tool. When Claude Code connects to Bedrock with `CLAUDE_CODE_USE_BEDROCK=1`, web search is silently unavailable. Claude Code does not issue an error — it simply does not use web search at all, relying on training data instead.

This matters because Claude's training data has a cutoff date. For actively developed libraries, recent API changes, current CVEs, or anything published after the cutoff, web search is often the difference between a correct and an incorrect answer.

## Why Bedrock Does Not Support web_search

The `web_search` tool is part of Anthropic's server-side tool infrastructure. When a model invokes it, the Anthropic API routes the query to a search service, formats the results, and continues the conversation. Bedrock exposes Anthropic's models but does not implement Anthropic's server-side tooling. The underlying inference is identical; the surrounding service layer is different.

There is no Bedrock configuration that enables `web_search` natively — it is an architectural gap.

## How CCAG Fills the Gap

CCAG runs a server-side search loop that replicates what the Anthropic API does natively. The steps:

1. Claude Code sends a request containing a `web_search` tool definition
2. CCAG replaces the server tool with a regular tool definition that Bedrock accepts
3. Bedrock returns a `tool_use` block requesting a web search
4. CCAG executes the search via the configured provider (DuckDuckGo, Tavily, Serper, or custom)
5. Search results are formatted as `web_search_tool_result` content blocks
6. CCAG re-invokes Bedrock with results appended to the conversation
7. Steps 3-6 repeat until Bedrock stops requesting searches (up to the per-request limit)

The search loop runs entirely on the gateway. Claude Code sends one request and receives one final response — it does not see the intermediate tool calls.

## Supported Providers

| Provider | API Key | Notes |
|---|---|---|
| DuckDuckGo | None required | Default. HTML scraping. May be rate-limited under heavy use. |
| Tavily | Required | AI-optimized search, structured results. Good for technical queries. |
| Serper | Required | Google Search results via API. High accuracy. |
| Custom | Optional | Any HTTPS endpoint that returns results in the expected JSON format. |

DuckDuckGo is the default — no configuration needed to get started. For higher quality results on technical queries or under heavy load, configure Tavily or Serper.

## Setup

### Gateway-Level Provider

Set a default provider for all users via environment variable:

```bash
# Use DuckDuckGo (default, no config needed)
# or:
WEB_SEARCH_PROVIDER=tavily
TAVILY_API_KEY=tvly-your-key-here
# or:
WEB_SEARCH_PROVIDER=serper
SERPER_API_KEY=your-serper-key-here
```

### Per-User Provider via API

Configure a different provider for a specific user:

```bash
curl -X PUT https://ccag.example.com/admin/search-providers \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "provider_type": "tavily",
    "api_key": "tvly-...",
    "max_results": 5
  }'
```

Then activate it:

```bash
curl -X POST https://ccag.example.com/admin/search-providers/activate \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"provider_type": "tavily"}'
```

Only one provider is active per user at a time. The gateway-level default applies when no per-user provider is configured.

### Test Your Provider

Validate credentials and connectivity before activating:

```bash
curl -X POST https://ccag.example.com/admin/search-providers/test \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"provider_type": "tavily", "api_key": "tvly-..."}'
# Returns: {"success": true, "result_count": 5, "results": [...]}
```

## Custom Provider

A custom provider is any HTTPS endpoint that accepts a POST request and returns search results. This works with self-hosted search engines (SearXNG), internal knowledge bases, or proprietary search APIs.

CCAG sends:

```json
{
  "query": "axum 0.8 router middleware",
  "count": 5
}
```

The endpoint should return a JSON array:

```json
[
  {
    "title": "Axum 0.8 Migration Guide",
    "url": "https://docs.rs/axum/0.8/...",
    "snippet": "In 0.8, middleware is applied via..."
  }
]
```

Field names are flexible: `link` or `url`, `snippet` or `content` or `description` are all accepted.

Configure:

```bash
curl -X PUT https://ccag.example.com/admin/search-providers \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "provider_type": "custom",
    "api_url": "https://search.internal.example.com/api/search",
    "api_key": "optional-key",
    "max_results": 5
  }'
```

Using SearXNG with the custom provider keeps all search queries on your own infrastructure — no queries leave your network.

## Per-Request Limits

Each request can trigger up to `max_uses` searches (default 5, hard cap 10). The loop exits when Bedrock stops requesting searches or the limit is reached. This prevents runaway costs from search-heavy tasks.

## Differences from Anthropic Direct API

- Queries go to your configured provider, not Anthropic's search infrastructure
- Results arrive together after the search completes (no intermediate "Searching..." progress events in Claude Code)
- Citations using `encrypted_content` do not render (titles and URLs still show)
- DuckDuckGo results may differ from the results Anthropic's search returns

Functionally, Claude Code receives search results in the same format regardless of provider. The difference is only in result quality.

## Privacy Considerations

Search queries may contain context from your conversation — file names, variable names, error messages, or internal library names. If you work with proprietary code, consider which search provider receives those queries.

Options in order of increasing privacy:
1. Tavily / Serper: queries go to third-party search APIs under their privacy policies
2. DuckDuckGo: queries go to DuckDuckGo under their no-tracking policy
3. SearXNG via custom provider: queries stay on your infrastructure

## Monitoring

CCAG increments the `ccag.web_searches.total` Prometheus counter for each search execution, labeled by provider and status (`success`, `error`, `rate_limited`). This is useful for tracking search costs when using paid providers.

```bash
# Example Prometheus query: search errors in the last hour
rate(ccag_web_searches_total{status="error"}[1h])
```

See [Metrics](../metrics.md) for the full metric reference.

## See Also

- [Web Search Reference](../web-search.md): full API reference for search provider management
- [Extended Thinking on Bedrock](bedrock-extended-thinking.md): the other major feature unavailable in direct Bedrock mode
- [Configuration](../configuration.md): environment variables for web search
- [FAQ](../faq.md): web search troubleshooting
