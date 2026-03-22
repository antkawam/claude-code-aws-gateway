# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
