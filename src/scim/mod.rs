pub mod auth;
pub mod discovery;
pub mod filter;
pub mod groups;
pub mod types;
pub mod users;

use axum::{
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

/// SCIM content type per RFC 7644.
pub const SCIM_CONTENT_TYPE: &str = "application/scim+json; charset=utf-8";

/// RFC 7644 Section 3.12 error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimError {
    pub schemas: Vec<String>,
    #[serde(rename = "scimType", skip_serializing_if = "Option::is_none")]
    pub scim_type: Option<String>,
    pub detail: String,
    /// HTTP status code as a string (RFC 7644 requires string).
    pub status: String,
    /// HTTP status code for response generation (not serialized).
    #[serde(skip)]
    pub http_status: StatusCode,
}

impl ScimError {
    const ERROR_SCHEMA: &'static str = "urn:ietf:params:scim:api:messages:2.0:Error";

    fn new(status: StatusCode, scim_type: Option<&str>, detail: impl Into<String>) -> Self {
        Self {
            schemas: vec![Self::ERROR_SCHEMA.to_string()],
            scim_type: scim_type.map(str::to_string),
            detail: detail.into(),
            status: status.as_u16().to_string(),
            http_status: status,
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidValue"), detail)
    }

    pub fn invalid_filter(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidFilter"), detail)
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, None, detail)
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, Some("uniqueness"), detail)
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, None, detail)
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, None, detail)
    }
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let status = self.http_status;
        let body = match serde_json::to_string(&self) {
            Ok(json) => json,
            Err(_) => r#"{"schemas":["urn:ietf:params:scim:api:messages:2.0:Error"],"detail":"Internal serialization error","status":"500"}"#.to_string(),
        };
        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(SCIM_CONTENT_TYPE),
        );
        response
    }
}

/// RFC 7644 ListResponse container.
#[derive(Debug, Clone, Serialize)]
pub struct ScimListResponse<T: Serialize> {
    pub schemas: Vec<String>,
    #[serde(rename = "totalResults")]
    pub total_results: i64,
    #[serde(rename = "startIndex")]
    pub start_index: i64,
    #[serde(rename = "itemsPerPage")]
    pub items_per_page: i64,
    #[serde(rename = "Resources")]
    pub resources: Vec<T>,
}

impl<T: Serialize> ScimListResponse<T> {
    pub fn new(resources: Vec<T>, total_results: i64, start_index: i64) -> Self {
        let items_per_page = resources.len() as i64;
        Self {
            schemas: vec!["urn:ietf:params:scim:api:messages:2.0:ListResponse".to_string()],
            total_results,
            start_index,
            items_per_page,
            resources,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scim_error_serializes_correctly() {
        let err = ScimError::conflict("User alice@example.com already exists");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(
            json["schemas"][0],
            "urn:ietf:params:scim:api:messages:2.0:Error"
        );
        assert_eq!(json["scimType"], "uniqueness");
        assert_eq!(json["status"], "409");
    }

    #[test]
    fn scim_error_unauthorized_has_no_scim_type() {
        let err = ScimError::unauthorized("Missing token");
        let json = serde_json::to_value(&err).unwrap();
        assert!(!json.as_object().unwrap().contains_key("scimType"));
        assert_eq!(json["status"], "401");
    }

    #[test]
    fn scim_list_response_items_per_page() {
        let resp = ScimListResponse::new(vec![1u32, 2, 3], 10, 1);
        assert_eq!(resp.items_per_page, 3);
        assert_eq!(resp.total_results, 10);
    }

    // --- Additional RFC-conformance tests ---

    /// Full RFC 7644 error format: schemas array, scimType, detail, status as string.
    #[test]
    fn scim_error_serializes_to_rfc_format() {
        let err = ScimError::conflict("User with userName alice@example.com already exists");
        let json = serde_json::to_value(&err).unwrap();

        // schemas must be a single-element array with the Error schema URI
        let schemas = json["schemas"].as_array().unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(
            schemas[0].as_str().unwrap(),
            "urn:ietf:params:scim:api:messages:2.0:Error"
        );

        // scimType must be "uniqueness" for conflict errors
        assert_eq!(json["scimType"].as_str().unwrap(), "uniqueness");

        // detail must be the provided message
        assert_eq!(
            json["detail"].as_str().unwrap(),
            "User with userName alice@example.com already exists"
        );

        // status must be a string (RFC 7644 requires string, not integer)
        assert_eq!(json["status"].as_str().unwrap(), "409");

        // http_status must not appear in serialized form (it is #[serde(skip)])
        assert!(!json.as_object().unwrap().contains_key("http_status"));
    }

    /// When scim_type is None, the `scimType` key must be absent from the JSON.
    #[test]
    fn scim_error_without_scim_type_key_absent() {
        let err = ScimError::not_found("Resource not found");
        let json = serde_json::to_value(&err).unwrap();
        let obj = json.as_object().unwrap();
        assert!(
            !obj.contains_key("scimType"),
            "scimType should be absent when None"
        );
        assert_eq!(json["status"].as_str().unwrap(), "404");
    }

    /// All constructor methods produce the correct HTTP status code.
    #[test]
    fn scim_error_status_codes() {
        assert_eq!(
            ScimError::bad_request("bad").http_status,
            axum::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(ScimError::bad_request("bad").status, "400");

        assert_eq!(
            ScimError::not_found("nf").http_status,
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(ScimError::not_found("nf").status, "404");

        assert_eq!(
            ScimError::conflict("dup").http_status,
            axum::http::StatusCode::CONFLICT
        );
        assert_eq!(ScimError::conflict("dup").status, "409");

        assert_eq!(
            ScimError::unauthorized("unauth").http_status,
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(ScimError::unauthorized("unauth").status, "401");

        assert_eq!(
            ScimError::internal("oops").http_status,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(ScimError::internal("oops").status, "500");
    }

    /// `invalid_filter` uses `invalidFilter` scimType (separate from bad_request).
    #[test]
    fn scim_error_invalid_filter_scim_type() {
        let err = ScimError::invalid_filter("unparseable filter");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["scimType"].as_str().unwrap(), "invalidFilter");
        assert_eq!(json["status"].as_str().unwrap(), "400");
    }

    /// ListResponse serializes with `Resources` (capital R) per RFC 7644.
    #[test]
    fn list_response_serializes_correctly() {
        let resp = ScimListResponse::new(vec!["item1", "item2"], 5, 1);
        let json = serde_json::to_value(&resp).unwrap();
        let obj = json.as_object().unwrap();

        // schemas field present
        let schemas = json["schemas"].as_array().unwrap();
        assert_eq!(
            schemas[0].as_str().unwrap(),
            "urn:ietf:params:scim:api:messages:2.0:ListResponse"
        );

        // Capital-R Resources key
        assert!(
            obj.contains_key("Resources"),
            "Must have capital-R Resources"
        );
        assert!(
            !obj.contains_key("resources"),
            "Must not have lowercase resources"
        );

        // Numeric fields
        assert_eq!(json["totalResults"].as_i64().unwrap(), 5);
        assert_eq!(json["startIndex"].as_i64().unwrap(), 1);
        assert_eq!(json["itemsPerPage"].as_i64().unwrap(), 2);

        // Resources array length matches items
        let resources = json["Resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2);
    }

    /// startIndex is preserved in ListResponse.
    #[test]
    fn list_response_start_index_preserved() {
        let resp = ScimListResponse::new(vec![42u32], 100, 51);
        assert_eq!(resp.start_index, 51);
        assert_eq!(resp.total_results, 100);
        assert_eq!(resp.items_per_page, 1);
    }
}
