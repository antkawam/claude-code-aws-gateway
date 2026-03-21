# Web Search

AWS Bedrock does not support Anthropic's `web_search` server tool. When Claude Code sends a request containing a web search tool use, CCAG intercepts the request, executes the search using your configured provider, and returns results in the format Claude Code expects.

## How It Works

1. Claude Code sends a request with a `web_search` tool definition
2. CCAG replaces the server tool with a regular tool definition that Bedrock accepts
3. When Bedrock returns a `tool_use` block for web search, CCAG executes the search
4. Search results are formatted as `web_search_tool_result` content blocks
5. CCAG re-invokes Bedrock with the results appended to the conversation
6. Steps 3-5 repeat until Bedrock stops requesting searches (up to the per-request limit)

The search loop runs server-side. Claude Code sees the final response as if the search was handled natively.

## Supported Providers

| Provider | Auth | Notes |
|---|---|---|
| DuckDuckGo | None (free) | HTML scraping. May be rate-limited under heavy use. Returns HTTP 403 from some datacenter IPs. |
| Tavily | API key | AI-optimized search with structured results. Free tier at [tavily.com](https://tavily.com). |
| Serper | API key | Google Search results via API. Fast and accurate. API keys at [serper.dev](https://serper.dev). |
| Custom | Optional API key | Any POST endpoint returning search results. Works with SearXNG, internal search APIs, or custom proxies. |

DuckDuckGo is the default. No API key is required.

## Configuring a Provider

Provider configuration is per-user. Set it through the admin portal (Web Search section) or the API.

### Set a Provider

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

`max_results` controls how many results each search returns (1-20, default 5).

### Activate a Provider

If you have multiple providers configured, activate the one you want to use:

```bash
curl -X POST https://ccag.example.com/admin/search-providers/activate \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{"provider_type": "tavily"}'
```

Only one provider is active per user at a time. Activating one disables the others.

### Test a Provider

Validate that credentials and connectivity work before activating:

```bash
curl -X POST https://ccag.example.com/admin/search-providers/test \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "provider_type": "serper",
    "api_key": "..."
  }'
```

Returns `{"success": true, "result_count": N, "results": [...]}` on success.

### Delete a Provider

```bash
curl -X DELETE https://ccag.example.com/admin/search-providers/serper \
  -H "authorization: Bearer $TOKEN"
```

## Custom Provider Setup

A custom provider is any HTTPS endpoint that accepts a POST request and returns search results as JSON.

### Request Format

CCAG sends:

```json
{
  "query": "search terms",
  "count": 5
}
```

With `Authorization: Bearer {api_key}` if an API key is configured.

### Response Format

The endpoint must return a JSON array (or an object with a `results` array):

```json
[
  {
    "title": "Page Title",
    "url": "https://example.com/page",
    "snippet": "Excerpt from the page..."
  }
]
```

Field names are flexible: `link` or `url`, `snippet` or `content` or `description` are all accepted.

### Configuration

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

## Differences from Anthropic Direct API

- Search queries go to your configured provider, not Anthropic's infrastructure
- Results arrive together after the search completes (no intermediate "Searching..." progress events)
- Citations referencing `encrypted_content` do not work (Claude Code still shows titles and URLs)
- The `web_search` tool is replaced with a regular tool definition for Bedrock

## Per-Request Limits

Each request can trigger up to `max_uses` web searches (default 5, hard cap 10). The search loop exits when Bedrock stops requesting searches or the limit is reached.

## Privacy

Search queries may contain context from your conversation. If you are working with sensitive code or proprietary information, consider which search provider you trust with those queries. Self-hosted options (SearXNG via the custom provider) keep queries on your infrastructure.

## API Reference

| Method | Path | Description |
|---|---|---|
| `GET` | `/admin/search-providers` | List all configured providers for current user |
| `PUT` | `/admin/search-providers` | Create or update a provider config |
| `POST` | `/admin/search-providers/activate` | Switch active provider |
| `POST` | `/admin/search-providers/test` | Test a provider config |
| `DELETE` | `/admin/search-providers/{type}` | Delete a provider config |

## See Also

- [Configuration](configuration.md): general gateway settings
- [FAQ](faq.md): web search troubleshooting
- [Metrics](metrics.md): `ccag.web_searches.total` metric
