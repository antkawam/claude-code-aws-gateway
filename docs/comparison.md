---
title: "CCAG vs LiteLLM vs Direct Bedrock — Claude Code Gateway Comparison"
description: "Comparing Claude Code AWS Gateway with LiteLLM and direct Bedrock mode. A LiteLLM alternative purpose-built for Claude Code on Bedrock with extended thinking, web search, team budgets, and OIDC SSO under a single MIT license."
---

# Choosing How to Run Claude Code on AWS

There are three common approaches to running Claude Code through Amazon Bedrock. This page compares them so you can pick the right one for your team.

## The Core Problem

When Claude Code connects to Bedrock directly (`CLAUDE_CODE_USE_BEDROCK=1`), it detects a Bedrock backend and disables several features client-side — extended thinking, web search, and some tool use capabilities. This is a client-side behavior in Claude Code, not a Bedrock limitation per se.

The workaround is to present a Bedrock-backed service as the Anthropic Messages API (`ANTHROPIC_BASE_URL`), so Claude Code enables its full feature set while inference still runs through your AWS account.

## Feature Comparison

| Feature | Direct Bedrock | CCAG | LiteLLM |
|---|---|---|---|
| **Claude Code Features** | | | |
| Extended thinking | No (disabled client-side) | Yes | Yes (since v1.63.0) |
| Web search (`web_search` tool) | No (Bedrock doesn't implement it) | Yes (built-in: DuckDuckGo, Tavily, Serper, or custom) | Yes (Anthropic passthrough; [known bug](https://github.com/BerriAI/litellm/issues/17737) with multi-turn + custom tools) |
| Claude Code-specific translation | N/A | Yes (model ID mapping, beta header filtering, `cache_control` sanitization, inference profile resolution) | Generic passthrough |
| Presents as Anthropic API | No | Yes | Yes |
| **Team Management** | | | |
| Virtual API keys | N/A | Yes (MIT) | Yes (open source) |
| Per-user/team budgets | N/A | Yes (MIT) — notify, throttle, or block | Basic (open source) / advanced (Enterprise) |
| Budget notifications (webhook, SNS, EventBridge) | N/A | Yes (MIT) | Webhook (open source) |
| OIDC SSO | N/A | Yes (MIT) — multi-IDP | Yes (free for ≤5 users, Enterprise beyond) |
| SCIM 2.0 provisioning | N/A | Yes (MIT) | No |
| Admin portal with analytics | N/A | Yes (MIT) — built-in SPA | Yes (open source, advanced analytics Enterprise) |
| **Infrastructure** | | | |
| Multi-account/region endpoint pool | N/A | Yes — sticky user, primary/fallback, round robin | Yes — router-level |
| Per-endpoint throttle tracking | N/A | Yes (rolling window) | Limited |
| Model ID auto-mapping | N/A | Yes (region-aware inference profiles) | Manual config |
| Prompt caching | Bedrock native | Preserved (passes `cache_control` through) | Preserved |
| **Operations** | | | |
| Deployment complexity | None | Single container + Postgres | Single container + Postgres (or Redis) |
| Language | N/A | Rust (single binary, ~15 MB) | Python |
| Latency overhead | None | 1–5 ms | 5–20 ms |
| Prometheus metrics | N/A | Yes | Yes (Enterprise) |
| OTLP export | N/A | Yes | No |
| **Licensing** | | | |
| License | N/A | MIT — all features | MIT (core) / Enterprise (team features) |
| Self-hosted | N/A | Yes | Yes |

## When to Use Each

### Direct Bedrock (`CLAUDE_CODE_USE_BEDROCK=1`)

Best when:
- You are a solo developer or a small team where shared infrastructure is overkill
- You do not need extended thinking or web search
- You want zero operational overhead

Limitations:
- Extended thinking, web search, and some tool use are disabled
- No team management, budgets, or SSO
- Each developer needs their own AWS credentials

### CCAG

Best when:
- Your team needs the full Claude Code feature set (extended thinking, web search, tool use) while running through Bedrock
- You need per-user or per-team budget enforcement
- You want OIDC SSO so developers do not need AWS credentials
- You want to pool quota across multiple AWS accounts or regions to avoid 429 errors
- You want all features under a single MIT license with no enterprise tier

Limitations:
- Claude Code-specific — not a general-purpose LLM proxy
- Requires a Postgres database
- Only supports Anthropic Claude models on Bedrock

### LiteLLM

Best when:
- You need a general-purpose LLM proxy that supports 100+ models across providers (OpenAI, Anthropic, Azure, etc.)
- You already run LiteLLM for other workloads and want to consolidate
- You need provider-level routing across different LLM vendors, not just Bedrock regions

Limitations:
- Advanced team features (SSO beyond 5 users, audit logs, advanced budget controls) require an Enterprise license
- General-purpose proxy — not optimized for Claude Code-specific behaviors (model ID mapping, beta flag handling, `cache_control` sanitization, inference profile resolution)
- Web search passthrough has a [known bug](https://github.com/BerriAI/litellm/issues/17737) with multi-turn conversations combining web search and custom tools

## Architecture Differences

**CCAG** is purpose-built for Claude Code on Bedrock. It translates Anthropic API requests to Bedrock format, handles model ID mapping with region-aware inference profiles, sanitizes `cache_control` fields Bedrock does not accept, strips Bedrock-specific SSE metadata, and intercepts web search tool use. The gateway is a single Rust binary (~15 MB) with an embedded admin portal.

**LiteLLM** is a general-purpose LLM proxy written in Python. It supports 100+ models across dozens of providers. Core features (virtual keys, basic budgets, analytics) are open source. Advanced team features (SSO beyond 5 users, audit logs, advanced budget controls) require an Enterprise license.

**Direct Bedrock** is the simplest option — Claude Code talks to Bedrock directly using the AWS SDK. No proxy, no translation, no operational overhead. But Claude Code detects the Bedrock backend and disables features accordingly.

## Migrating from Direct Bedrock to CCAG

If you are currently using `CLAUDE_CODE_USE_BEDROCK=1` and want to switch:

1. Deploy CCAG ([Getting Started](getting-started.md))
2. Remove `CLAUDE_CODE_USE_BEDROCK` from your environment
3. Set `ANTHROPIC_BASE_URL` to your CCAG instance URL
4. Set `ANTHROPIC_API_KEY` to a virtual key from the CCAG portal (or use OIDC login)

```bash
# Before (direct Bedrock)
export CLAUDE_CODE_USE_BEDROCK=1
export AWS_PROFILE=my-profile

# After (through CCAG)
export ANTHROPIC_BASE_URL=https://ccag.example.com
export ANTHROPIC_API_KEY=sk-proxy-xxxxx
```

Claude Code will now use its full feature set. No other configuration changes are needed.

## Migrating from LiteLLM to CCAG

If you are using LiteLLM primarily for Claude Code on Bedrock:

1. Deploy CCAG alongside LiteLLM (they can coexist)
2. Point Claude Code's `ANTHROPIC_BASE_URL` to CCAG
3. Keep LiteLLM for non-Claude workloads if needed

CCAG does not replace LiteLLM for multi-vendor routing. If you use LiteLLM for OpenAI, Azure, and other providers, keep it for those. Use CCAG specifically for Claude Code on Bedrock where you need the full feature set.

## See Also

- [Getting Started](getting-started.md): deploy CCAG in under 5 minutes
- [Endpoint Routing](endpoints.md): multi-account/region failover setup
- [Configuration](configuration.md): full environment variable reference
