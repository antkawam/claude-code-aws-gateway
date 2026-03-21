use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum KeysCommands {
    /// Create a new virtual API key
    Create {
        /// Key name (descriptive label)
        #[arg(long)]
        name: String,

        /// Rate limit in requests per minute (0 = unlimited)
        #[arg(long, default_value = "0")]
        rate_limit: i32,

        /// Assign to team ID
        #[arg(long)]
        team: Option<String>,
    },

    /// List all virtual keys
    List,

    /// Revoke a virtual key
    Revoke {
        /// Key ID to revoke
        key_id: String,
    },

    /// Delete a virtual key permanently
    Delete {
        /// Key ID to delete
        key_id: String,
    },

    /// Generate a setup token for a key
    SetupToken {
        /// Key ID to generate setup token for
        key_id: String,
    },
}

pub async fn run(cmd: KeysCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url.clone(), token).await?;

    match cmd {
        KeysCommands::Create {
            name,
            rate_limit,
            team,
        } => {
            let mut body = serde_json::json!({
                "name": name,
                "rate_limit_rpm": rate_limit,
            });
            if let Some(team_id) = team {
                body["team_id"] = serde_json::Value::String(team_id);
            }

            let resp = client.post("/admin/keys", &body).await?;
            if let Some(key) = resp["key"].as_str() {
                util::success(&format!("Key created: {name}"));
                eprintln!();
                eprintln!("API Key (save this - it won't be shown again):");
                println!("{key}");
                eprintln!();
                eprintln!("Configure Claude Code:");
                if let Some(ref gateway_url) = url {
                    eprintln!("  export ANTHROPIC_BASE_URL={gateway_url}");
                }
                eprintln!("  export ANTHROPIC_API_KEY={key}");
            }
        }

        KeysCommands::List => {
            let resp = client.get("/admin/keys").await?;
            if let Some(keys) = resp["keys"].as_array() {
                if keys.is_empty() {
                    eprintln!("No virtual keys found.");
                    return Ok(());
                }
                eprintln!(
                    "{:<36}  {:<20}  {:<8}  {:<10}  CREATED",
                    "ID", "NAME", "ACTIVE", "RPM"
                );
                eprintln!("{}", "-".repeat(90));
                for key in keys {
                    println!(
                        "{:<36}  {:<20}  {:<8}  {:<10}  {}",
                        key["id"].as_str().unwrap_or("-"),
                        key["name"].as_str().unwrap_or("-"),
                        if key["is_active"].as_bool().unwrap_or(false) {
                            "yes"
                        } else {
                            "no"
                        },
                        key["rate_limit_rpm"]
                            .as_i64()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        key["created_at"].as_str().unwrap_or("-"),
                    );
                }
            }
        }

        KeysCommands::Revoke { key_id } => {
            client
                .post(
                    &format!("/admin/keys/{key_id}/revoke"),
                    &serde_json::json!({}),
                )
                .await?;
            util::success(&format!("Key {key_id} revoked"));
        }

        KeysCommands::Delete { key_id } => {
            client.delete(&format!("/admin/keys/{key_id}")).await?;
            util::success(&format!("Key {key_id} deleted"));
        }

        KeysCommands::SetupToken { key_id } => {
            let resp = client
                .post(
                    &format!("/admin/keys/{key_id}/setup-token"),
                    &serde_json::json!({}),
                )
                .await?;
            if let Some(token) = resp["token"].as_str() {
                println!("{token}");
            } else {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        }
    }
    Ok(())
}
