use anyhow::Result;
use clap::Subcommand;
use uuid::Uuid;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand, Debug)]
pub enum BetasCommands {
    /// List all admin beta overrides
    ListOverrides,

    /// Set an override that wins over learned cache values (TTL-immune)
    Override {
        /// Endpoint UUID
        endpoint_id: Uuid,

        /// Bedrock profile ID, e.g. `us.anthropic.claude-opus-4-7`
        profile_id: String,

        /// Beta name, e.g. `context-1m-2025-08-07`
        beta_name: String,

        /// `true` to mark supported; `false` to mark unsupported
        #[arg(action = clap::ArgAction::Set)]
        supported: bool,

        /// Optional human-readable reason; stored alongside the override
        #[arg(long)]
        reason: Option<String>,
    },

    /// Remove an admin override; cache reverts to learned state (or absent)
    ClearOverride {
        /// Endpoint UUID
        endpoint_id: Uuid,

        /// Bedrock profile ID
        profile_id: String,

        /// Beta name
        beta_name: String,
    },
}

pub async fn run(cmd: BetasCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        BetasCommands::ListOverrides => {
            let resp = client.get("/admin/beta-overrides").await?;
            if let Some(overrides) = resp["overrides"].as_array() {
                if overrides.is_empty() {
                    eprintln!("No beta overrides configured.");
                    return Ok(());
                }
                eprintln!(
                    "{:<36}  {:<42}  {:<30}  {:<9}  REASON",
                    "ENDPOINT_ID", "PROFILE_ID", "BETA_NAME", "SUPPORTED"
                );
                eprintln!("{}", "-".repeat(165));
                for ovr in overrides {
                    println!(
                        "{:<36}  {:<42}  {:<30}  {:<9}  {}",
                        ovr["endpoint_id"].as_str().unwrap_or("-"),
                        ovr["profile_id"].as_str().unwrap_or("-"),
                        ovr["beta_name"].as_str().unwrap_or("-"),
                        if ovr["supported"].as_bool().unwrap_or(false) {
                            "true"
                        } else {
                            "false"
                        },
                        ovr["reason"].as_str().unwrap_or("-"),
                    );
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
        }

        BetasCommands::Override {
            endpoint_id,
            profile_id,
            beta_name,
            supported,
            reason,
        } => {
            let mut body = serde_json::json!({
                "endpoint_id": endpoint_id.to_string(),
                "profile_id": profile_id,
                "beta_name": beta_name,
                "supported": supported,
            });
            if let Some(r) = reason {
                body["reason"] = serde_json::Value::String(r);
            }

            let resp = client.post("/admin/beta-overrides", &body).await?;
            util::success(&format!(
                "Beta override set: endpoint={} profile={} beta={} supported={}",
                resp["endpoint_id"]
                    .as_str()
                    .unwrap_or(&endpoint_id.to_string()),
                resp["profile_id"].as_str().unwrap_or(&profile_id),
                resp["beta_name"].as_str().unwrap_or(&beta_name),
                resp["supported"].as_bool().unwrap_or(supported),
            ));
        }

        BetasCommands::ClearOverride {
            endpoint_id,
            profile_id,
            beta_name,
        } => {
            client
                .delete(&format!(
                    "/admin/beta-overrides/{}/{}/{}",
                    endpoint_id, profile_id, beta_name,
                ))
                .await?;
            util::success(&format!(
                "Beta override cleared: endpoint={} profile={} beta={}",
                endpoint_id, profile_id, beta_name,
            ));
        }
    }

    Ok(())
}
