use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum IdpsCommands {
    /// Create a new identity provider
    Create {
        /// Provider name
        #[arg(long)]
        name: String,

        /// OIDC issuer URL
        #[arg(long)]
        issuer_url: String,

        /// OIDC client ID
        #[arg(long)]
        client_id: Option<String>,

        /// Expected audience claim
        #[arg(long)]
        audience: Option<String>,

        /// JWKS URL (auto-discovered from issuer if omitted)
        #[arg(long)]
        jwks_url: Option<String>,

        /// Authentication flow type
        #[arg(long, default_value = "device_code")]
        flow_type: String,

        /// Auto-provision users on first login
        #[arg(long)]
        auto_provision: bool,

        /// Default role for auto-provisioned users
        #[arg(long, default_value = "member")]
        default_role: String,

        /// Comma-separated allowed email domains
        #[arg(long)]
        allowed_domains: Option<String>,
    },

    /// List all identity providers
    List,

    /// Update an identity provider
    Update {
        /// IDP ID to update
        id: String,

        /// Provider name
        #[arg(long)]
        name: Option<String>,

        /// OIDC issuer URL
        #[arg(long)]
        issuer_url: Option<String>,

        /// OIDC client ID
        #[arg(long)]
        client_id: Option<String>,

        /// Expected audience claim
        #[arg(long)]
        audience: Option<String>,

        /// JWKS URL
        #[arg(long)]
        jwks_url: Option<String>,

        /// Authentication flow type
        #[arg(long)]
        flow_type: Option<String>,

        /// Auto-provision users on first login
        #[arg(long)]
        auto_provision: Option<bool>,

        /// Default role for auto-provisioned users
        #[arg(long)]
        default_role: Option<String>,

        /// Comma-separated allowed email domains
        #[arg(long)]
        allowed_domains: Option<String>,

        /// Enable or disable the IDP
        #[arg(long)]
        enabled: Option<bool>,
    },

    /// Delete an identity provider
    Delete {
        /// IDP ID to delete
        id: String,
    },
}

pub async fn run(cmd: IdpsCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        IdpsCommands::Create {
            name,
            issuer_url,
            client_id,
            audience,
            jwks_url,
            flow_type,
            auto_provision,
            default_role,
            allowed_domains,
        } => {
            let mut body = serde_json::json!({
                "name": name,
                "issuer_url": issuer_url,
                "flow_type": flow_type,
                "auto_provision": auto_provision,
                "default_role": default_role,
            });
            if let Some(v) = client_id {
                body["client_id"] = serde_json::Value::String(v);
            }
            if let Some(v) = audience {
                body["audience"] = serde_json::Value::String(v);
            }
            if let Some(v) = jwks_url {
                body["jwks_url"] = serde_json::Value::String(v);
            }
            if let Some(v) = allowed_domains {
                let domains: Vec<&str> = v.split(',').map(|s| s.trim()).collect();
                body["allowed_domains"] = serde_json::json!(domains);
            }

            let resp = client.post("/admin/idps", &body).await?;
            let id = resp["id"].as_str().unwrap_or("unknown");
            util::success(&format!("IDP created: {name} (id: {id})"));
        }

        IdpsCommands::List => {
            let resp = client.get("/admin/idps").await?;
            if let Some(idps) = resp["idps"].as_array() {
                if idps.is_empty() {
                    eprintln!("No identity providers found.");
                    return Ok(());
                }
                eprintln!(
                    "{:<36}  {:<20}  {:<40}  {:<12}  ENABLED",
                    "ID", "NAME", "ISSUER", "FLOW_TYPE"
                );
                eprintln!("{}", "-".repeat(120));
                for idp in idps {
                    println!(
                        "{:<36}  {:<20}  {:<40}  {:<12}  {}",
                        idp["id"].as_str().unwrap_or("-"),
                        idp["name"].as_str().unwrap_or("-"),
                        idp["issuer_url"].as_str().unwrap_or("-"),
                        idp["flow_type"].as_str().unwrap_or("-"),
                        if idp["enabled"].as_bool().unwrap_or(true) {
                            "yes"
                        } else {
                            "no"
                        },
                    );
                }
            }
        }

        IdpsCommands::Update {
            id,
            name,
            issuer_url,
            client_id,
            audience,
            jwks_url,
            flow_type,
            auto_provision,
            default_role,
            allowed_domains,
            enabled,
        } => {
            let mut body = serde_json::json!({});
            if let Some(v) = name {
                body["name"] = serde_json::Value::String(v);
            }
            if let Some(v) = issuer_url {
                body["issuer_url"] = serde_json::Value::String(v);
            }
            if let Some(v) = client_id {
                body["client_id"] = serde_json::Value::String(v);
            }
            if let Some(v) = audience {
                body["audience"] = serde_json::Value::String(v);
            }
            if let Some(v) = jwks_url {
                body["jwks_url"] = serde_json::Value::String(v);
            }
            if let Some(v) = flow_type {
                body["flow_type"] = serde_json::Value::String(v);
            }
            if let Some(v) = auto_provision {
                body["auto_provision"] = serde_json::json!(v);
            }
            if let Some(v) = default_role {
                body["default_role"] = serde_json::Value::String(v);
            }
            if let Some(v) = allowed_domains {
                let domains: Vec<&str> = v.split(',').map(|s| s.trim()).collect();
                body["allowed_domains"] = serde_json::json!(domains);
            }
            if let Some(v) = enabled {
                body["enabled"] = serde_json::json!(v);
            }

            client.put(&format!("/admin/idps/{id}"), &body).await?;
            util::success(&format!("IDP {id} updated"));
        }

        IdpsCommands::Delete { id } => {
            client.delete(&format!("/admin/idps/{id}")).await?;
            util::success(&format!("IDP {id} deleted"));
        }
    }
    Ok(())
}
