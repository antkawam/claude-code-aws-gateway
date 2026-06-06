// Allow `unused_variables` in test code only — test match arms like
// `other => panic!(...)` are intentional catch-alls that bind the
// unmatched value for readability without using it.
#![cfg_attr(test, allow(unused_variables))]
use clap::{Parser, Subcommand};

mod commands;
mod config;
mod util;

#[derive(Parser)]
#[command(
    name = "ccag",
    version,
    about = "Claude Code AWS Gateway - CLI operations tool"
)]
struct Cli {
    /// Gateway URL (overrides CCAG_URL env var)
    #[arg(long, global = true, env = "CCAG_URL")]
    url: Option<String>,

    /// Auth token (overrides CCAG_TOKEN env var)
    #[arg(long, global = true, env = "CCAG_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate with the gateway (saves URL + token to ~/.ccag/)
    Login {
        /// Gateway URL (saved for future commands)
        #[arg(long, env = "CCAG_URL")]
        url: Option<String>,

        /// Admin username (skips interactive prompt)
        #[arg(long, short)]
        username: Option<String>,

        /// Admin password (skips interactive prompt)
        #[arg(long, short)]
        password: Option<String>,
    },

    /// Manage runtime configuration
    #[command(subcommand)]
    Config(commands::config::ConfigCommands),

    /// Manage virtual API keys
    #[command(subcommand)]
    Keys(commands::keys::KeysCommands),

    /// Manage users
    #[command(subcommand)]
    Users(commands::users::UsersCommands),

    /// Manage teams
    #[command(subcommand)]
    Teams(commands::teams::TeamsCommands),

    /// Manage Bedrock endpoints
    #[command(subcommand)]
    Endpoints(commands::endpoints::EndpointsCommands),

    /// Manage identity providers
    #[command(subcommand)]
    Idps(commands::idps::IdpsCommands),

    /// Manage SCIM provisioning
    #[command(subcommand)]
    Scim(commands::scim::ScimCommands),

    /// Manage beta-capability overrides
    #[command(subcommand)]
    Betas(commands::betas::BetasCommands),

    /// Check deployment status and health
    Status(commands::status::StatusArgs),

    /// Tail gateway logs
    Logs(commands::logs::LogsArgs),

    /// Update ccag CLI to the latest release
    Update,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Login {
            url,
            username,
            password,
        } => {
            let url = config::resolve_url(url)?;

            let token = match (username, password) {
                (Some(u), Some(p)) => {
                    config::AdminClient::login_with_credentials(&url, &u, &p).await?
                }
                _ => config::AdminClient::interactive_login(&url).await?,
            };

            // Persist both URL and token
            config::AdminClient::save_url(&url)?;
            config::AdminClient::save_token(&token)?;

            // Verify the token works
            let client = config::AdminClient::new(&url, &token);
            let me = client.get("/auth/me").await?;
            let who = me["sub"]
                .as_str()
                .or(me["email"].as_str())
                .or(me["name"].as_str())
                .unwrap_or("unknown");
            let role = me["role"].as_str().unwrap_or("unknown");

            util::success(&format!("Logged in as {who} ({role})"));
            eprintln!("  Gateway: {url}");
            eprintln!("  Token saved to ~/.ccag/token");
            Ok(())
        }
        Commands::Config(cmd) => commands::config::run(cmd, cli.url, cli.token).await,
        Commands::Keys(cmd) => commands::keys::run(cmd, cli.url, cli.token).await,
        Commands::Users(cmd) => commands::users::run(cmd, cli.url, cli.token).await,
        Commands::Teams(cmd) => commands::teams::run(cmd, cli.url, cli.token).await,
        Commands::Endpoints(cmd) => commands::endpoints::run(cmd, cli.url, cli.token).await,
        Commands::Idps(cmd) => commands::idps::run(cmd, cli.url, cli.token).await,
        Commands::Scim(cmd) => commands::scim::run(cmd, cli.url, cli.token).await,
        Commands::Betas(cmd) => commands::betas::run(cmd, cli.url, cli.token).await,
        Commands::Status(args) => commands::status::run(args, cli.url).await,
        Commands::Logs(args) => commands::logs::run(args).await,
        Commands::Update => commands::update::run().await,
    }
}

// ---------------------------------------------------------------------------
// Layer A — CLI clap parser contract tests (T8)
//
// These tests drive the clap `derive` parser directly via `Cli::try_parse_from`
// with no I/O or network calls.  They will FAIL TO COMPILE until the Builder
// creates `cli/src/commands/betas.rs` and registers it in `Commands` (see
// task 8 contract in `.claude/specs/1m-context-support-tasks.md`).
//
// Contract the Builder must satisfy:
//   1. Add `pub mod betas;` to `cli/src/commands/mod.rs`.
//   2. Add a `Betas` variant to `Commands` in this file:
//        /// Manage beta-capability overrides
//        #[command(subcommand)]
//        Betas(commands::betas::BetasCommands),
//   3. Add handler arm in `main()`:
//        Commands::Betas(cmd) => commands::betas::run(cmd, cli.url, cli.token).await,
//   4. `BetasCommands` must expose:
//        - `ListOverrides`                     (no args)
//        - `Override { endpoint_id: Uuid, profile_id: String, beta_name: String, supported: bool, reason: Option<String> }`
//        - `ClearOverride { endpoint_id: Uuid, profile_id: String, beta_name: String }`
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests_cli_betas {
    use super::{Cli, Commands};
    use clap::Parser;
    // `commands` is a private module declared at crate root — accessible via `crate::`.
    // These imports will fail to compile until the Builder adds `betas.rs` and registers it.
    #[allow(unused_imports)]
    use crate::commands::betas::BetasCommands;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    /// Test 6 — `ccag betas list-overrides` parses to `Commands::Betas(BetasCommands::ListOverrides)`.
    ///
    /// Fails to compile until Builder adds `Commands::Betas` + `BetasCommands`.
    #[test]
    fn list_overrides_parses() {
        let cli = parse(&["ccag", "betas", "list-overrides"])
            .expect("list-overrides should parse without error");
        assert!(
            matches!(cli.command, Commands::Betas(BetasCommands::ListOverrides)),
            "expected Commands::Betas(BetasCommands::ListOverrides)"
        );
    }

    /// Test 7 — `ccag betas override <uuid> <profile> <beta> true --reason "manual"`.
    /// Parses; reason is `Some("manual")`.
    ///
    /// Fails to compile until Builder adds `Commands::Betas` + `BetasCommands`.
    #[test]
    fn override_parses_with_reason() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let cli = parse(&[
            "ccag",
            "betas",
            "override",
            uuid,
            "us.anthropic.claude-opus-4-7",
            "context-1m-2025-08-07",
            "true",
            "--reason",
            "manual",
        ])
        .expect("override with --reason should parse successfully");

        match cli.command {
            Commands::Betas(BetasCommands::Override {
                endpoint_id,
                profile_id,
                beta_name,
                supported,
                reason,
            }) => {
                assert_eq!(endpoint_id.to_string(), uuid, "endpoint_id mismatch");
                assert_eq!(profile_id, "us.anthropic.claude-opus-4-7");
                assert_eq!(beta_name, "context-1m-2025-08-07");
                assert!(supported, "supported must be true");
                assert_eq!(reason, Some("manual".to_string()));
            }
            other => panic!("expected Betas(Override), got a different variant"),
        }
    }

    /// Test 8 — `ccag betas override <uuid> <profile> <beta> false` (no --reason).
    /// Parses; reason is `None`.
    ///
    /// Fails to compile until Builder adds `Commands::Betas` + `BetasCommands`.
    #[test]
    fn override_parses_without_reason() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let cli = parse(&[
            "ccag",
            "betas",
            "override",
            uuid,
            "us.anthropic.claude-haiku-4-5",
            "context-1m-2025-08-07",
            "false",
        ])
        .expect("override without --reason should parse successfully");

        match cli.command {
            Commands::Betas(BetasCommands::Override {
                supported, reason, ..
            }) => {
                assert!(!supported, "supported must be false");
                assert!(reason.is_none(), "reason must be None when omitted");
            }
            _ => panic!("expected Betas(Override) variant"),
        }
    }

    /// Test 9 — a non-boolean `supported` value causes a clap parse error (exit 2).
    ///
    /// Fails to compile until Builder adds `Commands::Betas` + `BetasCommands`.
    #[test]
    fn override_rejects_invalid_bool() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let result = parse(&[
            "ccag",
            "betas",
            "override",
            uuid,
            "us.anthropic.claude-opus-4-7",
            "context-1m-2025-08-07",
            "not-a-bool",
        ]);
        assert!(
            result.is_err(),
            "a non-bool 'supported' value must produce a clap parse error"
        );
    }

    /// Test 10 — a non-UUID `endpoint_id` causes a clap parse error (exit 2).
    ///
    /// Fails to compile until Builder adds `Commands::Betas` + `BetasCommands`.
    #[test]
    fn override_rejects_invalid_uuid() {
        let result = parse(&[
            "ccag",
            "betas",
            "override",
            "not-a-uuid",
            "us.anthropic.claude-opus-4-7",
            "context-1m-2025-08-07",
            "true",
        ]);
        assert!(
            result.is_err(),
            "a non-UUID endpoint_id must produce a clap parse error"
        );
    }

    /// Test 11 — `ccag betas clear-override <uuid> <profile> <beta>` parses to
    /// `Commands::Betas(BetasCommands::ClearOverride)` with all three fields.
    ///
    /// Fails to compile until Builder adds `Commands::Betas` + `BetasCommands`.
    #[test]
    fn clear_override_parses() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let cli = parse(&[
            "ccag",
            "betas",
            "clear-override",
            uuid,
            "us.anthropic.claude-opus-4-7",
            "context-1m-2025-08-07",
        ])
        .expect("clear-override should parse successfully");

        match cli.command {
            Commands::Betas(BetasCommands::ClearOverride {
                endpoint_id,
                profile_id,
                beta_name,
            }) => {
                assert_eq!(endpoint_id.to_string(), uuid);
                assert_eq!(profile_id, "us.anthropic.claude-opus-4-7");
                assert_eq!(beta_name, "context-1m-2025-08-07");
            }
            _ => panic!("expected Betas(ClearOverride) variant"),
        }
    }
}
