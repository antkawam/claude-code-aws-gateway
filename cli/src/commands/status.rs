use anyhow::{Context, Result};
use clap::Args;

use crate::util;

#[derive(Args)]
pub struct StatusArgs {
    /// Show extended details (task IPs, deployment info)
    #[arg(long)]
    verbose: bool,

    /// CloudFormation stack name (overrides CCAG_STACK_NAME env var)
    #[arg(long, env = "CCAG_STACK_NAME", default_value = "CCAG")]
    stack_name: String,

    /// AWS region (overrides AWS_REGION env var)
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,

    /// AWS CLI profile
    #[arg(long, env = "AWS_PROFILE")]
    profile: Option<String>,
}

pub async fn run(args: StatusArgs, url: Option<String>) -> Result<()> {
    let region = args
        .region
        .as_deref()
        .context("AWS region required. Set AWS_REGION env var or pass --region flag.")?;

    unsafe {
        if let Some(ref profile) = args.profile {
            std::env::set_var("AWS_PROFILE", profile);
        }
        std::env::set_var("AWS_REGION", region);
    }

    eprintln!("Stack:   {}", args.stack_name);
    eprintln!("Region:  {region}");
    if let Some(ref gateway_url) = url {
        eprintln!("URL:     {gateway_url}");
    }
    eprintln!();

    // Check CloudFormation stack status
    let output = tokio::process::Command::new("aws")
        .args([
            "cloudformation",
            "describe-stacks",
            "--stack-name",
            &args.stack_name,
            "--query",
            "Stacks[0].StackStatus",
            "--output",
            "text",
            "--region",
            region,
        ])
        .output()
        .await?;

    let stack_status = String::from_utf8(output.stdout)?.trim().to_string();
    if stack_status.is_empty() || stack_status.contains("does not exist") {
        util::warn("Stack not found.");
        return Ok(());
    }

    eprintln!("Stack status: {stack_status}");

    // Check ECS service
    let cluster_name = format!("{}-Cluster", args.stack_name);
    let service_name = format!("{}-Service", args.stack_name);

    let output = tokio::process::Command::new("aws")
        .args([
            "ecs",
            "describe-services",
            "--cluster",
            &cluster_name,
            "--services",
            &service_name,
            "--query",
            "services[0].{running:runningCount,desired:desiredCount,status:status,image:taskDefinition}",
            "--output",
            "json",
            "--region",
            region,
        ])
        .output()
        .await?;

    if let Ok(svc) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        let running = svc["running"].as_i64().unwrap_or(0);
        let desired = svc["desired"].as_i64().unwrap_or(0);
        let status = svc["status"].as_str().unwrap_or("UNKNOWN");
        eprintln!("Service:      {status} ({running}/{desired} tasks running)");
    }

    // Health check
    if let Some(ref gateway_url) = url {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()?;

        match client.get(format!("{gateway_url}/health")).send().await {
            Ok(resp) if resp.status().is_success() => {
                util::success("Gateway is healthy");
            }
            Ok(resp) => {
                util::warn(&format!("Gateway returned {}", resp.status()));
            }
            Err(e) => {
                util::warn(&format!("Gateway unreachable: {e}"));
            }
        }

        if args.verbose {
            match client
                .get(format!("{gateway_url}/health/deep"))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        eprintln!();
                        eprintln!("Health details:");
                        eprintln!("{}", serde_json::to_string_pretty(&body)?);
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}
