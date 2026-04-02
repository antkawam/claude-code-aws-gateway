---
title: "Enable Extended Thinking for Claude Code on Bedrock"
description: "CLAUDE_CODE_USE_BEDROCK=1 disables extended thinking on the client side. CCAG presents as the Anthropic API so Claude Code enables its full feature set while inference still runs through your AWS account."
---

# Enable Extended Thinking for Claude Code on Bedrock

Extended thinking is one of Claude Code's most powerful features for complex tasks — it lets Claude reason through multi-step problems before responding. But if you connect Claude Code directly to Bedrock using `CLAUDE_CODE_USE_BEDROCK=1`, extended thinking is silently disabled. This is not a Bedrock limitation. It is a client-side decision in Claude Code that cannot be overridden with environment variables.

## Why CLAUDE_CODE_USE_BEDROCK Disables Extended Thinking

When you set `CLAUDE_CODE_USE_BEDROCK=1`, Claude Code detects that it is talking to Bedrock and switches to a reduced capability mode. This is a deliberate feature-detection heuristic in the Claude Code client: it assumes that Bedrock does not support certain features and skips them rather than sending requests that might fail.

The features disabled in Bedrock mode include:

- Extended thinking (`thinking` content blocks)
- Some tool use capabilities (gated behind beta headers Bedrock does not accept)
- Web search (Anthropic's server-side `web_search` tool)

Bedrock actually does support extended thinking — the inference runs correctly if you send a valid request. The problem is that Claude Code never sends the request.

## How CCAG Fixes This

CCAG presents as the Anthropic Messages API. Claude Code sets `ANTHROPIC_BASE_URL` and does not know it is talking to a gateway. Because Claude Code believes it is connected to Anthropic directly, it enables its full feature set.

The gateway then translates each request from Anthropic format to Bedrock format:

1. Model IDs are mapped: `claude-opus-4-5` becomes `us.anthropic.claude-opus-4-5-20251101-v1:0` (or the equivalent inference profile for your region)
2. `thinking` blocks in the request are passed through to Bedrock unchanged
3. Bedrock returns thinking content blocks in the response
4. CCAG passes them through to Claude Code unchanged

Claude Code renders the thinking output as it normally would on the direct API.

## Setup

### 1. Deploy CCAG

Follow [Getting Started](../getting-started.md) to deploy the gateway. For a single developer, the Docker Compose option takes about 5 minutes.

### 2. Connect Claude Code

The portal's **Connect** page generates a one-command setup script:

```bash
curl -fsSL https://your-gateway/setup | sh
```

This sets `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` in your shell profile. No other changes are needed.

If you prefer to configure manually:

```bash
export ANTHROPIC_BASE_URL="https://your-gateway"
export ANTHROPIC_API_KEY="sk-proxy-your-key-here"
```

Do not set `CLAUDE_CODE_USE_BEDROCK`. Once `ANTHROPIC_BASE_URL` points to CCAG, Bedrock is no longer the direct endpoint.

### 3. Verify Extended Thinking Is Active

Start Claude Code and look for the thinking indicator when working on a complex task. Alternatively, check the gateway logs for requests containing `"thinking"` in the Bedrock request body:

```bash
RUST_LOG=debug cargo run  # or check container logs
```

A log line like `bedrock request contains thinking: true` confirms the feature is active.

## What Changes vs. Direct Bedrock

| Feature | Direct Bedrock | Through CCAG |
|---|---|---|
| Extended thinking | Disabled by client | Enabled |
| Tool use | Partial | Full |
| Web search | Disabled | Enabled (DuckDuckGo, Tavily, or Serper) |
| Inference location | Single region | Configurable (multi-region, multi-account) |
| Model IDs | Bedrock ARNs required | Anthropic model names |

Everything else is equivalent — requests are forwarded to Bedrock, inference runs in your account, and you are billed on your Bedrock invoice.

## Model ID Mapping

CCAG automatically maps Anthropic model IDs to the appropriate Bedrock inference profile for your region. The mapping uses the AWS SDK's detected region at startup.

For example:
- `claude-opus-4-5-20251101` maps to `us.anthropic.claude-opus-4-5-20251101-v1:0` in US regions
- The same model maps to `eu.anthropic.claude-opus-4-5-20251101-v1:0` in European regions

Model mappings are configurable in the admin portal (Settings > Model Mappings) if you need to override the defaults or use application inference profiles.

## Using Inference Profiles for Extended Thinking

Newer Claude models (4.5+) require inference profiles — you cannot invoke them directly by model ID. Cross-region inference profiles are the recommended approach because they provide redundancy: Bedrock routes to the nearest healthy region within the geographic scope.

CCAG creates and uses inference profiles automatically. You do not need to manage this manually unless you want to use application inference profiles for cost tracking or quota management.

See [Endpoint Routing](../endpoints.md) for how to configure multi-region pools.

## Troubleshooting

**Extended thinking still not appearing:**

Confirm that `CLAUDE_CODE_USE_BEDROCK` is not set in your environment:

```bash
echo $CLAUDE_CODE_USE_BEDROCK  # should be empty
```

**Model not available error:**

Check that your Bedrock account has model access enabled for the Claude model you are trying to use. Go to the [Bedrock console](https://console.aws.amazon.com/bedrock/home#/modelaccess) and enable access for Claude models.

**Timeout on thinking requests:**

Extended thinking requests can take longer than standard requests. The default CCAG timeout is 10 minutes, which is sufficient for most tasks. If you hit timeouts on very long reasoning tasks, contact your gateway admin to increase the timeout.

## See Also

- [Getting Started](../getting-started.md): initial deployment and developer onboarding
- [Web Search on Bedrock](web-search-bedrock.md): enabling the web search tool
- [Endpoint Routing](../endpoints.md): multi-region configuration
- [Configuration](../configuration.md): environment variables
