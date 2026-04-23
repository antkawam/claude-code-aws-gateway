use serde_json::Value;

use super::WebSearchResult;
use crate::db::search_providers::UserSearchProvider;

/// Resolved search provider configuration for a request.
#[derive(Debug, Clone)]
pub enum SearchProvider {
    /// DuckDuckGo HTML scraping (default, no API key needed)
    DuckDuckGo { max_results: usize },
    /// Tavily AI search API
    Tavily { api_key: String, max_results: usize },
    /// Serper.dev Google Search API
    Serper { api_key: String, max_results: usize },
    /// SearXNG self-hosted instance (GET JSON API)
    SearXNG { api_url: String, max_results: usize },
    /// Custom provider with a simple POST JSON contract
    Custom {
        api_url: String,
        api_key: Option<String>,
        max_results: usize,
    },
}

impl SearchProvider {
    pub fn from_config(config: &UserSearchProvider) -> anyhow::Result<Self> {
        let max_results = config.max_results.clamp(1, 20) as usize;
        match config.provider_type.as_str() {
            "duckduckgo" => Ok(Self::DuckDuckGo { max_results }),
            "tavily" => {
                let api_key = config
                    .api_key
                    .as_ref()
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Tavily requires an API key"))?
                    .clone();
                Ok(Self::Tavily {
                    api_key,
                    max_results,
                })
            }
            "serper" => {
                let api_key = config
                    .api_key
                    .as_ref()
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Serper requires an API key"))?
                    .clone();
                Ok(Self::Serper {
                    api_key,
                    max_results,
                })
            }
            "custom" => {
                let api_url = config
                    .api_url
                    .as_ref()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Custom provider requires an API URL"))?
                    .clone();
                Ok(Self::Custom {
                    api_url,
                    api_key: config.api_key.clone(),
                    max_results,
                })
            }
            "searxng" => {
                let api_url = config
                    .api_url
                    .as_ref()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("SearXNG provider requires an API URL"))?
                    .clone();
                Ok(Self::SearXNG { api_url, max_results })
            }
            other => anyhow::bail!("Unknown search provider: {}", other),
        }
    }

    /// Construct a `SearchProvider` from a global config JSON object (as stored in proxy_settings).
    /// Expected fields: provider_type (required), api_key, api_url, max_results.
    pub fn from_global_config(config: &serde_json::Value) -> anyhow::Result<Self> {
        let provider_type = config
            .get("provider_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("provider_type is required"))?;

        let api_key = config
            .get("api_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let api_url = config
            .get("api_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let max_results = config
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;
        let max_results = max_results.clamp(1, 20);

        match provider_type {
            "duckduckgo" => Ok(Self::DuckDuckGo { max_results }),
            "tavily" => {
                let api_key = api_key
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Tavily requires an API key"))?;
                Ok(Self::Tavily {
                    api_key,
                    max_results,
                })
            }
            "serper" => {
                let api_key = api_key
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Serper requires an API key"))?;
                Ok(Self::Serper {
                    api_key,
                    max_results,
                })
            }
            "custom" => {
                let api_url = api_url
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Custom provider requires an API URL"))?;
                Ok(Self::Custom {
                    api_url,
                    api_key,
                    max_results,
                })
            }
            "searxng" => {
                let api_url = api_url
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("SearXNG provider requires an API URL"))?;
                Ok(Self::SearXNG { api_url, max_results })
            }
            other => anyhow::bail!("Unknown search provider: {}", other),
        }
    }

    pub fn provider_name(&self) -> &str {
        match self {
            Self::DuckDuckGo { .. } => "duckduckgo",
            Self::Tavily { .. } => "tavily",
            Self::Serper { .. } => "serper",
            Self::SearXNG { .. } => "searxng",
            Self::Custom { .. } => "custom",
        }
    }

    /// Execute a search query using this provider.
    pub async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> anyhow::Result<Vec<WebSearchResult>> {
        match self {
            Self::DuckDuckGo { max_results } => {
                super::search_duckduckgo(client, query, *max_results).await
            }
            Self::Tavily {
                api_key,
                max_results,
            } => search_tavily(client, api_key, query, *max_results).await,
            Self::Serper {
                api_key,
                max_results,
            } => search_serper(client, api_key, query, *max_results).await,
            Self::SearXNG { api_url, max_results } => {
                search_searxng(client, api_url, query, *max_results).await
            }
            Self::Custom {
                api_url,
                api_key,
                max_results,
            } => search_custom(client, api_url, api_key.as_deref(), query, *max_results).await,
        }
    }

    /// Validate the provider config by running a test search.
    pub async fn validate(&self, client: &reqwest::Client) -> anyhow::Result<Vec<WebSearchResult>> {
        self.search(client, "test search query").await
    }
}

/// Search via Tavily AI Search API.
/// POST https://api.tavily.com/search
/// Auth: Authorization: Bearer <api_key>
async fn search_tavily(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<WebSearchResult>> {
    let body = serde_json::json!({
        "query": query,
        "max_results": max_results,
    });

    let resp = client
        .post("https://api.tavily.com/search")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            anyhow::bail!("Tavily: invalid API key (HTTP {})", status);
        }
        if status.as_u16() == 429 {
            anyhow::bail!("Tavily: rate limited — you may have exceeded your free tier credits");
        }
        anyhow::bail!("Tavily returned HTTP {}: {}", status, body_text);
    }

    let json: Value = resp.json().await?;
    let results = json
        .get("results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    Ok(results
        .into_iter()
        .filter_map(|r| {
            Some(WebSearchResult {
                title: r.get("title")?.as_str()?.to_string(),
                url: r.get("url")?.as_str()?.to_string(),
                snippet: r
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

/// Search via Serper.dev Google Search API.
/// POST https://google.serper.dev/search
/// Auth: X-API-KEY: <api_key>
async fn search_serper(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<WebSearchResult>> {
    let body = serde_json::json!({
        "q": query,
        "num": max_results,
    });

    let resp = client
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            anyhow::bail!("Serper: invalid API key (HTTP {})", status);
        }
        if status.as_u16() == 429 {
            anyhow::bail!("Serper: rate limited — you may have exceeded your free tier quota");
        }
        anyhow::bail!("Serper returned HTTP {}: {}", status, body_text);
    }

    let json: Value = resp.json().await?;
    let results = json
        .get("organic")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    Ok(results
        .into_iter()
        .filter_map(|r| {
            Some(WebSearchResult {
                title: r.get("title")?.as_str()?.to_string(),
                url: r.get("link")?.as_str()?.to_string(),
                snippet: r
                    .get("snippet")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

/// Search via a SearXNG self-hosted instance.
/// GET {api_url}/search?q=...&format=json&categories=general
/// Expects: {"results": [{"title": "...", "url": "...", "content": "..."}]}
async fn search_searxng(
    client: &reqwest::Client,
    api_url: &str,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<WebSearchResult>> {
    let url = format!("{}/search", api_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .query(&[
            ("q", query),
            ("format", "json"),
            ("categories", "general"),
            ("language", "en"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("SearXNG returned HTTP {}", status);
    }

    let json: Value = resp.json().await?;
    let results = json
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| anyhow::anyhow!("SearXNG response missing results array"))?;

    Ok(results
        .iter()
        .take(max_results)
        .filter_map(|r| {
            Some(WebSearchResult {
                title: r.get("title")?.as_str()?.to_string(),
                url: r.get("url")?.as_str()?.to_string(),
                snippet: r
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

/// Search via a custom provider.
/// Contract: POST {api_url} with {"query": "...", "count": N}
/// Expects: [{"title": "...", "link": "...", "snippet": "..."}]
/// Optional auth via Authorization: Bearer <api_key>
async fn search_custom(
    client: &reqwest::Client,
    api_url: &str,
    api_key: Option<&str>,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<WebSearchResult>> {
    let body = serde_json::json!({
        "query": query,
        "count": max_results,
    });

    let mut req = client
        .post(api_url)
        .header("Content-Type", "application/json")
        .json(&body);

    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {}", key));
    }

    let resp = req.send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Custom search returned HTTP {}: {}", status, body_text);
    }

    let json: Value = resp.json().await?;

    // Support both array-at-root and {results: [...]} formats
    let results = if let Some(arr) = json.as_array() {
        arr.clone()
    } else if let Some(arr) = json.get("results").and_then(|r| r.as_array()) {
        arr.clone()
    } else {
        anyhow::bail!("Custom search: expected JSON array or {{\"results\": [...]}}");
    };

    Ok(results
        .into_iter()
        .filter_map(|r| {
            let title = r
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let url = r
                .get("link")
                .or_else(|| r.get("url"))
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let snippet = r
                .get("snippet")
                .or_else(|| r.get("content"))
                .or_else(|| r.get("description"))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if url.is_empty() {
                return None;
            }
            Some(WebSearchResult {
                title,
                url,
                snippet,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_from_config_duckduckgo() {
        let config = UserSearchProvider {
            id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            provider_type: "duckduckgo".to_string(),
            api_key: None,
            api_url: None,
            max_results: 5,
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let provider = SearchProvider::from_config(&config).unwrap();
        assert_eq!(provider.provider_name(), "duckduckgo");
    }

    #[test]
    fn test_provider_from_config_tavily_requires_key() {
        let config = UserSearchProvider {
            id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            provider_type: "tavily".to_string(),
            api_key: None,
            api_url: None,
            max_results: 5,
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert!(SearchProvider::from_config(&config).is_err());
    }

    #[test]
    fn test_provider_from_config_tavily_with_key() {
        let config = UserSearchProvider {
            id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            provider_type: "tavily".to_string(),
            api_key: Some("tvly-test-key".to_string()),
            api_url: None,
            max_results: 10,
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let provider = SearchProvider::from_config(&config).unwrap();
        assert_eq!(provider.provider_name(), "tavily");
    }

    #[test]
    fn test_provider_from_config_custom_requires_url() {
        let config = UserSearchProvider {
            id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            provider_type: "custom".to_string(),
            api_key: None,
            api_url: None,
            max_results: 5,
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert!(SearchProvider::from_config(&config).is_err());
    }

    #[test]
    fn test_provider_from_config_max_results_clamped() {
        let config = UserSearchProvider {
            id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            provider_type: "duckduckgo".to_string(),
            api_key: None,
            api_url: None,
            max_results: 100,
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let provider = SearchProvider::from_config(&config).unwrap();
        match provider {
            SearchProvider::DuckDuckGo { max_results } => assert_eq!(max_results, 20),
            _ => panic!("Expected DuckDuckGo"),
        }
    }

    #[test]
    fn test_provider_unknown_type() {
        let config = UserSearchProvider {
            id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            provider_type: "nonexistent".to_string(),
            api_key: None,
            api_url: None,
            max_results: 5,
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert!(SearchProvider::from_config(&config).is_err());
    }

    // ============================================================
    // Round 3: Global provider construction from config JSON
    // ============================================================
    // These test SearchProvider::from_global_config() which doesn't exist yet.
    // They will fail to compile until the method is implemented.

    #[test]
    fn test_search_provider_from_config_json_tavily() {
        // Given a global config JSON (as stored in proxy_settings), construct a SearchProvider
        let config = serde_json::json!({
            "provider_type": "tavily",
            "api_key": "tvly-test-key-123",
            "max_results": 5
        });
        let provider = SearchProvider::from_global_config(&config)
            .expect("Should construct Tavily provider from global config JSON");
        assert_eq!(provider.provider_name(), "tavily");
        // Verify max_results is respected
        match provider {
            SearchProvider::Tavily {
                api_key,
                max_results,
            } => {
                assert_eq!(api_key, "tvly-test-key-123");
                assert_eq!(max_results, 5);
            }
            _ => panic!("Expected Tavily variant, got {:?}", provider),
        }
    }

    #[test]
    fn test_search_provider_from_config_json_duckduckgo() {
        // DuckDuckGo doesn't need an API key
        let config = serde_json::json!({
            "provider_type": "duckduckgo",
            "max_results": 10
        });
        let provider = SearchProvider::from_global_config(&config)
            .expect("Should construct DuckDuckGo provider from global config JSON");
        assert_eq!(provider.provider_name(), "duckduckgo");
        match provider {
            SearchProvider::DuckDuckGo { max_results } => {
                assert_eq!(max_results, 10);
            }
            _ => panic!("Expected DuckDuckGo variant, got {:?}", provider),
        }
    }

    #[test]
    fn test_search_provider_from_config_json_serper() {
        let config = serde_json::json!({
            "provider_type": "serper",
            "api_key": "serper-key-456",
            "max_results": 7
        });
        let provider = SearchProvider::from_global_config(&config)
            .expect("Should construct Serper provider from global config JSON");
        assert_eq!(provider.provider_name(), "serper");
        match provider {
            SearchProvider::Serper {
                api_key,
                max_results,
            } => {
                assert_eq!(api_key, "serper-key-456");
                assert_eq!(max_results, 7);
            }
            _ => panic!("Expected Serper variant, got {:?}", provider),
        }
    }

    #[test]
    fn test_search_provider_from_config_json_custom() {
        let config = serde_json::json!({
            "provider_type": "custom",
            "api_url": "https://my-search.example.com/api",
            "api_key": "custom-key",
            "max_results": 3
        });
        let provider = SearchProvider::from_global_config(&config)
            .expect("Should construct Custom provider from global config JSON");
        assert_eq!(provider.provider_name(), "custom");
        match provider {
            SearchProvider::Custom {
                api_url,
                api_key,
                max_results,
            } => {
                assert_eq!(api_url, "https://my-search.example.com/api");
                assert_eq!(api_key.as_deref(), Some("custom-key"));
                assert_eq!(max_results, 3);
            }
            _ => panic!("Expected Custom variant, got {:?}", provider),
        }
    }

    #[test]
    fn test_search_provider_from_config_json_missing_type() {
        // Config with no provider_type should fail
        let config = serde_json::json!({
            "api_key": "some-key",
            "max_results": 5
        });
        let result = SearchProvider::from_global_config(&config);
        assert!(
            result.is_err(),
            "from_global_config should fail when provider_type is missing"
        );
    }

    #[test]
    fn test_search_provider_from_config_json_tavily_no_key() {
        // Tavily requires an API key -- should fail
        let config = serde_json::json!({
            "provider_type": "tavily",
            "max_results": 5
        });
        let result = SearchProvider::from_global_config(&config);
        assert!(
            result.is_err(),
            "from_global_config should fail for Tavily without api_key"
        );
    }

    #[test]
    fn test_search_provider_from_config_json_max_results_clamped() {
        // max_results should be clamped to [1, 20]
        let config = serde_json::json!({
            "provider_type": "duckduckgo",
            "max_results": 100
        });
        let provider = SearchProvider::from_global_config(&config)
            .expect("Should construct provider even with out-of-range max_results");
        match provider {
            SearchProvider::DuckDuckGo { max_results } => {
                assert_eq!(max_results, 20, "max_results should be clamped to 20");
            }
            _ => panic!("Expected DuckDuckGo variant"),
        }
    }
}
