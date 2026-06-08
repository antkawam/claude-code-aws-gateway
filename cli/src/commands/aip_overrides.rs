use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;

#[derive(Subcommand)]
pub enum AipOverridesCommands {
    /// List all AIP overrides for an endpoint
    List {
        /// Endpoint ID
        endpoint: String,
    },

    /// Add an AIP override for a specific model on an endpoint
    Add {
        /// Endpoint ID
        endpoint: String,

        /// Model ID (e.g. claude-sonnet-4-5)
        #[arg(long)]
        model: String,

        /// Application Inference Profile ARN
        #[arg(long)]
        arn: String,

        /// Optional reason for the override
        #[arg(long)]
        reason: Option<String>,
    },

    /// Remove an AIP override for a specific model on an endpoint
    Remove {
        /// Endpoint ID
        endpoint: String,

        /// Model ID
        #[arg(long)]
        model: String,
    },
}

pub async fn run(
    cmd: AipOverridesCommands,
    url: Option<String>,
    token: Option<String>,
) -> Result<()> {
    match cmd {
        AipOverridesCommands::List { endpoint } => list(endpoint, url, token).await,
        AipOverridesCommands::Add {
            endpoint,
            model,
            arn,
            reason,
        } => add(endpoint, model, arn, reason, url, token).await,
        AipOverridesCommands::Remove { endpoint, model } => {
            remove(endpoint, model, url, token).await
        }
    }
}

async fn list(endpoint: String, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;
    let path = format!("/admin/endpoints/{endpoint}/aip-overrides");
    let resp = client.get(&path).await?;

    if let Some(overrides) = resp["overrides"].as_array() {
        if overrides.is_empty() {
            eprintln!("No AIP overrides found for endpoint {endpoint}.");
            return Ok(());
        }
        eprintln!(
            "{:<30}  {:<80}  {:<24}  SET-BY",
            "MODEL", "AIP ARN", "SET AT"
        );
        eprintln!("{}", "-".repeat(150));
        for o in overrides {
            println!(
                "{:<30}  {:<80}  {:<24}  {}",
                o["model_id"].as_str().unwrap_or("-"),
                o["aip_arn"].as_str().unwrap_or("-"),
                o["set_at"].as_str().unwrap_or("-"),
                o["set_by"].as_str().unwrap_or("-"),
            );
            if let Some(reason) = o["reason"].as_str() {
                println!("  reason: {reason}");
            }
        }
    }

    Ok(())
}

async fn add(
    endpoint: String,
    model: String,
    arn: String,
    reason: Option<String>,
    url: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;
    let path = format!("/admin/endpoints/{endpoint}/aip-overrides");

    let mut body = serde_json::json!({
        "model_id": model,
        "aip_arn": arn,
    });
    if let Some(r) = reason {
        body["reason"] = serde_json::Value::String(r);
    }

    let resp = client.post(&path, &body).await?;
    let model_id = resp["model_id"].as_str().unwrap_or(&model);
    crate::util::success(&format!(
        "AIP override added: {model_id} on endpoint {endpoint}"
    ));

    Ok(())
}

async fn remove(
    endpoint: String,
    model: String,
    url: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;
    let path = format!("/admin/endpoints/{endpoint}/aip-overrides/{model}");

    client.delete(&path).await?;
    crate::util::success(&format!(
        "AIP override removed: {model} from endpoint {endpoint}"
    ));

    Ok(())
}
