use std::sync::Arc;

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, request::Parts},
};
use uuid::Uuid;

use crate::db::scim_tokens;
use crate::proxy::GatewayState;
use crate::scim::ScimError;

/// Authenticated SCIM bearer token context.
#[derive(Debug, Clone)]
pub struct ScimAuth {
    pub token_id: Uuid,
    pub idp_id: Uuid,
}

impl FromRequestParts<Arc<GatewayState>> for ScimAuth {
    type Rejection = ScimError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<GatewayState>,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_token(&parts.headers)
            .ok_or_else(|| ScimError::unauthorized("Missing or malformed Authorization header"))?;

        let pool = state.db().await;
        let token_hash = scim_tokens::hash_token(token);

        let record = scim_tokens::validate_scim_token(&pool, &token_hash)
            .await
            .map_err(|_| ScimError::unauthorized("Token validation failed"))?
            .ok_or_else(|| ScimError::unauthorized("Invalid or revoked SCIM token"))?;

        // Fire-and-forget: update last_used_at without blocking the response.
        let pool_clone = pool.clone();
        let token_id = record.id;
        tokio::spawn(async move {
            let _ = scim_tokens::update_last_used(&pool_clone, token_id).await;
        });

        Ok(ScimAuth {
            token_id: record.id,
            idp_id: record.idp_id,
        })
    }
}

/// Extract the raw bearer token from the `Authorization` header.
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn extract_bearer_token_valid() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer scim-ccag-abc123"),
        );
        assert_eq!(extract_bearer_token(&headers), Some("scim-ccag-abc123"));
    }

    #[test]
    fn extract_bearer_token_missing() {
        let headers = HeaderMap::new();
        assert!(extract_bearer_token(&headers).is_none());
    }

    #[test]
    fn extract_bearer_token_no_bearer_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Basic abc123"));
        assert!(extract_bearer_token(&headers).is_none());
    }

    #[test]
    fn extract_bearer_token_empty_after_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer "));
        assert!(extract_bearer_token(&headers).is_none());
    }
}
