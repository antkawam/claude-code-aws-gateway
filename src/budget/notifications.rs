use serde::Serialize;

use crate::db::budget::BudgetEvent;

/// Notification payload sent via webhook/SNS/EventBridge.
#[derive(Debug, Serialize, Clone)]
pub struct NotificationPayload {
    pub source: String,
    pub version: String,
    pub category: String,
    pub event_type: String,
    pub severity: String,
    pub user_identity: Option<String>,
    pub team_id: Option<uuid::Uuid>,
    pub team_name: Option<String>,
    pub detail: NotificationDetail,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct NotificationDetail {
    pub threshold_percent: i32,
    pub spend_usd: f64,
    pub limit_usd: f64,
    pub percent: f64,
    pub period: String,
    pub period_start: String,
}

/// Map event_type string to notification category.
pub fn event_category(event_type: &str) -> &'static str {
    match event_type {
        "notify" | "shape" | "block" | "team_notify" | "team_shape" | "team_block" => "budget",
        "rate_limit" => "rate_limit",
        _ => "budget",
    }
}

/// Map event_type string to severity.
fn event_severity(event_type: &str) -> &'static str {
    match event_type {
        "notify" | "team_notify" => "warning",
        "shape" | "team_shape" => "high",
        "block" | "team_block" => "critical",
        "rate_limit" => "warning",
        _ => "info",
    }
}

/// Map event_type to a readable event type name for the payload.
fn canonical_event_type(event_type: &str) -> String {
    match event_type {
        "notify" => "budget_warning".to_string(),
        "shape" => "budget_shaped".to_string(),
        "block" => "budget_blocked".to_string(),
        "team_notify" => "team_budget_warning".to_string(),
        "team_shape" => "team_budget_shaped".to_string(),
        "team_block" => "team_budget_blocked".to_string(),
        "rate_limit" => "rate_limit_hit".to_string(),
        other => other.to_string(),
    }
}

impl From<&BudgetEvent> for NotificationPayload {
    fn from(e: &BudgetEvent) -> Self {
        Self {
            source: "ccag".to_string(),
            version: "1".to_string(),
            category: event_category(&e.event_type).to_string(),
            event_type: canonical_event_type(&e.event_type),
            severity: event_severity(&e.event_type).to_string(),
            user_identity: e.user_identity.clone(),
            team_id: e.team_id,
            team_name: None,
            detail: NotificationDetail {
                threshold_percent: e.threshold_percent,
                spend_usd: e.spend_usd,
                limit_usd: e.limit_usd,
                percent: e.percent,
                period: e.period.clone(),
                period_start: e.period_start.to_rfc3339(),
            },
            timestamp: e.created_at.to_rfc3339(),
        }
    }
}

/// Send a notification via webhook (HTTP POST with JSON body).
pub async fn send_webhook(
    client: &reqwest::Client,
    url: &str,
    payload: &NotificationPayload,
) -> anyhow::Result<()> {
    let resp = client.post(url).json(payload).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Webhook returned {}: {}", status, body);
    }
    Ok(())
}

/// Send a notification via AWS SNS.
pub async fn send_sns(
    sns_client: &aws_sdk_sns::Client,
    topic_arn: &str,
    payload: &NotificationPayload,
) -> anyhow::Result<()> {
    let message = serde_json::to_string_pretty(payload)?;
    let subject = format!(
        "{} for {}",
        payload.event_type,
        payload.user_identity.as_deref().unwrap_or("team")
    );

    sns_client
        .publish()
        .topic_arn(topic_arn)
        .subject(&subject[..subject.len().min(100)])
        .message(&message)
        .send()
        .await?;

    Ok(())
}

/// Send a notification via AWS EventBridge.
pub async fn send_eventbridge(
    eb_client: &aws_sdk_eventbridge::Client,
    bus_arn: &str,
    payload: &NotificationPayload,
) -> anyhow::Result<()> {
    let detail = serde_json::to_string(payload)?;

    let entry = aws_sdk_eventbridge::types::PutEventsRequestEntry::builder()
        .source("ccag.notifications")
        .detail_type(payload.event_type.clone())
        .detail(detail)
        .event_bus_name(bus_arn)
        .build();

    let result = eb_client.put_events().entries(entry).send().await?;

    if result.failed_entry_count() > 0 {
        let error_msg = result
            .entries()
            .first()
            .and_then(|e| e.error_message().map(|s| s.to_string()))
            .unwrap_or_else(|| "Unknown EventBridge error".to_string());
        anyhow::bail!("EventBridge PutEvents failed: {}", error_msg);
    }

    Ok(())
}

/// Build a dedicated HTTP client with a 10s timeout for notification delivery.
pub fn delivery_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("Failed to build delivery HTTP client")
}

/// Deliver a payload to the given destination. Returns (success, error_message, duration_ms).
pub async fn deliver(
    delivery_http: &reqwest::Client,
    sns_client: &Option<aws_sdk_sns::Client>,
    eb_client: &Option<aws_sdk_eventbridge::Client>,
    dest_type: &str,
    dest_value: &str,
    payload: &NotificationPayload,
) -> (bool, Option<String>, i32) {
    let start = std::time::Instant::now();
    let result = match dest_type {
        "webhook" => send_webhook(delivery_http, dest_value, payload).await,
        "sns" => match sns_client {
            Some(client) => send_sns(client, dest_value, payload).await,
            None => Err(anyhow::anyhow!("SNS client not available")),
        },
        "eventbridge" => match eb_client {
            Some(client) => send_eventbridge(client, dest_value, payload).await,
            None => Err(anyhow::anyhow!("EventBridge client not available")),
        },
        other => Err(anyhow::anyhow!("Unknown destination type: {}", other)),
    };
    let duration_ms = start.elapsed().as_millis() as i32;

    match result {
        Ok(()) => (true, None, duration_ms),
        Err(e) => (false, Some(e.to_string()), duration_ms),
    }
}

/// Background loop: deliver pending budget events via configured channels.
/// Checks DB for active notification config; falls back to env var.
pub async fn delivery_loop(
    db_pool: std::sync::Arc<tokio::sync::RwLock<sqlx::PgPool>>,
    delivery_http: reqwest::Client,
    notification_url: Option<String>,
    sns_client: Option<aws_sdk_sns::Client>,
    eb_client: Option<aws_sdk_eventbridge::Client>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    let mut cycle_count: u64 = 0;

    loop {
        interval.tick().await;
        cycle_count += 1;

        let pool = db_pool.read().await.clone();

        let events = match crate::db::budget::get_undelivered_events(&pool, 50).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(%e, "Failed to fetch undelivered budget events");
                continue;
            }
        };

        if events.is_empty() {
            // Prune delivery log periodically even when idle
            if cycle_count.is_multiple_of(100) {
                let _ = crate::db::notification_config::prune_delivery_log(&pool, 500).await;
            }
            continue;
        }

        // Resolve destination: DB active config takes precedence over env var
        let db_config = crate::db::notification_config::get_active(&pool)
            .await
            .ok()
            .flatten();

        let (dest_type, dest_value, categories) = if let Some(ref cfg) = db_config {
            (
                cfg.destination_type.as_str(),
                cfg.destination_value.as_str(),
                cfg.event_categories.clone(),
            )
        } else if let Some(ref url) = notification_url {
            // Env var fallback
            let dtype = if url.starts_with("arn:aws:sns:") {
                "sns"
            } else {
                "webhook"
            };
            (dtype, url.as_str(), serde_json::json!(["budget"]))
        } else {
            // No destination configured — mark all as delivered (no-op)
            for event in &events {
                let _ = crate::db::budget::mark_delivered(&pool, event.id).await;
            }
            continue;
        };

        // Parse enabled categories
        let enabled_categories: Vec<String> = categories
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["budget".to_string()]);

        tracing::info!(
            count = events.len(),
            dest_type,
            "Delivering budget notifications"
        );

        for event in &events {
            let payload = NotificationPayload::from(event);

            // Filter by category
            if !enabled_categories.contains(&payload.category) {
                // Category not enabled — mark delivered without sending
                let _ = crate::db::budget::mark_delivered(&pool, event.id).await;
                continue;
            }

            let (success, error, duration_ms) = deliver(
                &delivery_http,
                &sns_client,
                &eb_client,
                dest_type,
                dest_value,
                &payload,
            )
            .await;

            // Log delivery attempt
            let payload_json = serde_json::to_value(&payload).unwrap_or_default();
            let _ = crate::db::notification_config::log_delivery(
                &pool,
                Some(event.id),
                dest_type,
                dest_value,
                &payload.event_type,
                &payload_json,
                if success { "success" } else { "failure" },
                error.as_deref(),
                duration_ms,
            )
            .await;

            if success {
                if let Err(e) = crate::db::budget::mark_delivered(&pool, event.id).await {
                    tracing::warn!(event_id = event.id, %e, "Failed to mark event delivered");
                }
            } else {
                tracing::warn!(
                    event_id = event.id,
                    error = error.as_deref().unwrap_or("unknown"),
                    "Notification delivery failed"
                );
            }
        }

        // Prune delivery log periodically
        if cycle_count.is_multiple_of(100) {
            let _ = crate::db::notification_config::prune_delivery_log(&pool, 500).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_category_mapping() {
        assert_eq!(event_category("notify"), "budget");
        assert_eq!(event_category("shape"), "budget");
        assert_eq!(event_category("block"), "budget");
        assert_eq!(event_category("team_notify"), "budget");
        assert_eq!(event_category("team_shape"), "budget");
        assert_eq!(event_category("team_block"), "budget");
        assert_eq!(event_category("rate_limit"), "rate_limit");
        assert_eq!(event_category("unknown_type"), "budget"); // default
    }

    #[test]
    fn canonical_event_type_mapping() {
        assert_eq!(canonical_event_type("notify"), "budget_warning");
        assert_eq!(canonical_event_type("shape"), "budget_shaped");
        assert_eq!(canonical_event_type("block"), "budget_blocked");
        assert_eq!(canonical_event_type("team_notify"), "team_budget_warning");
        assert_eq!(canonical_event_type("team_shape"), "team_budget_shaped");
        assert_eq!(canonical_event_type("team_block"), "team_budget_blocked");
        assert_eq!(canonical_event_type("rate_limit"), "rate_limit_hit");
        assert_eq!(canonical_event_type("custom"), "custom"); // passthrough
    }

    #[test]
    fn event_severity_mapping() {
        assert_eq!(event_severity("notify"), "warning");
        assert_eq!(event_severity("shape"), "high");
        assert_eq!(event_severity("block"), "critical");
        assert_eq!(event_severity("rate_limit"), "warning");
    }

    #[test]
    fn payload_serialization() {
        let payload = NotificationPayload {
            source: "ccag".to_string(),
            version: "1".to_string(),
            category: "budget".to_string(),
            event_type: "budget_warning".to_string(),
            severity: "warning".to_string(),
            user_identity: Some("test@example.com".to_string()),
            team_id: None,
            team_name: Some("test-team".to_string()),
            detail: NotificationDetail {
                threshold_percent: 80,
                spend_usd: 40.0,
                limit_usd: 50.0,
                percent: 80.0,
                period: "weekly".to_string(),
                period_start: "2026-03-17T00:00:00Z".to_string(),
            },
            timestamp: "2026-03-19T14:30:00Z".to_string(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["source"], "ccag");
        assert_eq!(json["version"], "1");
        assert_eq!(json["category"], "budget");
        assert_eq!(json["event_type"], "budget_warning");
        assert_eq!(json["severity"], "warning");
        assert_eq!(json["user_identity"], "test@example.com");
        assert_eq!(json["team_name"], "test-team");
        assert_eq!(json["detail"]["threshold_percent"], 80);
        assert_eq!(json["detail"]["spend_usd"], 40.0);
        assert_eq!(json["detail"]["limit_usd"], 50.0);
        assert_eq!(json["timestamp"], "2026-03-19T14:30:00Z");
    }

    #[test]
    fn delivery_http_client_has_timeout() {
        let client = delivery_http_client();
        // Client builds successfully — timeout is set internally
        // (reqwest doesn't expose timeout getter, but the builder succeeds)
        drop(client);
    }

    #[tokio::test]
    async fn webhook_success() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = delivery_http_client();
        let payload = test_payload();

        let result = send_webhook(&client, &format!("{}/hook", mock_server.uri()), &payload).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn webhook_failure_returns_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
            .mount(&mock_server)
            .await;

        let client = delivery_http_client();
        let payload = test_payload();

        let result = send_webhook(&client, &format!("{}/hook", mock_server.uri()), &payload).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("500"));
    }

    #[tokio::test]
    async fn webhook_timeout() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK").set_delay(
                std::time::Duration::from_secs(15), // Exceeds 10s client timeout
            ))
            .mount(&mock_server)
            .await;

        let client = delivery_http_client();
        let payload = test_payload();

        let result = send_webhook(&client, &format!("{}/hook", mock_server.uri()), &payload).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn deliver_routes_to_webhook() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = delivery_http_client();
        let payload = test_payload();

        let (success, error, duration) = deliver(
            &client,
            &None,
            &None,
            "webhook",
            &format!("{}/hook", mock_server.uri()),
            &payload,
        )
        .await;

        assert!(success);
        assert!(error.is_none());
        assert!(duration > 0);
    }

    #[tokio::test]
    async fn deliver_sns_without_client_returns_error() {
        let client = delivery_http_client();
        let payload = test_payload();

        let (success, error, _) = deliver(
            &client,
            &None, // no SNS client
            &None,
            "sns",
            "arn:aws:sns:us-east-1:123456789012:topic",
            &payload,
        )
        .await;

        assert!(!success);
        assert!(error.unwrap().contains("SNS client not available"));
    }

    #[tokio::test]
    async fn deliver_eventbridge_without_client_returns_error() {
        let client = delivery_http_client();
        let payload = test_payload();

        let (success, error, _) = deliver(
            &client,
            &None,
            &None, // no EB client
            "eventbridge",
            "arn:aws:events:us-east-1:123456789012:event-bus/test",
            &payload,
        )
        .await;

        assert!(!success);
        assert!(error.unwrap().contains("EventBridge client not available"));
    }

    #[tokio::test]
    async fn deliver_unknown_type_returns_error() {
        let client = delivery_http_client();
        let payload = test_payload();

        let (success, error, _) =
            deliver(&client, &None, &None, "email", "foo@bar.com", &payload).await;

        assert!(!success);
        assert!(error.unwrap().contains("Unknown destination type"));
    }

    fn test_payload() -> NotificationPayload {
        NotificationPayload {
            source: "ccag".to_string(),
            version: "1".to_string(),
            category: "budget".to_string(),
            event_type: "budget_warning".to_string(),
            severity: "warning".to_string(),
            user_identity: Some("test@example.com".to_string()),
            team_id: None,
            team_name: None,
            detail: NotificationDetail {
                threshold_percent: 80,
                spend_usd: 40.0,
                limit_usd: 50.0,
                percent: 80.0,
                period: "weekly".to_string(),
                period_start: "2026-03-17T00:00:00Z".to_string(),
            },
            timestamp: "2026-03-19T14:30:00Z".to_string(),
        }
    }
}
