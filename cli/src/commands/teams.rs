use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum TeamsCommands {
    /// Create a new team
    Create {
        /// Team name
        #[arg(long)]
        name: String,
    },

    /// List all teams
    List,

    /// Delete a team
    Delete {
        /// Team ID to delete
        team_id: String,
    },

    /// Set budget for a team
    SetBudget {
        /// Team ID
        team_id: String,

        /// Budget amount in USD
        #[arg(long)]
        amount: f64,

        /// Budget period
        #[arg(long, default_value = "monthly")]
        period: String,

        /// Budget policy
        #[arg(long, default_value = "standard")]
        policy: String,

        /// Default per-user budget in USD
        #[arg(long)]
        user_budget: Option<f64>,
    },

    /// Show team analytics (users, spend)
    Analytics {
        /// Team ID
        team_id: String,
    },

    /// Show team endpoint routing
    Endpoints {
        /// Team ID
        team_id: String,
    },

    /// Set team endpoint routing
    SetEndpoints {
        /// Team ID
        team_id: String,

        /// Endpoint assignments as id:priority (repeatable)
        #[arg(long = "endpoint")]
        endpoints: Vec<String>,

        /// Routing strategy
        #[arg(long)]
        strategy: Option<String>,
    },

    /// List team members
    Members {
        /// Team ID
        team_id: String,
    },

    /// Add a member to a team
    AddMember {
        /// Team ID or name
        #[arg(long)]
        team: String,

        /// User email or ID
        #[arg(long)]
        user: String,
    },

    /// Remove a member from a team
    RemoveMember {
        /// Team ID or name
        #[arg(long)]
        team: String,

        /// User email or ID
        #[arg(long)]
        user: String,
    },
}

pub async fn run(cmd: TeamsCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        TeamsCommands::Create { name } => {
            let resp = client
                .post("/admin/teams", &serde_json::json!({ "name": name }))
                .await?;
            let id = resp["id"].as_str().unwrap_or("-");
            let name = resp["name"].as_str().unwrap_or("-");
            util::success(&format!("Team created: {name} (id: {id})"));
        }

        TeamsCommands::List => {
            let resp = client.get("/admin/teams").await?;
            if let Some(teams) = resp["teams"].as_array() {
                if teams.is_empty() {
                    eprintln!("No teams found.");
                    return Ok(());
                }
                eprintln!("{:<36}  {:<20}  CREATED", "ID", "NAME");
                eprintln!("{}", "-".repeat(76));
                for team in teams {
                    println!(
                        "{:<36}  {:<20}  {}",
                        team["id"].as_str().unwrap_or("-"),
                        team["name"].as_str().unwrap_or("-"),
                        team["created_at"].as_str().unwrap_or("-"),
                    );
                }
            }
        }

        TeamsCommands::Delete { team_id } => {
            client.delete(&format!("/admin/teams/{team_id}")).await?;
            util::success(&format!("Team {team_id} deleted"));
        }

        TeamsCommands::SetBudget {
            team_id,
            amount,
            period,
            policy,
            user_budget,
        } => {
            let mut body = serde_json::json!({
                "budget_amount_usd": amount,
                "budget_period": period,
                "budget_policy": policy,
            });
            if let Some(ub) = user_budget {
                body["default_user_budget_usd"] = serde_json::json!(ub);
            }
            client
                .put(&format!("/admin/teams/{team_id}/budget"), &body)
                .await?;
            util::success(&format!("Budget set for team {team_id}"));
        }

        TeamsCommands::Analytics { team_id } => {
            let resp = client
                .get(&format!("/admin/teams/{team_id}/analytics"))
                .await?;
            let team_name = resp["team_name"].as_str().unwrap_or("-");
            eprintln!("Team: {team_name}");
            eprintln!();
            if let Some(users) = resp["users"].as_array() {
                if users.is_empty() {
                    eprintln!("No users in this team.");
                    return Ok(());
                }
                eprintln!(
                    "{:<30}  {:<12}  {:<14}  REQUESTS",
                    "EMAIL", "SPEND_LIMIT", "CURRENT_SPEND"
                );
                eprintln!("{}", "-".repeat(76));
                for user in users {
                    println!(
                        "{:<30}  {:<12}  {:<14}  {}",
                        user["email"].as_str().unwrap_or("-"),
                        user["spend_limit"]
                            .as_f64()
                            .map(|v| format!("${v:.2}"))
                            .unwrap_or_else(|| "-".to_string()),
                        user["current_spend"]
                            .as_f64()
                            .map(|v| format!("${v:.2}"))
                            .unwrap_or_else(|| "-".to_string()),
                        user["requests"]
                            .as_i64()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                }
            }
        }

        TeamsCommands::Endpoints { team_id } => {
            let resp = client
                .get(&format!("/admin/teams/{team_id}/endpoints"))
                .await?;
            let strategy = resp["routing_strategy"].as_str().unwrap_or("-");
            eprintln!("Routing strategy: {strategy}");
            eprintln!();
            if let Some(endpoints) = resp["endpoints"].as_array() {
                if endpoints.is_empty() {
                    eprintln!("No endpoints assigned.");
                    return Ok(());
                }
                eprintln!("{:<36}  {:<20}  PRIORITY", "ID", "NAME");
                eprintln!("{}", "-".repeat(66));
                for ep in endpoints {
                    println!(
                        "{:<36}  {:<20}  {}",
                        ep["id"].as_str().unwrap_or("-"),
                        ep["name"].as_str().unwrap_or("-"),
                        ep["priority"]
                            .as_i64()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                }
            }
        }

        TeamsCommands::SetEndpoints {
            team_id,
            endpoints,
            strategy,
        } => {
            let parsed: Vec<serde_json::Value> = endpoints
                .iter()
                .map(|e| {
                    let parts: Vec<&str> = e.splitn(2, ':').collect();
                    serde_json::json!({
                        "endpoint_id": parts[0],
                        "priority": parts.get(1).and_then(|p| p.parse::<i64>().ok()).unwrap_or(0),
                    })
                })
                .collect();
            let mut body = serde_json::json!({ "endpoints": parsed });
            if let Some(s) = strategy {
                body["routing_strategy"] = serde_json::Value::String(s);
            }
            client
                .put(&format!("/admin/teams/{team_id}/endpoints"), &body)
                .await?;
            util::success(&format!("Endpoints updated for team {team_id}"));
        }

        TeamsCommands::Members { team_id } => {
            let resp = client
                .get(&format!("/admin/teams/{team_id}/members"))
                .await?;
            if let Some(members) = resp["members"].as_array() {
                if members.is_empty() {
                    eprintln!("No members in this team.");
                    return Ok(());
                }
                eprintln!("{:<36}  {:<30}  {:<10}  ACTIVE", "ID", "EMAIL", "ROLE");
                eprintln!("{}", "-".repeat(86));
                for m in members {
                    println!(
                        "{:<36}  {:<30}  {:<10}  {}",
                        m["id"].as_str().unwrap_or("-"),
                        m["email"].as_str().unwrap_or("-"),
                        m["role"].as_str().unwrap_or("-"),
                        if m["active"].as_bool().unwrap_or(true) {
                            "yes"
                        } else {
                            "no"
                        },
                    );
                }
            }
        }

        TeamsCommands::AddMember { team, user } => {
            // Resolve user: if it looks like a UUID, use as user_id directly.
            // Otherwise, list users and find by email.
            let user_id = resolve_user_id(&client, &user).await?;
            let resp = client
                .post(
                    &format!("/admin/teams/{team}/members"),
                    &serde_json::json!({ "user_id": user_id }),
                )
                .await?;
            if resp.get("added").is_some() {
                util::success(&format!("User {user} added to team {team}"));
            } else {
                let msg = resp["error"]["message"]
                    .as_str()
                    .unwrap_or("Failed to add member");
                util::error(msg);
            }
        }

        TeamsCommands::RemoveMember { team, user } => {
            let user_id = resolve_user_id(&client, &user).await?;
            client
                .delete(&format!("/admin/teams/{team}/members/{user_id}"))
                .await?;
            util::success(&format!("User {user} removed from team {team}"));
        }
    }
    Ok(())
}

/// Resolve a user identifier (UUID or email) to a UUID string.
async fn resolve_user_id(client: &AdminClient, user: &str) -> anyhow::Result<String> {
    if uuid::Uuid::parse_str(user).is_ok() {
        return Ok(user.to_string());
    }

    let resp = client.get("/admin/users").await?;
    if let Some(users) = resp["users"].as_array() {
        for u in users {
            if u["email"].as_str() == Some(user)
                && let Some(id) = u["id"].as_str()
            {
                return Ok(id.to_string());
            }
        }
    }
    anyhow::bail!("User not found: {user}")
}
