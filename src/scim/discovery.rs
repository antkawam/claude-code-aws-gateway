use axum::{
    http::HeaderValue,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::scim::SCIM_CONTENT_TYPE;

/// Produce a SCIM JSON response with the correct content type.
fn scim_json_response(body: serde_json::Value) -> Response {
    let json_str = body.to_string();
    let mut response = (axum::http::StatusCode::OK, json_str).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(SCIM_CONTENT_TYPE),
    );
    response
}

/// GET /scim/v2/ServiceProviderConfig
///
/// Returns server capabilities per RFC 7643 Section 5.
/// No authentication required.
pub async fn service_provider_config() -> impl IntoResponse {
    scim_json_response(json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "patch": { "supported": true },
        "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
        "filter": { "supported": true, "maxResults": 100 },
        "changePassword": { "supported": false },
        "sort": { "supported": false },
        "etag": { "supported": false },
        "authenticationSchemes": [
            {
                "type": "oauthbearertoken",
                "name": "OAuth Bearer Token",
                "description": "Authentication using a SCIM bearer token",
                "specUri": "https://datatracker.ietf.org/doc/html/rfc6750"
            }
        ]
    }))
}

/// GET /scim/v2/ResourceTypes
///
/// Lists the resource types supported by this SCIM service provider.
/// No authentication required.
pub async fn resource_types() -> impl IntoResponse {
    scim_json_response(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": 2,
        "Resources": [
            {
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
                "id": "User",
                "name": "User",
                "endpoint": "/scim/v2/Users",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:User"
            },
            {
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
                "id": "Group",
                "name": "Group",
                "endpoint": "/scim/v2/Groups",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:Group"
            }
        ]
    }))
}

/// GET /scim/v2/Schemas
///
/// Returns the User and Group schema definitions per RFC 7643 Sections 7.1-7.2.
/// No authentication required.
pub async fn schemas() -> impl IntoResponse {
    scim_json_response(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": 2,
        "Resources": [
            {
                "id": "urn:ietf:params:scim:schemas:core:2.0:User",
                "name": "User",
                "description": "User Account",
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Schema"],
                "attributes": [
                    {
                        "name": "userName",
                        "type": "string",
                        "multiValued": false,
                        "description": "Unique identifier for the user, typically an email address.",
                        "required": true,
                        "caseExact": false,
                        "mutability": "readWrite",
                        "returned": "default",
                        "uniqueness": "server"
                    },
                    {
                        "name": "name",
                        "type": "complex",
                        "multiValued": false,
                        "description": "The components of the user's real name.",
                        "required": false,
                        "mutability": "readWrite",
                        "returned": "default",
                        "subAttributes": [
                            {
                                "name": "givenName",
                                "type": "string",
                                "multiValued": false,
                                "description": "The given name of the user.",
                                "required": false,
                                "caseExact": false,
                                "mutability": "readWrite",
                                "returned": "default"
                            },
                            {
                                "name": "familyName",
                                "type": "string",
                                "multiValued": false,
                                "description": "The family name of the user.",
                                "required": false,
                                "caseExact": false,
                                "mutability": "readWrite",
                                "returned": "default"
                            }
                        ]
                    },
                    {
                        "name": "displayName",
                        "type": "string",
                        "multiValued": false,
                        "description": "The name of the user, suitable for display.",
                        "required": false,
                        "caseExact": false,
                        "mutability": "readWrite",
                        "returned": "default"
                    },
                    {
                        "name": "emails",
                        "type": "complex",
                        "multiValued": true,
                        "description": "Email addresses for the user.",
                        "required": false,
                        "mutability": "readWrite",
                        "returned": "default",
                        "subAttributes": [
                            {
                                "name": "value",
                                "type": "string",
                                "multiValued": false,
                                "description": "Email address value.",
                                "required": false,
                                "caseExact": false,
                                "mutability": "readWrite",
                                "returned": "default"
                            },
                            {
                                "name": "primary",
                                "type": "boolean",
                                "multiValued": false,
                                "description": "Indicates the primary email address.",
                                "required": false,
                                "mutability": "readWrite",
                                "returned": "default"
                            },
                            {
                                "name": "type",
                                "type": "string",
                                "multiValued": false,
                                "description": "A label indicating the email type (e.g., work, home).",
                                "required": false,
                                "caseExact": false,
                                "mutability": "readWrite",
                                "returned": "default"
                            }
                        ]
                    },
                    {
                        "name": "active",
                        "type": "boolean",
                        "multiValued": false,
                        "description": "A Boolean value indicating the user's administrative status.",
                        "required": false,
                        "mutability": "readWrite",
                        "returned": "default"
                    },
                    {
                        "name": "externalId",
                        "type": "string",
                        "multiValued": false,
                        "description": "A String that is an identifier for the resource as defined by the provisioning client.",
                        "required": false,
                        "caseExact": true,
                        "mutability": "readWrite",
                        "returned": "default"
                    },
                    {
                        "name": "groups",
                        "type": "complex",
                        "multiValued": true,
                        "description": "A list of groups to which the user belongs.",
                        "required": false,
                        "mutability": "readOnly",
                        "returned": "default",
                        "subAttributes": [
                            {
                                "name": "value",
                                "type": "string",
                                "multiValued": false,
                                "description": "The identifier of the group.",
                                "required": false,
                                "caseExact": false,
                                "mutability": "readOnly",
                                "returned": "default"
                            },
                            {
                                "name": "display",
                                "type": "string",
                                "multiValued": false,
                                "description": "The displayName of the group.",
                                "required": false,
                                "caseExact": false,
                                "mutability": "readOnly",
                                "returned": "default"
                            },
                            {
                                "name": "$ref",
                                "type": "reference",
                                "multiValued": false,
                                "description": "The URI of the group.",
                                "required": false,
                                "mutability": "readOnly",
                                "returned": "default"
                            }
                        ]
                    }
                ],
                "meta": {
                    "resourceType": "Schema",
                    "location": "/scim/v2/Schemas/urn:ietf:params:scim:schemas:core:2.0:User"
                }
            },
            {
                "id": "urn:ietf:params:scim:schemas:core:2.0:Group",
                "name": "Group",
                "description": "Group",
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Schema"],
                "attributes": [
                    {
                        "name": "displayName",
                        "type": "string",
                        "multiValued": false,
                        "description": "A human-readable name for the group.",
                        "required": true,
                        "caseExact": false,
                        "mutability": "readWrite",
                        "returned": "default"
                    },
                    {
                        "name": "members",
                        "type": "complex",
                        "multiValued": true,
                        "description": "A list of members of the group.",
                        "required": false,
                        "mutability": "readWrite",
                        "returned": "default",
                        "subAttributes": [
                            {
                                "name": "value",
                                "type": "string",
                                "multiValued": false,
                                "description": "Identifier of the member.",
                                "required": false,
                                "caseExact": false,
                                "mutability": "immutable",
                                "returned": "default"
                            },
                            {
                                "name": "display",
                                "type": "string",
                                "multiValued": false,
                                "description": "The displayName of the member.",
                                "required": false,
                                "caseExact": false,
                                "mutability": "readOnly",
                                "returned": "default"
                            },
                            {
                                "name": "$ref",
                                "type": "reference",
                                "multiValued": false,
                                "description": "The URI of the member.",
                                "required": false,
                                "mutability": "readOnly",
                                "returned": "default"
                            }
                        ]
                    },
                    {
                        "name": "externalId",
                        "type": "string",
                        "multiValued": false,
                        "description": "A String that is an identifier for the resource as defined by the provisioning client.",
                        "required": false,
                        "caseExact": true,
                        "mutability": "readWrite",
                        "returned": "default"
                    }
                ],
                "meta": {
                    "resourceType": "Schema",
                    "location": "/scim/v2/Schemas/urn:ietf:params:scim:schemas:core:2.0:Group"
                }
            }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn service_provider_config_returns_scim_content_type() {
        let resp = service_provider_config().await.into_response();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("application/scim+json"),
            "Content-Type was: {ct}"
        );
    }

    #[tokio::test]
    async fn service_provider_config_body() {
        let resp = service_provider_config().await.into_response();
        let json = body_json(resp).await;
        assert_eq!(
            json["schemas"][0],
            "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"
        );
        assert_eq!(json["patch"]["supported"], true);
        assert_eq!(json["bulk"]["supported"], false);
        assert_eq!(json["filter"]["supported"], true);
    }

    #[tokio::test]
    async fn resource_types_has_two_resources() {
        let resp = resource_types().await.into_response();
        let json = body_json(resp).await;
        assert_eq!(json["totalResults"], 2);
        let resources = json["Resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2);
        let names: Vec<&str> = resources
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"User"));
        assert!(names.contains(&"Group"));
    }

    #[tokio::test]
    async fn schemas_has_user_and_group() {
        let resp = schemas().await.into_response();
        let json = body_json(resp).await;
        let resources = json["Resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2);
        let ids: Vec<&str> = resources
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"urn:ietf:params:scim:schemas:core:2.0:User"));
        assert!(ids.contains(&"urn:ietf:params:scim:schemas:core:2.0:Group"));
    }

    // --- Additional tests (from test-agent) ---

    /// ServiceProviderConfig shape: key capability fields match the spec.
    #[tokio::test]
    async fn service_provider_config_shape() {
        let resp = service_provider_config().await.into_response();
        let json = body_json(resp).await;

        // patch.supported = true
        assert_eq!(json["patch"]["supported"], true);

        // bulk.supported = false
        assert_eq!(json["bulk"]["supported"], false);

        // filter.supported = true, maxResults = 100
        assert_eq!(json["filter"]["supported"], true);
        assert_eq!(json["filter"]["maxResults"], 100);

        // sort.supported = false
        assert_eq!(json["sort"]["supported"], false);

        // authenticationSchemes has oauthbearertoken
        let schemes = json["authenticationSchemes"].as_array().unwrap();
        assert!(!schemes.is_empty());
        let scheme_types: Vec<&str> = schemes.iter().filter_map(|s| s["type"].as_str()).collect();
        assert!(
            scheme_types.contains(&"oauthbearertoken"),
            "Expected oauthbearertoken scheme, got: {scheme_types:?}"
        );
    }

    /// ResourceTypes shape: 2 resources with correct endpoints and schemas.
    #[tokio::test]
    async fn resource_types_shape() {
        let resp = resource_types().await.into_response();
        let json = body_json(resp).await;

        // ListResponse schema
        assert_eq!(
            json["schemas"][0],
            "urn:ietf:params:scim:api:messages:2.0:ListResponse"
        );

        let resources = json["Resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2);

        // Find User resource type
        let user_rt = resources.iter().find(|r| r["id"] == "User").unwrap();
        assert_eq!(user_rt["name"].as_str().unwrap(), "User");
        assert_eq!(user_rt["endpoint"].as_str().unwrap(), "/scim/v2/Users");
        assert_eq!(
            user_rt["schema"].as_str().unwrap(),
            "urn:ietf:params:scim:schemas:core:2.0:User"
        );

        // Find Group resource type
        let group_rt = resources.iter().find(|r| r["id"] == "Group").unwrap();
        assert_eq!(group_rt["name"].as_str().unwrap(), "Group");
        assert_eq!(group_rt["endpoint"].as_str().unwrap(), "/scim/v2/Groups");
        assert_eq!(
            group_rt["schema"].as_str().unwrap(),
            "urn:ietf:params:scim:schemas:core:2.0:Group"
        );
    }

    /// Schemas shape: contains User and Group schema definitions with correct URIs.
    #[tokio::test]
    async fn schemas_shape() {
        let resp = schemas().await.into_response();
        let json = body_json(resp).await;

        // ListResponse schema
        assert_eq!(
            json["schemas"][0],
            "urn:ietf:params:scim:api:messages:2.0:ListResponse"
        );
        assert_eq!(json["totalResults"], 2);

        let resources = json["Resources"].as_array().unwrap();

        // User schema has required attributes
        let user_schema = resources
            .iter()
            .find(|r| r["id"] == "urn:ietf:params:scim:schemas:core:2.0:User")
            .unwrap();
        let user_attrs = user_schema["attributes"].as_array().unwrap();
        let attr_names: Vec<&str> = user_attrs
            .iter()
            .filter_map(|a| a["name"].as_str())
            .collect();
        assert!(
            attr_names.contains(&"userName"),
            "User schema must have userName attribute"
        );
        assert!(
            attr_names.contains(&"active"),
            "User schema must have active attribute"
        );

        // Group schema has required attributes
        let group_schema = resources
            .iter()
            .find(|r| r["id"] == "urn:ietf:params:scim:schemas:core:2.0:Group")
            .unwrap();
        let group_attrs = group_schema["attributes"].as_array().unwrap();
        let group_attr_names: Vec<&str> = group_attrs
            .iter()
            .filter_map(|a| a["name"].as_str())
            .collect();
        assert!(
            group_attr_names.contains(&"displayName"),
            "Group schema must have displayName"
        );
        assert!(
            group_attr_names.contains(&"members"),
            "Group schema must have members"
        );
    }

    /// All three discovery endpoints set the SCIM content type.
    #[tokio::test]
    async fn discovery_content_type_all_endpoints() {
        for resp in [
            service_provider_config().await.into_response(),
            resource_types().await.into_response(),
            schemas().await.into_response(),
        ] {
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(
                ct.contains("application/scim+json"),
                "Content-Type must be application/scim+json, got: {ct}"
            );
        }
    }
}
// #[cfg(test)]
