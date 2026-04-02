---
title: "Prometheus Metrics & OTLP Export"
description: "Monitor CCAG with Prometheus scraping and OpenTelemetry OTLP export. Metric reference, Grafana examples, and alerting setup."
---

# Metrics

CCAG exposes operational metrics via two channels:

1. **Prometheus**: scrape `GET /metrics` (requires admin auth)
2. **OTLP**: push to any OpenTelemetry-compatible collector via gRPC

Both channels observe the same instruments. Enabling one does not disable the other.

## Prometheus Scrape Endpoint

```
GET /metrics
Authorization: Bearer <admin-token>
```

Returns metrics in Prometheus text exposition format (`text/plain; version=0.0.4`).

### Scrape Config Example

```yaml
scrape_configs:
  - job_name: ccag
    scrape_interval: 30s
    scheme: https
    metrics_path: /metrics
    authorization:
      credentials: "<admin-session-token-or-api-key>"
    static_configs:
      - targets: ["ccag.example.com"]
```

## OTLP Export

Set the `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable to enable gRPC metric push:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
```

Metrics are exported every 60 seconds. The exporter uses gRPC (Tonic). Both Prometheus and OTLP can run simultaneously.

## Metric Reference

All metrics use the `ccag` prefix (dots in instrument names become underscores in Prometheus).

### Request Metrics

| Instrument | Type | Labels | Description |
|---|---|---|---|
| `ccag.requests.total` | Counter | `model`, `streaming`, `status` | Total proxy requests |
| `ccag.request.duration_ms` | Histogram | `model`, `streaming`, `status` | Request duration in milliseconds |
| `ccag.requests.in_flight` | UpDownCounter | | Currently in-flight requests |

### Token Metrics

| Instrument | Type | Labels | Description |
|---|---|---|---|
| `ccag.tokens.input` | Counter | `model` | Total input tokens processed |
| `ccag.tokens.output` | Counter | `model` | Total output tokens generated |
| `ccag.tokens.cache_read` | Counter | `model` | Cache read input tokens |
| `ccag.tokens.cache_write` | Counter | `model` | Cache write (creation) tokens |

### Tool & Search Metrics

| Instrument | Type | Labels | Description |
|---|---|---|---|
| `ccag.tool_calls.total` | Counter | `tool`, `type` | Tool calls observed (`type`: `builtin` or `mcp`) |
| `ccag.web_searches.total` | Counter | | Web searches executed via interception |

### Error & Throttle Metrics

| Instrument | Type | Labels | Description |
|---|---|---|---|
| `ccag.errors.total` | Counter | `error_type`, `endpoint_id` | Bedrock/upstream errors |
| `ccag.bedrock.throttles.total` | Counter | `model`, `endpoint_id` | Bedrock throttling events |
| `ccag.rate_limits.total` | Counter | | Gateway rate limit rejections |
| `ccag.auth_failures.total` | Counter | `reason` | Authentication failures |

### Operational Metrics

| Instrument | Type | Labels | Description |
|---|---|---|---|
| `ccag.spend_flush_errors.total` | Counter | | Spend tracker flush failures |

## Grafana Dashboard

Import a basic dashboard by creating a new dashboard and adding these panels:

- **Request rate**: `rate(ccag_requests_total[5m])`
- **Error rate**: `rate(ccag_errors_total[5m])`
- **Throttle rate**: `rate(ccag_bedrock_throttles_total[5m])` (group by `endpoint_id`)
- **In-flight**: `ccag_requests_in_flight`
- **p99 latency**: `histogram_quantile(0.99, rate(ccag_request_duration_ms_bucket[5m]))`
- **Token throughput**: `rate(ccag_tokens_input[5m]) + rate(ccag_tokens_output[5m])`

## See Also

- [Configuration](configuration.md): OTLP and logging settings
- [Endpoints](endpoints.md): per-endpoint error and throttle metrics
- [Budgets](budgets.md): spend tracking that feeds budget enforcement
