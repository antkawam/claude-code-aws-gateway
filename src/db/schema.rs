use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub budget_amount_usd: Option<f64>,
    pub budget_period: String,
    pub budget_policy: Option<serde_json::Value>,
    pub default_user_budget_usd: Option<f64>,
    pub notify_recipients: String,
    pub routing_strategy: String,
    pub created_at: DateTime<Utc>,
    pub external_id: Option<String>,
    pub display_name: Option<String>,
    pub scim_managed: bool,
    pub idp_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub team_id: Option<Uuid>,
    pub role: String,
    pub spend_limit_monthly_usd: Option<f64>,
    pub budget_period: String,
    pub created_at: DateTime<Utc>,
    pub active: bool,
    pub external_id: Option<String>,
    pub display_name: Option<String>,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub scim_managed: bool,
    pub idp_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct VirtualKey {
    pub id: Uuid,
    pub key_hash: String,
    pub key_prefix: String,
    pub name: Option<String>,
    pub user_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub is_active: bool,
    pub rate_limit_rpm: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Endpoint {
    pub id: Uuid,
    pub name: String,
    pub role_arn: Option<String>,
    pub external_id: Option<String>,
    pub inference_profile_arn: Option<String>,
    pub region: String,
    pub routing_prefix: String,
    pub priority: i32,
    pub is_default: bool,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TeamEndpoint {
    pub team_id: Uuid,
    pub endpoint_id: Uuid,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct IdentityProvider {
    pub id: Uuid,
    pub name: String,
    pub issuer_url: String,
    pub client_id: Option<String>,
    pub audience: Option<String>,
    pub jwks_url: Option<String>,
    pub flow_type: String,
    pub auto_provision: bool,
    pub default_role: String,
    pub allowed_domains: Option<Vec<String>>,
    pub enabled: bool,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub user_claim: Option<String>,
    pub scopes: Option<String>,
    pub scim_enabled: bool,
    pub scim_admin_groups: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ScimToken {
    pub id: Uuid,
    pub idp_id: Uuid,
    pub token_hash: String,
    pub token_prefix: String,
    pub name: Option<String>,
    pub created_by: String,
    pub enabled: bool,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ScimGroupRow {
    pub id: Uuid,
    pub external_id: Option<String>,
    pub display_name: String,
    pub idp_id: Uuid,
    pub created_at: DateTime<Utc>,
}
