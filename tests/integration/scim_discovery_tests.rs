/// Integration tests for SCIM discovery endpoints.
///
/// These tests call the discovery handlers directly (no network, no DB required)
/// and verify the HTTP contract: status code, content type, and response body shape.
///
/// Discovery endpoints are public — no Authorization header required.
use axum::body::to_bytes;
use axum::response::IntoResponse;

use ccag::scim::discovery::{resource_types, schemas, service_provider_config};

// ============================================================
// Helpers
// ============================================================

/// Read the full response body and parse as JSON.
async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("Failed to read response body");
    serde_json::from_slice(&bytes).expect("Response body is not valid JSON")
}

/// Extract the Content-Type header value as a &str.
fn content_type(resp: &axum::response::Response) -> String {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

// ============================================================
// ServiceProviderConfig
// ============================================================

#[tokio::test]
async fn test_service_provider_config_endpoint() {
    let resp = service_provider_config().await.into_response();

    // 200 OK
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let json = body_json(resp).await;

    // Correct schema URI
    assert_eq!(
        json["schemas"][0],
        "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"
    );

    // Core capability assertions
    assert_eq!(json["patch"]["supported"], true);
    assert_eq!(json["bulk"]["supported"], false);
    assert_eq!(json["filter"]["supported"], true);
    assert_eq!(json["filter"]["maxResults"], 100);
    assert_eq!(json["sort"]["supported"], false);
}

// ============================================================
// ResourceTypes
// ============================================================

#[tokio::test]
async fn test_resource_types_endpoint() {
    let resp = resource_types().await.into_response();

    // 200 OK
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let json = body_json(resp).await;

    // ListResponse schema
    assert_eq!(
        json["schemas"][0],
        "urn:ietf:params:scim:api:messages:2.0:ListResponse"
    );

    // Exactly 2 resource types
    assert_eq!(json["totalResults"], 2);
    let resources = json["Resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2);

    // Find User and Group
    let names: Vec<&str> = resources
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert!(names.contains(&"User"), "ResourceTypes must include User");
    assert!(names.contains(&"Group"), "ResourceTypes must include Group");
}

// ============================================================
// Schemas
// ============================================================

#[tokio::test]
async fn test_schemas_endpoint() {
    let resp = schemas().await.into_response();

    // 200 OK
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let json = body_json(resp).await;

    // totalResults = 2
    assert_eq!(json["totalResults"], 2);

    let resources = json["Resources"].as_array().unwrap();
    let ids: Vec<&str> = resources.iter().filter_map(|r| r["id"].as_str()).collect();
    assert!(
        ids.contains(&"urn:ietf:params:scim:schemas:core:2.0:User"),
        "Schemas must include User schema"
    );
    assert!(
        ids.contains(&"urn:ietf:params:scim:schemas:core:2.0:Group"),
        "Schemas must include Group schema"
    );
}

// ============================================================
// No auth required (public endpoints)
// ============================================================

/// Discovery endpoints must succeed without any Authorization header.
/// We verify this by calling the handlers directly — there is no auth middleware
/// involved in these pure async functions.
#[tokio::test]
async fn test_discovery_no_auth_required() {
    // ServiceProviderConfig — public
    let resp = service_provider_config().await.into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // ResourceTypes — public
    let resp = resource_types().await.into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // Schemas — public
    let resp = schemas().await.into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
}

// ============================================================
// Content-Type: application/scim+json
// ============================================================

#[tokio::test]
async fn test_discovery_content_type() {
    // All three discovery endpoints must return application/scim+json
    let cases: Vec<(&str, axum::response::Response)> = vec![
        (
            "ServiceProviderConfig",
            service_provider_config().await.into_response(),
        ),
        ("ResourceTypes", resource_types().await.into_response()),
        ("Schemas", schemas().await.into_response()),
    ];

    for (name, resp) in cases {
        let ct = content_type(&resp);
        assert!(
            ct.contains("application/scim+json"),
            "{name}: Content-Type must be application/scim+json, got: {ct}"
        );
    }
}
