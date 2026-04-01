use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum ScimCommands {
    /// Enable SCIM provisioning for an identity provider
    Enable {
        /// IDP ID or name
        #[arg(long)]
        idp: String,
    },

    /// Disable SCIM provisioning for an identity provider
    Disable {
        /// IDP ID or name
        #[arg(long)]
        idp: String,
    },

    /// Generate a SCIM bearer token for an identity provider
    CreateToken {
        /// IDP ID or name
        #[arg(long)]
        idp: String,

        /// Token name/label
        #[arg(long)]
        name: Option<String>,
    },

    /// List SCIM tokens for an identity provider
    ListTokens {
        /// IDP ID or name
        #[arg(long)]
        idp: String,
    },

    /// Revoke a SCIM token
    RevokeToken {
        /// Token ID to revoke
        #[arg(long)]
        token_id: String,

        /// IDP ID (required for the API path)
        #[arg(long)]
        idp: String,
    },

    /// Set which SCIM groups map to admin role
    SetAdminGroups {
        /// IDP ID or name
        #[arg(long)]
        idp: String,

        /// Comma-separated group names that grant admin role
        #[arg(long)]
        groups: String,
    },

    /// Show SCIM configuration status for an identity provider
    Status {
        /// IDP ID or name
        #[arg(long)]
        idp: String,
    },
}

pub async fn run(cmd: ScimCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        ScimCommands::Enable { idp } => {
            let idp_id = resolve_idp_id(&client, &idp).await?;
            client
                .put(
                    &format!("/admin/idps/{idp_id}/scim"),
                    &serde_json::json!({ "enabled": true }),
                )
                .await?;
            util::success(&format!("SCIM enabled for IDP {idp}"));
        }

        ScimCommands::Disable { idp } => {
            let idp_id = resolve_idp_id(&client, &idp).await?;
            client
                .put(
                    &format!("/admin/idps/{idp_id}/scim"),
                    &serde_json::json!({ "enabled": false }),
                )
                .await?;
            util::success(&format!("SCIM disabled for IDP {idp}"));
        }

        ScimCommands::CreateToken { idp, name } => {
            let idp_id = resolve_idp_id(&client, &idp).await?;
            let mut body = serde_json::json!({});
            if let Some(n) = name {
                body["name"] = serde_json::Value::String(n);
            }
            let resp = client
                .post(&format!("/admin/idps/{idp_id}/scim-tokens"), &body)
                .await?;

            let token_val = resp["token"].as_str().unwrap_or("-");
            let token_id = resp["id"].as_str().unwrap_or("-");

            // Print token to stdout for piping
            println!("{token_val}");

            // Print metadata to stderr
            eprintln!();
            util::success("SCIM token created");
            eprintln!("  ID: {token_id}");
            eprintln!("  Prefix: {}", resp["token_prefix"].as_str().unwrap_or("-"));
            eprintln!();
            util::warn("Copy this token now — it won't be shown again.");
        }

        ScimCommands::ListTokens { idp } => {
            let idp_id = resolve_idp_id(&client, &idp).await?;
            let resp = client
                .get(&format!("/admin/idps/{idp_id}/scim-tokens"))
                .await?;
            if let Some(tokens) = resp["tokens"].as_array() {
                if tokens.is_empty() {
                    eprintln!("No SCIM tokens found for this IDP.");
                    return Ok(());
                }
                eprintln!(
                    "{:<36}  {:<20}  {:<10}  {:<20}  LAST USED",
                    "ID", "PREFIX", "STATUS", "CREATED"
                );
                eprintln!("{}", "-".repeat(100));
                for t in tokens {
                    println!(
                        "{:<36}  {:<20}  {:<10}  {:<20}  {}",
                        t["id"].as_str().unwrap_or("-"),
                        t["token_prefix"].as_str().unwrap_or("-"),
                        if t["enabled"].as_bool().unwrap_or(false) {
                            "active"
                        } else {
                            "revoked"
                        },
                        t["created_at"].as_str().unwrap_or("-"),
                        t["last_used_at"].as_str().unwrap_or("never"),
                    );
                }
            }
        }

        ScimCommands::RevokeToken { token_id, idp } => {
            let idp_id = resolve_idp_id(&client, &idp).await?;
            client
                .delete(&format!("/admin/idps/{idp_id}/scim-tokens/{token_id}"))
                .await?;
            util::success(&format!("SCIM token {token_id} revoked"));
        }

        ScimCommands::SetAdminGroups { idp, groups } => {
            let idp_id = resolve_idp_id(&client, &idp).await?;
            let group_list: Vec<&str> = groups
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            client
                .put(
                    &format!("/admin/idps/{idp_id}/scim-admin-groups"),
                    &serde_json::json!({ "groups": group_list }),
                )
                .await?;
            util::success(&format!("Admin groups set for IDP {idp}: {:?}", group_list));
        }

        ScimCommands::Status { idp } => {
            let idp_id = resolve_idp_id(&client, &idp).await?;

            // Fetch IDP details
            let idps_resp = client.get("/admin/idps").await?;
            let idp_info = idps_resp["idps"]
                .as_array()
                .and_then(|arr| arr.iter().find(|i| i["id"].as_str() == Some(&idp_id)));

            let scim_enabled = idp_info
                .and_then(|i| i["scim_enabled"].as_bool())
                .unwrap_or(false);

            // Fetch admin groups
            let groups_resp = client
                .get(&format!("/admin/idps/{idp_id}/scim-admin-groups"))
                .await?;

            // Fetch tokens
            let tokens_resp = client
                .get(&format!("/admin/idps/{idp_id}/scim-tokens"))
                .await?;
            let token_count = tokens_resp["tokens"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            let active_token_count = tokens_resp["tokens"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|t| t["enabled"].as_bool().unwrap_or(false))
                        .count()
                })
                .unwrap_or(0);

            eprintln!(
                "SCIM Status for IDP: {}",
                idp_info.and_then(|i| i["name"].as_str()).unwrap_or(&idp)
            );
            eprintln!();
            eprintln!(
                "  SCIM Enabled:    {}",
                if scim_enabled { "yes" } else { "no" }
            );
            eprintln!("  Base URL:        /scim/v2");
            eprintln!(
                "  Tokens:          {} ({} active)",
                token_count, active_token_count
            );
            eprintln!("  Admin Groups:    {}", groups_resp["groups"]);
        }
    }
    Ok(())
}

/// Resolve an IDP identifier (UUID or name) to a UUID string.
/// If the input is a valid UUID, return it directly.
/// Otherwise, list all IDPs and find one matching by name.
async fn resolve_idp_id(client: &AdminClient, idp: &str) -> Result<String> {
    // Try parsing as UUID first
    if uuid::Uuid::parse_str(idp).is_ok() {
        return Ok(idp.to_string());
    }

    // Otherwise, resolve by name
    let resp = client.get("/admin/idps").await?;
    if let Some(idps) = resp["idps"].as_array() {
        for i in idps {
            if i["name"].as_str() == Some(idp)
                && let Some(id) = i["id"].as_str()
            {
                return Ok(id.to_string());
            }
        }
    }
    anyhow::bail!("IDP not found: {idp}")
}
