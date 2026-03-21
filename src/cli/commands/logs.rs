use anyhow::{Context, Result};
use clap::Args;

use crate::util;

#[derive(Args)]
pub struct LogsArgs {
    /// Number of log events to show (default: 100)
    #[arg(long, default_value = "100")]
    limit: u32,

    /// Follow logs in real-time
    #[arg(long, short)]
    follow: bool,

    /// Filter pattern (CloudWatch Logs filter syntax)
    #[arg(long)]
    filter: Option<String>,

    /// Time range start (e.g., "1h", "30m", "2024-01-01")
    #[arg(long, default_value = "1h")]
    since: String,

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

pub async fn run(args: LogsArgs) -> Result<()> {
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

    let log_group = format!("/ecs/{}", args.stack_name.to_lowercase());

    let mut cmd_args = vec![
        "logs".to_string(),
        "tail".to_string(),
        log_group,
        "--format".to_string(),
        "short".to_string(),
        "--since".to_string(),
        args.since,
        "--region".to_string(),
        region.to_string(),
    ];

    if args.follow {
        cmd_args.push("--follow".to_string());
    }

    if let Some(ref filter) = args.filter {
        cmd_args.push("--filter-pattern".to_string());
        cmd_args.push(filter.clone());
    }

    util::info("Tailing CloudWatch logs (Ctrl+C to stop)...");

    let status = tokio::process::Command::new("aws")
        .args(&cmd_args)
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("Failed to tail logs. Is the stack deployed?");
    }

    Ok(())
}
