//! `ccag mcp` — MCP / tool governance from the command line.
//!
//! Every subcommand talks to the `/admin/mcpgov/*` API, so scripting and GitOps use
//! exactly the same surface the portal does.

use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum McpCommands {
    /// Show the current governance posture and catalog counts
    Status,

    /// List discovered MCP servers
    Servers,

    /// List discovered tools
    Tools {
        /// Filter by status: inherit, approved, denied
        #[arg(long)]
        status: Option<String>,

        /// Filter by MCP server name
        #[arg(long)]
        server: Option<String>,

        /// Maximum rows to return
        #[arg(long, default_value = "200")]
        limit: i64,
    },

    /// Approve, deny, or reset an MCP server
    SetServer {
        /// Server name (the `<server>` in `mcp__<server>__<tool>`)
        server: String,

        /// New status: approved, denied, pending
        #[arg(long)]
        status: String,

        /// Optional operator note
        #[arg(long)]
        notes: Option<String>,
    },

    /// Add a server to the catalog before it has been seen in traffic
    AddServer {
        /// Server name
        server: String,

        /// Initial status
        #[arg(long, default_value = "approved")]
        status: String,

        /// Optional operator note
        #[arg(long)]
        notes: Option<String>,
    },

    /// Approve, deny, or reset a single tool
    SetTool {
        /// Full tool name, e.g. mcp__github__create_issue
        tool: String,

        /// New status: approved, denied, inherit
        #[arg(long)]
        status: String,
    },

    /// Set the same status on every tool of one server
    BulkSetTools {
        /// Server name
        server: String,

        /// New status: approved, denied, inherit
        #[arg(long)]
        status: String,
    },

    /// List policies
    Policies,

    /// Create or update a policy (upsert, keyed on scope + reference)
    SetPolicy {
        /// Scope: global, team, user
        #[arg(long, default_value = "global")]
        scope: String,

        /// Team UUID or user email. Required unless scope is global.
        #[arg(long = "ref")]
        scope_ref: Option<String>,

        /// Mode: observe, warn, enforce
        #[arg(long, default_value = "observe")]
        mode: String,

        /// What to do with tools that are not in the catalog: allow, deny
        #[arg(long, default_value = "allow")]
        default_action: String,

        /// Which tools the policy governs: mcp_only, all_tools
        #[arg(long, default_value = "mcp_only")]
        applies_to: String,

        /// Allow glob, repeatable. Non-empty makes this an exclusive allowlist.
        #[arg(long = "allow")]
        allow: Vec<String>,

        /// Deny glob, repeatable. Deny always wins.
        #[arg(long = "deny")]
        deny: Vec<String>,

        /// Create the policy disabled
        #[arg(long)]
        disabled: bool,
    },

    /// Delete a team or user policy (the global policy cannot be deleted)
    DeletePolicy {
        /// Policy UUID
        id: String,
    },

    /// Show what a given identity would actually be allowed to use
    Simulate {
        /// User identity (email)
        #[arg(long)]
        user: Option<String>,

        /// Team UUID
        #[arg(long)]
        team: Option<String>,

        /// Tool name to test, repeatable. Omit to test the whole catalog.
        #[arg(long = "tool")]
        tools: Vec<String>,

        /// Only print denied tools
        #[arg(long)]
        denied_only: bool,
    },

    /// Show recent policy decisions
    Events {
        /// Filter by decision: blocked, would_block, warned
        #[arg(long)]
        decision: Option<String>,

        /// Filter by user identity
        #[arg(long)]
        user: Option<String>,

        /// Maximum rows to return
        #[arg(long, default_value = "50")]
        limit: i64,
    },
}

const SERVER_STATUSES: [&str; 3] = ["approved", "denied", "pending"];
const TOOL_STATUSES: [&str; 3] = ["approved", "denied", "inherit"];

fn validate(value: &str, allowed: &[&str], what: &str) -> Result<()> {
    if !allowed.contains(&value) {
        anyhow::bail!(
            "invalid {what} `{value}` — expected one of: {}",
            allowed.join(", ")
        );
    }
    Ok(())
}

fn s(v: &serde_json::Value) -> &str {
    v.as_str().unwrap_or("-")
}

fn n(v: &serde_json::Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

pub async fn run(cmd: McpCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        McpCommands::Status => {
            let resp = client.get("/admin/mcpgov/summary").await?;
            let sum = &resp["summary"];
            let gp = &resp["global_policy"];

            eprintln!("Posture");
            eprintln!("  mode              {}", s(&gp["mode"]));
            eprintln!("  unknown tools     {}", s(&gp["default_action"]));
            eprintln!("  applies to        {}", s(&gp["applies_to"]));
            eprintln!(
                "  patterns          {} allow, {} deny",
                gp["allow_patterns"].as_array().map_or(0, |a| a.len()),
                gp["deny_patterns"].as_array().map_or(0, |a| a.len()),
            );
            eprintln!();
            eprintln!("Catalog");
            eprintln!(
                "  servers           {} total ({} pending, {} approved, {} denied)",
                n(&sum["servers_total"]),
                n(&sum["servers_pending"]),
                n(&sum["servers_approved"]),
                n(&sum["servers_denied"]),
            );
            eprintln!(
                "  tools             {} total ({} undecided, {} approved, {} denied)",
                n(&sum["tools_total"]),
                n(&sum["tools_pending"]),
                n(&sum["tools_approved"]),
                n(&sum["tools_denied"]),
            );
            eprintln!();
            eprintln!("Last 24h");
            eprintln!("  blocked           {}", n(&sum["events_24h_blocked"]));
            eprintln!("  would block       {}", n(&sum["events_24h_would_block"]));
            eprintln!("  warned            {}", n(&sum["events_24h_warned"]));

            let drops = &resp["buffer_drops"];
            if n(&drops["discovery"]) > 0 || n(&drops["events"]) > 0 {
                util::warn(&format!(
                    "write buffers dropped {} discovery and {} audit items under load",
                    n(&drops["discovery"]),
                    n(&drops["events"]),
                ));
            }

            if s(&gp["mode"]) == "observe" {
                util::info("observe mode: nothing is being stripped yet");
            }
        }

        McpCommands::Servers => {
            let resp = client.get("/admin/mcpgov/servers").await?;
            let rows = resp["servers"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                eprintln!("No MCP servers discovered yet.");
                return Ok(());
            }
            eprintln!("{:<30}  {:<10}  LAST SEEN", "SERVER", "STATUS");
            eprintln!("{}", "-".repeat(70));
            for r in &rows {
                println!(
                    "{:<30}  {:<10}  {}",
                    s(&r["server_name"]),
                    s(&r["status"]),
                    s(&r["last_seen"]),
                );
            }
        }

        McpCommands::Tools {
            status,
            server,
            limit,
        } => {
            let mut path = format!("/admin/mcpgov/tools?limit={limit}");
            if let Some(st) = &status {
                validate(st, &TOOL_STATUSES, "status")?;
                path.push_str(&format!("&status={st}"));
            }
            if let Some(sv) = &server {
                path.push_str(&format!("&server={sv}"));
            }

            let resp = client.get(&path).await?;
            let rows = resp["tools"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                eprintln!("No tools matched.");
                return Ok(());
            }
            eprintln!("{:<50}  {:<10}  {:>6}  SERVER", "TOOL", "STATUS", "SEEN");
            eprintln!("{}", "-".repeat(90));
            for r in &rows {
                println!(
                    "{:<50}  {:<10}  {:>6}  {}",
                    s(&r["tool_name"]),
                    s(&r["status"]),
                    n(&r["seen_count"]),
                    r["server_name"].as_str().unwrap_or("(builtin)"),
                );
            }
        }

        McpCommands::SetServer {
            server,
            status,
            notes,
        } => {
            validate(&status, &SERVER_STATUSES, "status")?;
            let mut body = serde_json::json!({ "server_name": server, "status": status });
            if let Some(nt) = notes {
                body["notes"] = serde_json::json!(nt);
            }
            client.put("/admin/mcpgov/servers/status", &body).await?;
            util::success(&format!("server {server} set to {status}"));
        }

        McpCommands::AddServer {
            server,
            status,
            notes,
        } => {
            validate(&status, &SERVER_STATUSES, "status")?;
            let mut body = serde_json::json!({ "server_name": server, "status": status });
            if let Some(nt) = notes {
                body["notes"] = serde_json::json!(nt);
            }
            client.post("/admin/mcpgov/servers", &body).await?;
            util::success(&format!("server {server} added as {status}"));
        }

        McpCommands::SetTool { tool, status } => {
            validate(&status, &TOOL_STATUSES, "status")?;
            client
                .put(
                    "/admin/mcpgov/tools/status",
                    &serde_json::json!({ "tool_name": tool, "status": status }),
                )
                .await?;
            util::success(&format!("tool {tool} set to {status}"));
        }

        McpCommands::BulkSetTools { server, status } => {
            validate(&status, &TOOL_STATUSES, "status")?;
            let resp = client
                .put(
                    "/admin/mcpgov/servers/bulk-status",
                    &serde_json::json!({ "server_name": server, "status": status }),
                )
                .await?;
            util::success(&format!(
                "{} tools of {server} set to {status}",
                n(&resp["updated"])
            ));
        }

        McpCommands::Policies => {
            let resp = client.get("/admin/mcpgov/policies").await?;
            let rows = resp["policies"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                eprintln!("No policies defined.");
                return Ok(());
            }
            for r in &rows {
                let scope = s(&r["scope"]);
                let label = if scope == "global" {
                    "global".to_string()
                } else {
                    format!("{scope}:{}", s(&r["scope_ref"]))
                };
                println!(
                    "{}\n  id            {}\n  mode          {}\n  unknown tools {}\n  applies to    {}\n  enabled       {}",
                    label,
                    s(&r["id"]),
                    s(&r["mode"]),
                    s(&r["default_action"]),
                    s(&r["applies_to"]),
                    r["enabled"].as_bool().unwrap_or(false),
                );
                for (field, title) in [("allow_patterns", "allow"), ("deny_patterns", "deny")] {
                    if let Some(list) = r[field].as_array()
                        && !list.is_empty()
                    {
                        let joined: Vec<&str> = list.iter().map(s).collect();
                        println!("  {title:<13} {}", joined.join(", "));
                    }
                }
                println!();
            }
        }

        McpCommands::SetPolicy {
            scope,
            scope_ref,
            mode,
            default_action,
            applies_to,
            allow,
            deny,
            disabled,
        } => {
            validate(&scope, &["global", "team", "user"], "scope")?;
            validate(&mode, &["observe", "warn", "enforce"], "mode")?;
            validate(&default_action, &["allow", "deny"], "default action")?;
            validate(&applies_to, &["mcp_only", "all_tools"], "applies-to")?;

            if scope != "global" && scope_ref.as_deref().unwrap_or("").trim().is_empty() {
                anyhow::bail!("--ref is required when --scope is `{scope}`");
            }

            // Guard the one combination that silently disables every built-in tool.
            if mode == "enforce"
                && applies_to == "all_tools"
                && default_action == "deny"
                && allow.is_empty()
            {
                anyhow::bail!(
                    "refusing to enforce all_tools with deny-by-default and no --allow patterns: \
                     this would strip every built-in tool (Read, Write, Bash, ...). \
                     Add --allow patterns, or use --applies-to mcp_only."
                );
            }

            let body = serde_json::json!({
                "scope": scope,
                "scope_ref": scope_ref,
                "mode": mode,
                "default_action": default_action,
                "applies_to": applies_to,
                "allow_patterns": allow,
                "deny_patterns": deny,
                "enabled": !disabled,
            });

            client.put("/admin/mcpgov/policies", &body).await?;
            util::success(&format!("policy saved for scope {scope}"));
            if mode == "enforce" {
                util::info("enforce mode is active: denied tool definitions will be stripped");
            }
        }

        McpCommands::DeletePolicy { id } => {
            client
                .delete(&format!("/admin/mcpgov/policies/{id}"))
                .await?;
            util::success(&format!("policy {id} deleted"));
        }

        McpCommands::Simulate {
            user,
            team,
            tools,
            denied_only,
        } => {
            let mut body = serde_json::json!({ "tool_names": tools });
            if let Some(u) = user {
                body["user_identity"] = serde_json::json!(u);
            }
            if let Some(t) = team {
                body["team_id"] = serde_json::json!(t);
            }

            let resp = client.post("/admin/mcpgov/simulate", &body).await?;
            let eff = &resp["effective_policy"];
            let counts = &resp["counts"];

            eprintln!(
                "Effective policy: mode={} unknown={} applies_to={} (from {})",
                s(&eff["mode"]),
                s(&eff["default_action"]),
                s(&eff["applies_to"]),
                s(&eff["decided_by"]),
            );
            eprintln!(
                "{} allowed, {} denied",
                n(&counts["allowed"]),
                n(&counts["denied"])
            );
            eprintln!();

            let rows = resp["results"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                eprintln!("Nothing to evaluate — the catalog is empty and no --tool was given.");
                return Ok(());
            }

            eprintln!("{:<50}  {:<8}  WHY", "TOOL", "VERDICT");
            eprintln!("{}", "-".repeat(100));
            for r in &rows {
                let allowed = r["allowed"].as_bool().unwrap_or(false);
                if denied_only && allowed {
                    continue;
                }
                println!(
                    "{:<50}  {:<8}  {}",
                    s(&r["tool_name"]),
                    if allowed { "allow" } else { "DENY" },
                    s(&r["reason"]),
                );
            }
        }

        McpCommands::Events {
            decision,
            user,
            limit,
        } => {
            let mut path = format!("/admin/mcpgov/events?limit={limit}");
            if let Some(d) = &decision {
                validate(d, &["blocked", "would_block", "warned"], "decision")?;
                path.push_str(&format!("&decision={d}"));
            }
            if let Some(u) = &user {
                path.push_str(&format!("&user={u}"));
            }

            let resp = client.get(&path).await?;
            let rows = resp["events"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                eprintln!("No policy decisions recorded.");
                return Ok(());
            }
            eprintln!("{:<26}  {:<12}  {:<40}  USER", "WHEN", "DECISION", "TOOL");
            eprintln!("{}", "-".repeat(110));
            for r in &rows {
                println!(
                    "{:<26}  {:<12}  {:<40}  {}",
                    s(&r["created_at"]),
                    s(&r["decision"]),
                    s(&r["tool_name"]),
                    r["user_identity"].as_str().unwrap_or("-"),
                );
            }
        }
    }

    Ok(())
}
