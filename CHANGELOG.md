# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
