use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum EndpointsCommands {
    /// List all Bedrock endpoints
    List,

    /// Create a new Bedrock endpoint
    Create {
        /// Endpoint name
        #[arg(long)]
        name: String,

        /// AWS region (e.g. us-east-1)
        #[arg(long)]
        region: String,

        /// Routing prefix for model ID mapping
        #[arg(long)]
        routing_prefix: String,

        /// IAM role ARN for cross-account access
        #[arg(long)]
        role_arn: Option<String>,

        /// External ID for role assumption
        #[arg(long)]
        external_id: Option<String>,

        /// Inference profile ARN
        #[arg(long)]
        inference_profile_arn: Option<String>,

        /// Routing priority (lower = preferred)
        #[arg(long)]
        priority: Option<i32>,
    },

    /// Update an existing Bedrock endpoint
    Update {
        /// Endpoint ID
        id: String,

        /// Endpoint name
        #[arg(long)]
        name: Option<String>,

        /// AWS region
        #[arg(long)]
        region: Option<String>,

        /// Routing prefix
        #[arg(long)]
        routing_prefix: Option<String>,

        /// IAM role ARN for cross-account access
        #[arg(long)]
        role_arn: Option<String>,

        /// External ID for role assumption
        #[arg(long)]
        external_id: Option<String>,

        /// Inference profile ARN
        #[arg(long)]
        inference_profile_arn: Option<String>,

        /// Routing priority (lower = preferred)
        #[arg(long)]
        priority: Option<i32>,

        /// Enable or disable the endpoint
        #[arg(long, default_value = "true")]
        enabled: bool,
    },

    /// Delete a Bedrock endpoint
    Delete {
        /// Endpoint ID
        id: String,
    },

    /// Set an endpoint as the default
    SetDefault {
        /// Endpoint ID
        id: String,
    },

    /// Show quota information for an endpoint
    Quotas {
        /// Endpoint ID
        id: String,
    },

    /// List available models for an endpoint
    Models {
        /// Endpoint ID
        id: String,
    },
}

pub async fn run(cmd: EndpointsCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        EndpointsCommands::List => {
            let resp = client.get("/admin/endpoints").await?;
            if let Some(endpoints) = resp["endpoints"].as_array() {
                if endpoints.is_empty() {
                    eprintln!("No endpoints found.");
                    return Ok(());
                }
                eprintln!(
                    "{:<36}  {:<20}  {:<16}  {:<8}  {:<8}  {:<8}  PRIORITY",
                    "ID", "NAME", "REGION", "HEALTH", "DEFAULT", "ENABLED"
                );
                eprintln!("{}", "-".repeat(110));
                for ep in endpoints {
                    println!(
                        "{:<36}  {:<20}  {:<16}  {:<8}  {:<8}  {:<8}  {}",
                        ep["id"].as_str().unwrap_or("-"),
                        ep["name"].as_str().unwrap_or("-"),
                        ep["region"].as_str().unwrap_or("-"),
                        ep["health"].as_str().unwrap_or("-"),
                        if ep["is_default"].as_bool().unwrap_or(false) {
                            "yes"
                        } else {
                            "no"
                        },
                        if ep["enabled"].as_bool().unwrap_or(true) {
                            "yes"
                        } else {
                            "no"
                        },
                        ep["priority"]
                            .as_i64()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                }
            }
        }

        EndpointsCommands::Create {
            name,
            region,
            routing_prefix,
            role_arn,
            external_id,
            inference_profile_arn,
            priority,
        } => {
            let mut body = serde_json::json!({
                "name": name,
                "region": region,
                "routing_prefix": routing_prefix,
            });
            if let Some(arn) = role_arn {
                body["role_arn"] = serde_json::Value::String(arn);
            }
            if let Some(eid) = external_id {
                body["external_id"] = serde_json::Value::String(eid);
            }
            if let Some(ipa) = inference_profile_arn {
                body["inference_profile_arn"] = serde_json::Value::String(ipa);
            }
            if let Some(p) = priority {
                body["priority"] = serde_json::json!(p);
            }

            let resp = client.post("/admin/endpoints", &body).await?;
            let id = resp["id"].as_str().unwrap_or("unknown");
            util::success(&format!("Endpoint created: {name} (id: {id})"));
        }

        EndpointsCommands::Update {
            id,
            name,
            region,
            routing_prefix,
            role_arn,
            external_id,
            inference_profile_arn,
            priority,
            enabled,
        } => {
            let mut body = serde_json::json!({
                "enabled": enabled,
            });
            if let Some(n) = name {
                body["name"] = serde_json::Value::String(n);
            }
            if let Some(r) = region {
                body["region"] = serde_json::Value::String(r);
            }
            if let Some(rp) = routing_prefix {
                body["routing_prefix"] = serde_json::Value::String(rp);
            }
            if let Some(arn) = role_arn {
                body["role_arn"] = serde_json::Value::String(arn);
            }
            if let Some(eid) = external_id {
                body["external_id"] = serde_json::Value::String(eid);
            }
            if let Some(ipa) = inference_profile_arn {
                body["inference_profile_arn"] = serde_json::Value::String(ipa);
            }
            if let Some(p) = priority {
                body["priority"] = serde_json::json!(p);
            }

            client.put(&format!("/admin/endpoints/{id}"), &body).await?;
            util::success(&format!("Endpoint {id} updated"));
        }

        EndpointsCommands::Delete { id } => {
            client.delete(&format!("/admin/endpoints/{id}")).await?;
            util::success(&format!("Endpoint {id} deleted"));
        }

        EndpointsCommands::SetDefault { id } => {
            client
                .put(
                    &format!("/admin/endpoints/{id}/default"),
                    &serde_json::json!({}),
                )
                .await?;
            util::success(&format!("Endpoint {id} set as default"));
        }

        EndpointsCommands::Quotas { id } => {
            let resp = client.get(&format!("/admin/endpoints/{id}/quotas")).await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }

        EndpointsCommands::Models { id } => {
            let resp = client.get(&format!("/admin/endpoints/{id}/models")).await?;
            if let Some(name) = resp["endpoint_name"].as_str() {
                eprintln!("Endpoint: {name}");
            }
            if let Some(profile) = resp["inference_profile"].as_str() {
                eprintln!("Inference profile: {profile}");
            }
            if let Some(models) = resp["models"].as_array() {
                if models.is_empty() {
                    eprintln!("No models found.");
                    return Ok(());
                }
                eprintln!();
                eprintln!("Models:");
                for model in models {
                    if let Some(arn) = model.as_str() {
                        println!("  {arn}");
                    } else if let Some(arn) = model["model_arn"].as_str() {
                        println!("  {arn}");
                    } else {
                        println!("  {model}");
                    }
                }
            }
        }
    }
    Ok(())
}
