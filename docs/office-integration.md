---
title: "Office Integration (Excel & PowerPoint)"
description: "Connect Claude for Excel and Claude for PowerPoint to CCAG for Bedrock-powered Office Add-in access without a Claude subscription."
---

# Office Integration (Excel & PowerPoint)

Claude for Excel and Claude for PowerPoint are Microsoft Office Add-ins that bring Claude into spreadsheets and presentations. When connected via **Enterprise gateway**, they route requests through CCAG to Bedrock — no Claude subscription required.

## How It Works

The Office add-ins are served from `pivot.claude.ai` and run inside a sandboxed iframe within Excel or PowerPoint. When you select "Enterprise gateway" during sign-in, the add-in sends requests to your CCAG instance instead of Anthropic's API.

```
Office Add-in (pivot.claude.ai iframe)
  → HTTPS POST /v1/messages → CCAG → Bedrock
  → HTTPS GET  /v1/models   → CCAG (static model list)
```

### Gateway Requirements

The add-ins require two endpoints:

| Endpoint | Method | Purpose |
|---|---|---|
| `/v1/messages` | POST | Send messages (streaming and non-streaming) |
| `/v1/models` | GET | List available Claude models |

Both endpoints are included in CCAG. The `/v1/models` endpoint returns a static list of supported models in the Anthropic API format.

### CORS

The add-ins run in a browser context from `https://pivot.claude.ai`, so cross-origin requests require CORS headers. CCAG allows origins matching `https://claude.ai` and `https://*.claude.ai`. No other origins are permitted.

### Authentication

The add-in sends the API token as a Bearer token in the `Authorization` header. **Virtual keys (`sk-proxy-...`) are required** — OIDC/SSO tokens are not supported because the add-in has no browser-based login flow for your identity provider.

Create a virtual key in the admin portal or via CLI:

```bash
ccag keys add office-user
```

## Setup

### Prerequisites

- CCAG deployed and accessible over **HTTPS** (the add-in rejects plain HTTP gateway URLs)
- A virtual key created in the gateway
- Microsoft Excel or PowerPoint (desktop with M365 subscription, or [free web version](https://www.office.com))

### Install the Add-in

1. Open Excel or PowerPoint
2. Install the Claude add-in from the [Microsoft Marketplace](https://marketplace.microsoft.com/en-us/product/saas/wa200009404)
3. Open the add-in from the ribbon (Home → Add-ins on Windows, Tools → Add-ins on Mac)

### Connect to Your Gateway

1. On the sign-in screen, select **Enterprise gateway**
2. Enter your **Gateway URL** (e.g., `https://ccag.example.com`)
3. Enter your **API token** — a virtual key from your gateway (`sk-proxy-...`)
4. The add-in validates the connection by calling `/v1/models`

Once connected, use Claude in the add-in sidebar as normal. All requests route through your gateway to Bedrock.

### Local Development

For local testing, the gateway must be exposed over HTTPS. Use a tunnel:

```bash
# Cloudflare Tunnel (recommended — no interstitial page)
cloudflared tunnel --url http://localhost:8080

# Or ngrok (free tier shows an interstitial that blocks add-in requests)
ngrok http 8080
```

Use the generated HTTPS URL as the gateway URL in the add-in.

## Troubleshooting

### "Could not reach gateway" on connection

- Verify the gateway URL is HTTPS (not HTTP)
- Check the gateway is running and `/v1/models` returns a response
- If using a tunnel, ensure it's active and the URL hasn't changed

### "Claude is temporarily unavailable"

Check the gateway logs for Bedrock errors:

```bash
docker compose logs gateway --tail 50
```

Common causes:
- **Anthropic use case form not submitted**: Complete the First Time Use form in the AWS Bedrock console (Model catalog → select any Anthropic model → submit the form). Access is granted immediately.
- **Model not enabled**: Ensure the requested model has access enabled in your AWS account's Bedrock console.

### CORS errors in browser console

The add-in runs from `https://pivot.claude.ai`. CCAG's CORS policy allows `*.claude.ai` origins. If you see CORS errors, verify you're running a version of CCAG that includes the CORS layer (v1.1.0+).
