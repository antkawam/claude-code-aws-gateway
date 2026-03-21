use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Get a runtime setting value
    Get {
        /// Setting key (e.g., virtual_keys_enabled, admin_login_enabled)
        key: String,
    },

    /// Set a runtime setting value
    Set {
        /// Setting key
        key: String,
        /// Setting value
        value: String,
    },

    /// List all runtime settings
    List,
}

pub async fn run(cmd: ConfigCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        ConfigCommands::Get { key } => {
            let resp = client.get("/admin/settings").await?;
            if let Some(settings) = resp.as_object() {
                if let Some(value) = settings.get(&key) {
                    println!("{key} = {value}");
                } else {
                    util::warn(&format!("Setting '{key}' not found"));
                }
            }
        }

        ConfigCommands::Set { key, value } => {
            client
                .put(
                    &format!("/admin/settings/{key}"),
                    &serde_json::json!({ "value": value }),
                )
                .await?;
            util::success(&format!("{key} = {value}"));
        }

        ConfigCommands::List => {
            let resp = client.get("/admin/settings").await?;
            if let Some(settings) = resp.as_object() {
                for (key, value) in settings {
                    println!("{key} = {value}");
                }
            }
        }
    }
    Ok(())
}
