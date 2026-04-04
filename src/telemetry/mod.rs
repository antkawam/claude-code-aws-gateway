use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, MeterProvider};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::Registry;

/// Holds all metric instruments for the proxy.
pub struct Metrics {
    pub request_count: Counter<u64>,
    pub request_duration: Histogram<f64>,
    pub input_tokens: Counter<u64>,
    pub output_tokens: Counter<u64>,
    pub cache_read_tokens: Counter<u64>,
    pub cache_write_tokens: Counter<u64>,
    pub tool_calls: Counter<u64>,
    pub rate_limit_count: Counter<u64>,
    pub web_search_count: Counter<u64>,
    pub error_count: Counter<u64>,
    pub auth_failure_count: Counter<u64>,
    pub spend_flush_errors: Counter<u64>,
    pub bedrock_throttle_count: Counter<u64>,
    pub endpoint_request_count: Counter<u64>,
    pub in_flight_requests: opentelemetry::metrics::UpDownCounter<i64>,
    prometheus_registry: Registry,
}

impl Metrics {
    pub fn new(otlp_endpoint: Option<&str>) -> anyhow::Result<(Self, SdkMeterProvider)> {
        let registry = Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()?;

        let mut builder = SdkMeterProvider::builder().with_reader(exporter);

        // If OTLP endpoint is configured, add a second reader for metric export
        if let Some(endpoint) = otlp_endpoint {
            let otlp_exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .build()?;
            let periodic_reader =
                opentelemetry_sdk::metrics::PeriodicReader::builder(otlp_exporter)
                    .with_interval(std::time::Duration::from_secs(60))
                    .build();
            builder = builder.with_reader(periodic_reader);
            tracing::info!(%endpoint, "OTLP metric export enabled (60s interval)");
        }

        let provider = builder.build();

        let meter = provider.meter("ccag");

        let metrics = Self {
            request_count: meter
                .u64_counter("ccag.requests.total")
                .with_description("Total number of proxy requests")
                .build(),
            request_duration: meter
                .f64_histogram("ccag.request.duration_ms")
                .with_description("Request duration in milliseconds")
                .build(),
            input_tokens: meter
                .u64_counter("ccag.tokens.input")
                .with_description("Total input tokens processed")
                .build(),
            output_tokens: meter
                .u64_counter("ccag.tokens.output")
                .with_description("Total output tokens generated")
                .build(),
            cache_read_tokens: meter
                .u64_counter("ccag.tokens.cache_read")
                .with_description("Total cache read input tokens")
                .build(),
            cache_write_tokens: meter
                .u64_counter("ccag.tokens.cache_write")
                .with_description("Total cache write (creation) input tokens")
                .build(),
            tool_calls: meter
                .u64_counter("ccag.tool_calls.total")
                .with_description("Total tool calls observed in requests")
                .build(),
            rate_limit_count: meter
                .u64_counter("ccag.rate_limits.total")
                .with_description("Total number of rate-limited requests")
                .build(),
            web_search_count: meter
                .u64_counter("ccag.web_searches.total")
                .with_description("Total web searches executed via interception")
                .build(),
            error_count: meter
                .u64_counter("ccag.errors.total")
                .with_description("Total Bedrock/upstream errors")
                .build(),
            auth_failure_count: meter
                .u64_counter("ccag.auth_failures.total")
                .with_description("Total authentication failures")
                .build(),
            spend_flush_errors: meter
                .u64_counter("ccag.spend_flush_errors.total")
                .with_description("Total spend flush failures")
                .build(),
            bedrock_throttle_count: meter
                .u64_counter("ccag.bedrock.throttles.total")
                .with_description("Total Bedrock throttling events")
                .build(),
            endpoint_request_count: meter
                .u64_counter("ccag.endpoint.requests.total")
                .with_description("Total requests per endpoint")
                .build(),
            in_flight_requests: meter
                .i64_up_down_counter("ccag.requests.in_flight")
                .with_description("Currently in-flight proxy requests")
                .build(),
            prometheus_registry: registry,
        };

        Ok((metrics, provider))
    }

    /// Record a completed request.
    pub fn record_request(&self, model: &str, streaming: bool, duration_ms: f64, status: &str) {
        let attrs = &[
            KeyValue::new("model", model.to_string()),
            KeyValue::new("streaming", streaming.to_string()),
            KeyValue::new("status", status.to_string()),
        ];
        self.request_count.add(1, attrs);
        self.request_duration.record(duration_ms, attrs);
    }

    /// Record token usage.
    pub fn record_tokens(
        &self,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) {
        let attrs = &[KeyValue::new("model", model.to_string())];
        self.input_tokens.add(input, attrs);
        self.output_tokens.add(output, attrs);
        if cache_read > 0 {
            self.cache_read_tokens.add(cache_read, attrs);
        }
        if cache_write > 0 {
            self.cache_write_tokens.add(cache_write, attrs);
        }
    }

    /// Record tool calls.
    pub fn record_tools(&self, tools: &[String]) {
        for tool in tools {
            let tool_type = if tool.starts_with("mcp__") {
                "mcp"
            } else {
                "builtin"
            };
            self.tool_calls.add(
                1,
                &[
                    KeyValue::new("tool", tool.clone()),
                    KeyValue::new("type", tool_type),
                ],
            );
        }
    }

    /// Record a rate limit hit.
    pub fn record_rate_limit(&self) {
        self.rate_limit_count.add(1, &[]);
    }

    /// Record web search executions.
    pub fn record_web_searches(&self, count: u64) {
        self.web_search_count.add(count, &[]);
    }

    /// Record a Bedrock/upstream error.
    pub fn record_error(&self, error_type: &str, endpoint_id: Option<&str>) {
        let mut attrs = vec![KeyValue::new("error_type", error_type.to_string())];
        if let Some(ep) = endpoint_id {
            attrs.push(KeyValue::new("endpoint_id", ep.to_string()));
        }
        self.error_count.add(1, &attrs);
    }

    /// Record an authentication failure.
    pub fn record_auth_failure(&self, reason: &str) {
        self.auth_failure_count
            .add(1, &[KeyValue::new("reason", reason.to_string())]);
    }

    /// Record a spend flush error.
    pub fn record_spend_flush_error(&self) {
        self.spend_flush_errors.add(1, &[]);
    }

    /// Record a request routed through a specific endpoint.
    pub fn record_endpoint_request(&self, endpoint_id: &str) {
        self.endpoint_request_count
            .add(1, &[KeyValue::new("endpoint_id", endpoint_id.to_string())]);
    }

    /// Record a Bedrock throttling event.
    pub fn record_bedrock_throttle(&self, model: &str, endpoint_id: Option<&str>) {
        let mut attrs = vec![KeyValue::new("model", model.to_string())];
        if let Some(ep) = endpoint_id {
            attrs.push(KeyValue::new("endpoint_id", ep.to_string()));
        }
        self.bedrock_throttle_count.add(1, &attrs);
    }

    /// Render Prometheus metrics as text.
    pub fn prometheus_text(&self) -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let metric_families = self.prometheus_registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_bedrock_throttle() {
        let (metrics, _provider) = Metrics::new(None).unwrap();
        // Should not panic when recording throttle events
        metrics.record_bedrock_throttle("claude-sonnet-4-6-20250514", None);
        metrics.record_bedrock_throttle("claude-haiku-4-5-20251001", Some("ep-123"));
        // Verify the counter appears in prometheus output
        let text = metrics.prometheus_text();
        assert!(
            text.contains("ccag_bedrock_throttles_total"),
            "Prometheus output should contain bedrock throttle counter"
        );
    }

    #[test]
    fn test_in_flight_requests() {
        let (metrics, _provider) = Metrics::new(None).unwrap();
        metrics.in_flight_requests.add(1, &[]);
        metrics.in_flight_requests.add(1, &[]);
        metrics.in_flight_requests.add(-1, &[]);
        let text = metrics.prometheus_text();
        assert!(
            text.contains("ccag_requests_in_flight"),
            "Prometheus output should contain in-flight gauge"
        );
    }

    #[test]
    fn test_record_endpoint_request() {
        let (metrics, _provider) = Metrics::new(None).unwrap();
        metrics.record_endpoint_request("ep-789");
        metrics.record_endpoint_request("ep-abc");
        let text = metrics.prometheus_text();
        assert!(
            text.contains("ccag_endpoint_requests_total"),
            "Prometheus output should contain endpoint request counter"
        );
    }

    #[test]
    fn test_record_error_with_endpoint() {
        let (metrics, _provider) = Metrics::new(None).unwrap();
        metrics.record_error("bedrock_invoke", None);
        metrics.record_error("bedrock_stream", Some("ep-456"));
        let text = metrics.prometheus_text();
        assert!(
            text.contains("ccag_errors_total"),
            "Prometheus output should contain error counter"
        );
    }
}
