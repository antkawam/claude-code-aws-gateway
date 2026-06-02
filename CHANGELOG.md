# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.7.0] - 2026-06-03

### Fixed

- **Model-mapping greedy-prefix misrouting** (the `claude-opus-4-8` → retired Opus 4.0 incident): the model cache matched requested IDs with `starts_with` ordered by prefix length, so a request for `claude-opus-4-8` could shadow-match a row keyed `claude-opus-4` and silently route to a retired inference profile, causing Bedrock 400s. Hardened across the read and write paths so the bug class cannot recur.

### Changed

- **Exact-match model cache** (Slice 1): `ModelCache` now stores mappings in a `HashMap` and looks them up by exact equality (O(1)), not prefix matching. The hardcoded fallback was likewise converted from greedy `starts_with` catch-alls to exact-match arms — a future `claude-sonnet-4-8` now passes through to discovery instead of misrouting to retired Sonnet 4.0.
- **Read-time date-suffix fallback** (Slice 2): on an exact miss, the cache retries once against the date-stripped form, but only when the stripped form is minor-version-bearing (e.g. `claude-opus-4-8`), never a bare major (`claude-opus-4`) — so dated aliases resolve without reopening the greedy shadow.
- **`discover_model` persists the exact requested model ID** (Slice 3) as `anthropic_prefix`, not the lossy date-stripped form (the root cause of the original incident). Profile selection now runs three ordered passes — exact stem, versioned stem, fuzzy contains — to disambiguate variants like `claude-opus-4-8-thinking`.

### Added

- **Cold-start model seed** (Slice 4): a curated `model_seed.json` is embedded at compile time and inserted at startup with `ON CONFLICT DO NOTHING`, so a request for a known model succeeds on a fresh deploy before any discovery tick. It is a floor, not a ceiling — discovery still handles anything unseeded. Explicitly not a runtime/call-home fetch.

## [1.2.0] - 2026-03-30

### Added

- **Websearch admin control**: three configurable modes for web search behavior:
  - **Enabled** (default): per-user provider configuration, each user chooses their own search provider
  - **Disabled**: web search tool silently stripped from requests server-side; setup script pushes `permissions.deny: ["WebSearch"]` to Claude Code clients
  - **Global**: admin configures a single search provider (DuckDuckGo, Tavily, Serper, or Custom) used for all users; per-user provider config is overridden
- Admin API: `GET/PUT /admin/websearch-mode` for reading and setting the mode, with provider config for global mode
- Websearch mode exposed in `GET /admin/settings` so the portal and clients can read it
- Portal Settings page: three-button mode selector with global provider config form (type, API key, URL, max results)
- Portal nav: Web Search menu item hidden when mode is disabled
- API key masking: global provider API key never returned in GET responses (replaced with `has_api_key` boolean)
- `SearchProvider::from_global_config()`: constructs a search provider from admin-configured JSON with validation (provider_type required, api_key required for Tavily/Serper, api_url required for Custom, max_results clamped to 1-20)
- `extract_web_search_tool_with_mode()`: mode-aware tool extraction that strips web_search tools when mode is disabled
- Setup script WebSearch deny injection covers both SSO and virtual key setup paths
- 33 new tests (14 integration, 19 unit) covering all websearch admin control behavior

### Changed

- `translate()` now accepts a `websearch_mode` parameter; handler reads mode from DB settings on each request
- Deploy script now uses GHCR (GitHub Container Registry) instead of ECR for image storage

## [1.1.0] - 2026-03-25

### Added

- Per-IDP `user_claim` configuration: controls which JWT claim is used as the user identifier. Configurable via portal UI, admin API, or `OIDC_USER_CLAIM` env var. Supports `email`, `preferred_username`, `upn`, `oid`, `name`, `sub`, or `auto` (default fallback chain).
- Extract `preferred_username`, `upn`, `oid`, and `name` claims from OIDC JWTs (previously only `email` and `sub` were extracted).
- OIDC login now requests `email` and `profile` scopes in addition to `openid`, for broader claim availability.

### Changed

- OIDC user identification now prefers `email` > `preferred_username` > `upn` > `name` > `sub` by default (was `sub` only). This fixes Entra ID (Azure AD) compatibility where `sub` is an opaque pairwise hash.
- EventBridge notification `detail` no longer includes `source`, `event_type`, or `timestamp` fields — these are redundant with the EventBridge envelope (`source`, `detail-type`, `time`). Webhook and SNS payloads are unchanged. **Breaking** for consumers parsing these fields from EventBridge `detail`.

### Fixed

- OIDC users from Entra ID no longer show up as opaque pairwise hashes. The gateway now uses the email or preferred_username claim for user identification.
- CLI login (`ccag login`) redirect URL no longer breaks when the IDP audience is a UUID client_id (common with Entra ID). The audience is only used as a redirect hostname when it looks like a domain name.
- IAM policy: split Bedrock invoke and ListInferenceProfiles into separate statements so the wildcard resource only applies to List, not Invoke.
- Documentation: clarified `OIDC_ISSUER` is a one-time bootstrap seed (not a persistent override). Changing it after first startup has no effect unless the IDP is deleted from the portal first.
- Documentation: expanded `ADMIN_USERS` description with OIDC subject identifier details and startup seeding behavior.
- Documentation: renamed confusing "Notifications" env var section to "Infrastructure Alarms" and corrected inaccurate `alarmWebhookUrl` reference. Separated from app-level notifications.
- Documentation: added separate EventBridge payload example showing envelope structure without redundant fields.

## [1.0.0] - 2026-03-21

### Added

- API gateway translating Anthropic Messages API to Amazon Bedrock
- Streaming support (SSE translation from Bedrock binary events)
- Multi-user authentication: virtual API keys, OIDC SSO (any provider), session tokens
- Admin portal SPA with dashboard, key management, team management, and analytics
- Budget enforcement with per-team and per-user spend limits
- Rate limiting (per-key sliding window)
- Web search interception (translates Anthropic's web_search tool via DuckDuckGo)
- Multi-endpoint support (route teams to different AWS accounts/regions)
- Model ID mapping (automatic region-based Bedrock inference profile resolution)
- Prometheus metrics and optional OTLP export
- AWS CDK infrastructure (ECS Fargate + RDS Postgres + ALB)
- Database migrations (automatic on startup)
- CloudWatch alarms for operational monitoring
- Autoscaling based on CPU and memory utilization
