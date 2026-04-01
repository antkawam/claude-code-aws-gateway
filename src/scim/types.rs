use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::schema::User;

const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";

/// SCIM User resource (RFC 7643 Section 4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimUser {
    pub schemas: Vec<String>,
    pub id: String,
    #[serde(rename = "externalId", skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(rename = "userName")]
    pub user_name: String,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<ScimName>,
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emails: Option<Vec<ScimEmail>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<ScimGroupRef>>,
    pub meta: ScimMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimName {
    #[serde(rename = "givenName", skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    #[serde(rename = "familyName", skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimEmail {
    pub value: String,
    pub primary: bool,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub email_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimGroupRef {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub ref_uri: Option<String>,
}

/// SCIM Group resource (RFC 7643 Section 4.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimGroup {
    pub schemas: Vec<String>,
    pub id: String,
    #[serde(rename = "externalId", skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<ScimMemberRef>>,
    pub meta: ScimMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimMemberRef {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(rename = "$ref", skip_serializing_if = "Option::is_none")]
    pub ref_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimMeta {
    #[serde(rename = "resourceType")]
    pub resource_type: String,
    pub created: String,
    #[serde(rename = "lastModified")]
    pub last_modified: String,
    pub location: String,
}

/// SCIM PATCH request envelope (RFC 7644 Section 3.5.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimPatchRequest {
    pub schemas: Vec<String>,
    #[serde(rename = "Operations")]
    pub operations: Vec<ScimPatchOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimPatchOp {
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

fn format_datetime(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl ScimUser {
    /// Convert a DB `User` row to a SCIM User resource.
    ///
    /// `groups` is not populated here — callers must set it separately if needed,
    /// as it requires an additional DB query.
    pub fn from_db_user(user: &User) -> Self {
        let id_str = user.id.to_string();
        let created = format_datetime(&user.created_at);

        // Build the name sub-attribute only if at least one part is present.
        let name = if user.given_name.is_some() || user.family_name.is_some() {
            Some(ScimName {
                given_name: user.given_name.clone(),
                family_name: user.family_name.clone(),
            })
        } else {
            None
        };

        ScimUser {
            schemas: vec![USER_SCHEMA.to_string()],
            id: id_str.clone(),
            external_id: user.external_id.clone(),
            user_name: user.email.clone(),
            active: user.active,
            name,
            display_name: user.display_name.clone(),
            emails: Some(vec![ScimEmail {
                value: user.email.clone(),
                primary: true,
                email_type: Some("work".to_string()),
            }]),
            groups: None,
            meta: ScimMeta {
                resource_type: "User".to_string(),
                created: created.clone(),
                last_modified: created,
                location: format!("/scim/v2/Users/{id_str}"),
            },
        }
    }
}

impl ScimGroup {
    /// Convert a DB `ScimGroupRow` to a SCIM Group resource (without members).
    ///
    /// Members require a separate query; callers must populate `members` themselves.
    pub fn from_db_scim_group(group: &crate::db::schema::ScimGroupRow) -> Self {
        let id_str = group.id.to_string();
        let created = format_datetime(&group.created_at);

        ScimGroup {
            schemas: vec![GROUP_SCHEMA.to_string()],
            id: id_str.clone(),
            external_id: group.external_id.clone(),
            display_name: group.display_name.clone(),
            members: None,
            meta: ScimMeta {
                resource_type: "Group".to_string(),
                created: created.clone(),
                last_modified: created,
                location: format!("/scim/v2/Groups/{id_str}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn sample_user() -> User {
        User {
            id: Uuid::new_v4(),
            email: "alice@example.com".to_string(),
            team_id: None,
            role: "member".to_string(),
            spend_limit_monthly_usd: None,
            budget_period: "monthly".to_string(),
            created_at: Utc::now(),
            active: true,
            external_id: Some("okta-123".to_string()),
            display_name: Some("Alice Smith".to_string()),
            given_name: Some("Alice".to_string()),
            family_name: Some("Smith".to_string()),
            scim_managed: true,
            idp_id: None,
        }
    }

    fn sample_scim_group() -> crate::db::schema::ScimGroupRow {
        crate::db::schema::ScimGroupRow {
            id: Uuid::new_v4(),
            external_id: Some("okta-group-456".to_string()),
            display_name: "Engineering".to_string(),
            idp_id: Uuid::new_v4(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn scim_user_from_db_user() {
        let user = sample_user();
        let scim = ScimUser::from_db_user(&user);

        assert_eq!(scim.schemas, vec![USER_SCHEMA]);
        assert_eq!(scim.id, user.id.to_string());
        assert_eq!(scim.user_name, "alice@example.com");
        assert!(scim.active);
        assert_eq!(scim.external_id.as_deref(), Some("okta-123"));
        assert_eq!(scim.display_name.as_deref(), Some("Alice Smith"));

        let name = scim.name.unwrap();
        assert_eq!(name.given_name.as_deref(), Some("Alice"));
        assert_eq!(name.family_name.as_deref(), Some("Smith"));

        let emails = scim.emails.unwrap();
        assert_eq!(emails[0].value, "alice@example.com");
        assert!(emails[0].primary);

        assert!(scim.meta.location.contains(&user.id.to_string()));
        assert_eq!(scim.meta.resource_type, "User");
    }

    #[test]
    fn scim_user_no_name_when_both_absent() {
        let mut user = sample_user();
        user.given_name = None;
        user.family_name = None;
        let scim = ScimUser::from_db_user(&user);
        assert!(scim.name.is_none());
    }

    #[test]
    fn scim_group_from_db_scim_group() {
        let group = sample_scim_group();
        let scim = ScimGroup::from_db_scim_group(&group);

        assert_eq!(scim.schemas, vec![GROUP_SCHEMA]);
        assert_eq!(scim.id, group.id.to_string());
        assert_eq!(scim.display_name, "Engineering");
        assert_eq!(scim.external_id.as_deref(), Some("okta-group-456"));
        assert!(scim.members.is_none());
        assert!(scim.meta.location.contains(&group.id.to_string()));
    }

    #[test]
    fn scim_patch_request_roundtrip() {
        let json = serde_json::json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [
                { "op": "replace", "path": "active", "value": false }
            ]
        });
        let req: ScimPatchRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.operations.len(), 1);
        assert_eq!(req.operations[0].op, "replace");
        assert_eq!(req.operations[0].path.as_deref(), Some("active"));
    }

    // --- Additional tests (from test-agent) ---

    /// ScimUser serializes and deserializes back to the same data.
    #[test]
    fn scim_user_serialization_roundtrip() {
        let user = sample_user();
        let scim = ScimUser::from_db_user(&user);

        let json = serde_json::to_string(&scim).unwrap();
        let deserialized: ScimUser = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, scim.id);
        assert_eq!(deserialized.user_name, scim.user_name);
        assert_eq!(deserialized.active, scim.active);
        assert_eq!(deserialized.external_id, scim.external_id);
        assert_eq!(deserialized.display_name, scim.display_name);
        assert_eq!(deserialized.meta.resource_type, scim.meta.resource_type);
        assert_eq!(deserialized.meta.location, scim.meta.location);
    }

    /// When external_id is None, the `externalId` JSON key must be absent.
    #[test]
    fn scim_user_skips_none_fields() {
        let mut user = sample_user();
        user.external_id = None;
        user.display_name = None;
        let scim = ScimUser::from_db_user(&user);

        let json = serde_json::to_value(&scim).unwrap();
        let obj = json.as_object().unwrap();

        assert!(
            !obj.contains_key("externalId"),
            "externalId key must be absent when None (skip_serializing_if)"
        );
        assert!(
            !obj.contains_key("displayName"),
            "displayName key must be absent when None"
        );
    }

    /// Meta location for a user follows the `/scim/v2/Users/{id}` pattern.
    #[test]
    fn scim_meta_location_format_user() {
        let user = sample_user();
        let scim = ScimUser::from_db_user(&user);
        assert_eq!(
            scim.meta.location,
            format!("/scim/v2/Users/{}", user.id),
            "User meta location must be /scim/v2/Users/{{id}}"
        );
    }

    /// Meta location for a group follows the `/scim/v2/Groups/{id}` pattern.
    #[test]
    fn scim_meta_location_format_group() {
        let group = sample_scim_group();
        let scim = ScimGroup::from_db_scim_group(&group);
        assert_eq!(
            scim.meta.location,
            format!("/scim/v2/Groups/{}", group.id),
            "Group meta location must be /scim/v2/Groups/{{id}}"
        );
    }

    /// Full PATCH request deserialization matching the spec format (Operations with capital O).
    #[test]
    fn scim_patch_request_deserialization() {
        let json_str = r#"{
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [
                { "op": "replace", "path": "active", "value": false },
                { "op": "replace", "path": "displayName", "value": "New Name" },
                { "op": "add", "path": "members", "value": [{"value": "user-uuid-1"}] }
            ]
        }"#;

        let req: ScimPatchRequest = serde_json::from_str(json_str).unwrap();

        // Schemas field
        assert_eq!(
            req.schemas,
            vec!["urn:ietf:params:scim:api:messages:2.0:PatchOp"]
        );

        // Three operations
        assert_eq!(req.operations.len(), 3);

        // First op: deactivate
        assert_eq!(req.operations[0].op, "replace");
        assert_eq!(req.operations[0].path.as_deref(), Some("active"));
        assert_eq!(
            req.operations[0].value.as_ref().unwrap(),
            &serde_json::json!(false)
        );

        // Second op: rename
        assert_eq!(req.operations[1].op, "replace");
        assert_eq!(req.operations[1].path.as_deref(), Some("displayName"));

        // Third op: add member
        assert_eq!(req.operations[2].op, "add");
        assert_eq!(req.operations[2].path.as_deref(), Some("members"));
    }

    /// ScimGroup with no external_id omits `externalId` from JSON.
    #[test]
    fn scim_group_skips_none_external_id() {
        let mut group = sample_scim_group();
        group.external_id = None;
        let scim = ScimGroup::from_db_scim_group(&group);

        let json = serde_json::to_value(&scim).unwrap();
        let obj = json.as_object().unwrap();
        assert!(
            !obj.contains_key("externalId"),
            "externalId must be absent from Group JSON when None"
        );
    }
}
// #[cfg(test)]
