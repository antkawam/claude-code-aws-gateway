---
title: "Documentation"
description: "Claude Code AWS Gateway documentation — deployment, configuration, authentication, budgets, CLI, and API reference."
---

# Claude Code AWS Gateway

A purpose-built gateway for running Claude Code through Amazon Bedrock. One-command developer onboarding, real-time budget controls, multi-account routing for latency optimization and data sovereignty, and a full admin portal.

![Analytics Dashboard](images/portal-analytics.png)

![Connect Page](images/portal-connect.png)

## User Guides

- [Getting Started](getting-started.md). Prerequisites, deployment walkthrough, first login, connecting Claude Code.
- [Configuration](configuration.md). Deployment config, environment variables, runtime settings, notifications.
- [Authentication](authentication.md). OIDC setup (Okta, Azure AD, Google, Auth0, Keycloak), virtual keys, SSO.
- [Endpoint Routing](endpoints.md). Multi-endpoint setup, cross-account routing, failover, quota visibility.
- [Budgets](budgets.md). Per-user and per-team spending limits, enforcement policies, shaping.
- [Web Search](web-search.md). Search provider configuration (DuckDuckGo, Tavily, Serper, custom).
- [Upgrading](upgrading.md). Upgrade flow, database migrations, rollback procedures.
- [Metrics](metrics.md). Prometheus scrape endpoint, OTLP export, metric reference, Grafana examples.
- [Office Integration](office-integration.md). Claude for Excel and PowerPoint via Enterprise gateway.
- [CLI Reference](cli-reference.md). `ccag` command reference.
- [FAQ](faq.md). Common questions on setup, features, operations, and troubleshooting.
