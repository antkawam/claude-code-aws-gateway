use anyhow::Result;
use clap::Subcommand;

use crate::config::AdminClient;
use crate::util;

#[derive(Subcommand)]
pub enum UsersCommands {
    /// List all users
    List,

    /// Create a new user
    Create {
        /// Username or OIDC subject
        #[arg(long)]
        name: String,

        /// Role: admin or member
        #[arg(long, default_value = "member")]
        role: String,

        /// Assign to team ID
        #[arg(long)]
        team: Option<String>,
    },

    /// Update a user's role
    Update {
        /// User ID
        user_id: String,

        /// New role: admin or member
        #[arg(long)]
        role: String,
    },

    /// Delete a user
    Delete {
        /// User ID to delete
        user_id: String,
    },

    /// Assign a user to a team
    SetTeam {
        /// User ID
        user_id: String,

        /// Team ID (omit to unassign)
        #[arg(long)]
        team: Option<String>,
    },

    /// Set a per-user spend limit
    SetSpendLimit {
        /// User ID
        user_id: String,

        /// Spend limit in USD (omit to remove)
        #[arg(long)]
        limit: Option<f64>,
    },
}

pub async fn run(cmd: UsersCommands, url: Option<String>, token: Option<String>) -> Result<()> {
    let client = AdminClient::from_options(url, token).await?;

    match cmd {
        UsersCommands::List => {
            let resp = client.get("/admin/users").await?;
            if let Some(users) = resp["users"].as_array() {
                if users.is_empty() {
                    eprintln!("No users found.");
                    return Ok(());
                }
                eprintln!(
                    "{:<36}  {:<20}  {:<8}  {:<36}  CREATED",
                    "ID", "EMAIL", "ROLE", "TEAM"
                );
                eprintln!("{}", "-".repeat(110));
                for user in users {
                    println!(
                        "{:<36}  {:<20}  {:<8}  {:<36}  {}",
                        user["id"].as_str().unwrap_or("-"),
                        user["email"].as_str().unwrap_or("-"),
                        user["role"].as_str().unwrap_or("-"),
                        user["team_id"].as_str().unwrap_or("-"),
                        user["created_at"].as_str().unwrap_or("-"),
                    );
                }
            }
        }

        UsersCommands::Create { name, role, team } => {
            let mut body = serde_json::json!({
                "email": name,
                "role": role,
            });
            if let Some(team_id) = team {
                body["team_id"] = serde_json::Value::String(team_id);
            }

            let resp = client.post("/admin/users", &body).await?;
            let id = resp["id"].as_str().unwrap_or("unknown");
            util::success(&format!("User created: {name} (id: {id}, role: {role})"));
        }

        UsersCommands::Update { user_id, role } => {
            client
                .put(
                    &format!("/admin/users/{user_id}"),
                    &serde_json::json!({ "role": role }),
                )
                .await?;
            util::success(&format!("User {user_id} updated (role: {role})"));
        }

        UsersCommands::Delete { user_id } => {
            client.delete(&format!("/admin/users/{user_id}")).await?;
            util::success(&format!("User {user_id} deleted"));
        }

        UsersCommands::SetTeam { user_id, team } => {
            client
                .put(
                    &format!("/admin/users/{user_id}/team"),
                    &serde_json::json!({ "team_id": team }),
                )
                .await?;
            match team {
                Some(tid) => util::success(&format!("User {user_id} assigned to team {tid}")),
                None => util::success(&format!("User {user_id} unassigned from team")),
            }
        }

        UsersCommands::SetSpendLimit { user_id, limit } => {
            client
                .put(
                    &format!("/admin/users/{user_id}/spend-limit"),
                    &serde_json::json!({ "limit_usd": limit }),
                )
                .await?;
            match limit {
                Some(l) => util::success(&format!("User {user_id} spend limit set to ${l:.2}")),
                None => util::success(&format!("User {user_id} spend limit removed")),
            }
        }
    }
    Ok(())
}
