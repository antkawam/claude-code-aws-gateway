use std::time::Instant;

use serde_json::{Value, json};
use tokio::sync::RwLock;

/// Cached quota information with TTL.
pub struct QuotaCache {
    client: aws_sdk_servicequotas::Client,
    cache: RwLock<Option<(Instant, Value)>>,
}

impl QuotaCache {
    pub fn new(client: aws_sdk_servicequotas::Client) -> Self {
        Self {
            client,
            cache: RwLock::new(None),
        }
    }

    /// Return cached quota data without fetching. Returns None if cache is empty or expired.
    pub async fn get_cached(&self) -> Option<Value> {
        let cache = self.cache.read().await;
        cache
            .as_ref()
            .filter(|(ts, _)| ts.elapsed().as_secs() < 300)
            .map(|(_, data)| data.clone())
    }

    /// Get Bedrock quotas, cached for 5 minutes.
    pub async fn get_bedrock_quotas(&self) -> Result<Value, String> {
        // Check cache
        {
            let cache = self.cache.read().await;
            if let Some((ts, data)) = cache.as_ref()
                && ts.elapsed().as_secs() < 300
            {
                return Ok(data.clone());
            }
        }

        // Fetch from AWS
        let quotas = self.fetch_quotas().await?;

        // Update cache
        {
            let mut cache = self.cache.write().await;
            *cache = Some((Instant::now(), quotas.clone()));
        }

        Ok(quotas)
    }

    async fn fetch_quotas(&self) -> Result<Value, String> {
        let mut quotas = Vec::new();
        let mut next_token: Option<String> = None;

        loop {
            let mut req = self.client.list_service_quotas().service_code("bedrock");

            if let Some(token) = &next_token {
                req = req.next_token(token);
            }

            let result = req
                .send()
                .await
                .map_err(|e| format!("Failed to list quotas: {:?}", e))?;

            let page_quotas = result.quotas();

            for q in page_quotas {
                // Filter for Claude-related quotas (RPM/TPM)
                let name = q.quota_name().unwrap_or_default();
                let name_lower = name.to_lowercase();
                if name_lower.contains("claude") || name_lower.contains("anthropic") {
                    quotas.push(json!({
                        "quota_name": name,
                        "quota_code": q.quota_code().unwrap_or_default(),
                        "value": q.value(),
                        "unit": q.unit().map(|u| format!("{u:?}")),
                        "adjustable": q.adjustable(),
                    }));
                }
            }

            next_token = result.next_token().map(String::from);
            if next_token.is_none() {
                break;
            }
        }

        Ok(json!({
            "service": "bedrock",
            "quotas": quotas,
        }))
    }
}
