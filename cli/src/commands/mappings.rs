use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum MappingsCommands {
    /// List all model mappings.
    List {
        /// Output as JSON (machine-readable).
        #[arg(long)]
        json: bool,
    },

    /// Add a new model mapping (admin alias).
    Add {
        /// Short Anthropic-side prefix, e.g. "claude-sonnet-4-6"
        anthropic_prefix: String,
        /// Bedrock model suffix, must start with "anthropic."
        bedrock_suffix: String,
        /// Human-readable display name (optional)
        #[arg(long)]
        display: Option<String>,
    },

    /// Delete a model mapping by anthropic_prefix.
    Delete {
        /// Anthropic prefix to delete
        anthropic_prefix: String,
        /// Skip the interactive confirmation prompt
        #[arg(long)]
        yes: bool,
    },

    /// Run a discovery preview for a model id (does NOT persist).
    Discover {
        /// Raw model ID to probe, e.g. "claude-future-9-9"
        model: String,
    },
}

pub async fn run(cmd: MappingsCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    match cmd {
        MappingsCommands::List { json } => list(json, url, token).await,
        MappingsCommands::Add {
            anthropic_prefix,
            bedrock_suffix,
            display,
        } => add(anthropic_prefix, bedrock_suffix, display, url, token).await,
        MappingsCommands::Delete {
            anthropic_prefix,
            yes,
        } => delete(anthropic_prefix, yes, url, token).await,
        MappingsCommands::Discover { model } => discover(model, url, token).await,
    }
}

async fn list(json: bool, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;
    let resp = client.get("/admin/mappings").await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    if let Some(mappings) = resp["mappings"].as_array() {
        if mappings.is_empty() {
            eprintln!("No model mappings found.");
            return Ok(());
        }
        eprintln!(
            "{:<32}  {:<48}  {:<24}  {:<10}  {:<12}  {:<24}  CREATED_AT",
            "ANTHROPIC_PREFIX",
            "BEDROCK_SUFFIX",
            "DISPLAY",
            "SOURCE",
            "CREATED_VIA",
            "LAST_USED_AT"
        );
        eprintln!("{}", "-".repeat(185));
        for m in mappings {
            println!(
                "{:<32}  {:<48}  {:<24}  {:<10}  {:<12}  {:<24}  {}",
                m["anthropic_prefix"].as_str().unwrap_or("-"),
                m["bedrock_suffix"].as_str().unwrap_or("-"),
                m["anthropic_display"].as_str().unwrap_or("-"),
                m["source"].as_str().unwrap_or("-"),
                m["created_via"].as_str().unwrap_or("-"),
                m["last_used_at"].as_str().unwrap_or("-"),
                m["created_at"].as_str().unwrap_or("-"),
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    }

    Ok(())
}

async fn add(
    anthropic_prefix: String,
    bedrock_suffix: String,
    display: Option<String>,
    url: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    let mut body = serde_json::json!({
        "anthropic_prefix": anthropic_prefix,
        "bedrock_suffix": bedrock_suffix,
    });
    if let Some(d) = display {
        body["anthropic_display"] = serde_json::Value::String(d);
    }

    let resp = client.post("/admin/mappings", &body).await?;
    let prefix = resp["anthropic_prefix"]
        .as_str()
        .unwrap_or(&anthropic_prefix);
    let suffix = resp["bedrock_suffix"].as_str().unwrap_or(&bedrock_suffix);
    util::success(&format!("added: {prefix} -> {suffix}"));

    Ok(())
}

async fn delete(
    anthropic_prefix: String,
    yes: bool,
    url: Option<String>,
    token: Option<String>,
) -> Result<()> {
    if !yes {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("Delete mapping '{anthropic_prefix}'?"))
            .default(false)
            .interact()?;
        if !confirmed {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    let client = AdminClient::from_options(url, token).await?;
    let path = format!("/admin/mappings/{anthropic_prefix}");
    client.delete(&path).await?;
    util::success(&format!("deleted: {anthropic_prefix}"));

    Ok(())
}

async fn discover(model: String, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;
    let body = serde_json::json!({ "model": model });

    let resp = client.post("/admin/mappings/discover", &body).await?;

    println!(
        "anthropic_prefix:     {}",
        resp["anthropic_prefix"].as_str().unwrap_or("-")
    );
    println!(
        "bedrock_suffix:       {}",
        resp["bedrock_suffix"].as_str().unwrap_or("-")
    );
    println!(
        "anthropic_display:    {}",
        resp["anthropic_display"].as_str().unwrap_or("-")
    );
    println!(
        "would_be_created_via: {}",
        resp["would_be_created_via"].as_str().unwrap_or("-")
    );

    Ok(())
}
