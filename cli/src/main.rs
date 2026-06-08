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

    /// Manage model ID mappings
    #[command(subcommand)]
    Mappings(commands::mappings::MappingsCommands),

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
        Commands::Mappings(cmd) => commands::mappings::run(cmd, cli.url, cli.token).await,
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

// ---------------------------------------------------------------------------
// Layer A — CLI clap parser contract tests (Task 8: ccag mappings)
//
// These tests drive the clap `derive` parser directly via `Cli::try_parse_from`
// with no I/O or network calls.  They will FAIL TO COMPILE until the Builder
// implements the `mappings` subcommand.
//
// BUILDER CONTRACT — everything the Builder must satisfy for these tests to
// pass:
//
//   Module:  `cli/src/commands/mappings.rs`
//
//   1. Add `pub mod mappings;` to `cli/src/commands/mod.rs`.
//
//   2. Add a `Mappings` variant to `Commands` in `cli/src/main.rs`:
//        /// Manage model ID mappings
//        #[command(subcommand)]
//        Mappings(commands::mappings::MappingsCommands),
//
//   3. Add handler arm in `main()`:
//        Commands::Mappings(cmd) => commands::mappings::run(cmd, cli.url, cli.token).await,
//
//   4. `MappingsCommands` must expose these variants (clap subcommands):
//
//        List {
//            /// Output raw JSON instead of a table
//            #[arg(long)]
//            json: bool,
//        }
//
//        Add {
//            /// Short Anthropic-side prefix, e.g. "claude-sonnet-4-6"
//            anthropic_prefix: String,
//            /// Bedrock model suffix, must start with "anthropic."
//            bedrock_suffix: String,
//            /// Human-readable display name (optional)
//            #[arg(long)]
//            display: Option<String>,
//        }
//
//        Delete {
//            /// Anthropic prefix to delete
//            anthropic_prefix: String,
//            /// Skip the interactive confirmation prompt
//            #[arg(long)]
//            yes: bool,
//        }
//
//        Discover {
//            /// Raw model ID to probe, e.g. "claude-future-9-9"
//            model: String,
//        }
//
//   5. `run(cmd: MappingsCommands, url: Option<String>, token: Option<String>)`
//      calls the following admin API endpoints via `AdminClient`:
//        - List   → GET  /admin/mappings
//        - Add    → POST /admin/mappings          body {anthropic_prefix, bedrock_suffix, anthropic_display?}
//        - Delete → DELETE /admin/mappings/{anthropic_prefix}
//        - Discover → POST /admin/mappings/discover  body {model}
//
//   6. `List` with `--json` prints the raw JSON response to stdout (same
//      pattern as existing `list` commands that pass `--json`).
//      Without `--json`, prints a table with columns:
//        ANTHROPIC_PREFIX  BEDROCK_SUFFIX  DISPLAY  SOURCE  CREATED_VIA  LAST_USED_AT  CREATED_AT
//
//   7. `Delete` without `--yes` shows a confirmation prompt via `dialoguer`;
//      with `--yes` proceeds immediately.
//
//   8. On server error (non-2xx), `run` returns Err(anyhow) — the binary
//      prints the error to stderr and exits non-zero (standard anyhow main).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests_cli_mappings {
    use super::{Cli, Commands};
    use clap::Parser;
    // These imports will fail to compile until the Builder adds `mappings.rs`
    // and registers the `Mappings` variant in `Commands`.
    #[allow(unused_imports)]
    use crate::commands::mappings::MappingsCommands;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // -----------------------------------------------------------------------
    // list
    // -----------------------------------------------------------------------

    /// `ccag mappings list` — no flags — parses to `MappingsCommands::List` with
    /// `json = false`.
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn list_parses_no_flags() {
        let cli = parse(&["ccag", "mappings", "list"])
            .expect("mappings list should parse without error");
        match cli.command {
            Commands::Mappings(MappingsCommands::List { json }) => {
                assert!(!json, "json flag must default to false");
            }
            _ => panic!("expected Commands::Mappings(MappingsCommands::List)"),
        }
    }

    /// `ccag mappings list --json` — parses to `MappingsCommands::List` with
    /// `json = true`.
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn list_parses_json_flag() {
        let cli = parse(&["ccag", "mappings", "list", "--json"])
            .expect("mappings list --json should parse without error");
        match cli.command {
            Commands::Mappings(MappingsCommands::List { json }) => {
                assert!(json, "json flag must be true when --json is present");
            }
            _ => panic!("expected Commands::Mappings(MappingsCommands::List)"),
        }
    }

    // -----------------------------------------------------------------------
    // add
    // -----------------------------------------------------------------------

    /// `ccag mappings add <prefix> <suffix>` — no optional display — parses
    /// with `display = None`.
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn add_parses_required_args_only() {
        let cli = parse(&[
            "ccag",
            "mappings",
            "add",
            "claude-sonnet-4-6",
            "anthropic.claude-sonnet-4-6-v1",
        ])
        .expect("mappings add with two positionals should parse without error");
        match cli.command {
            Commands::Mappings(MappingsCommands::Add {
                anthropic_prefix,
                bedrock_suffix,
                display,
            }) => {
                assert_eq!(anthropic_prefix, "claude-sonnet-4-6");
                assert_eq!(bedrock_suffix, "anthropic.claude-sonnet-4-6-v1");
                assert!(
                    display.is_none(),
                    "display must be None when --display is omitted"
                );
            }
            _ => panic!("expected Commands::Mappings(MappingsCommands::Add)"),
        }
    }

    /// `ccag mappings add <prefix> <suffix> --display "Sonnet 4.6"` — parses
    /// with `display = Some("Sonnet 4.6")`.
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn add_parses_with_display_flag() {
        let cli = parse(&[
            "ccag",
            "mappings",
            "add",
            "claude-sonnet-4-6",
            "anthropic.claude-sonnet-4-6-v1",
            "--display",
            "Sonnet 4.6",
        ])
        .expect("mappings add with --display should parse without error");
        match cli.command {
            Commands::Mappings(MappingsCommands::Add { display, .. }) => {
                assert_eq!(
                    display,
                    Some("Sonnet 4.6".to_string()),
                    "display must capture the --display value"
                );
            }
            _ => panic!("expected Commands::Mappings(MappingsCommands::Add)"),
        }
    }

    /// `ccag mappings add` with only one positional argument must fail (missing
    /// `bedrock_suffix`).
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn add_rejects_missing_bedrock_suffix() {
        let result = parse(&["ccag", "mappings", "add", "claude-sonnet-4-6"]);
        assert!(
            result.is_err(),
            "add with missing bedrock_suffix must produce a clap parse error"
        );
    }

    /// `ccag mappings add` with no positional arguments must fail (both required).
    #[test]
    fn add_rejects_no_args() {
        let result = parse(&["ccag", "mappings", "add"]);
        assert!(
            result.is_err(),
            "add with no args must produce a clap parse error"
        );
    }

    // -----------------------------------------------------------------------
    // delete
    // -----------------------------------------------------------------------

    /// `ccag mappings delete <prefix>` — no flags — parses with `yes = false`.
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn delete_parses_without_yes_flag() {
        let cli = parse(&["ccag", "mappings", "delete", "claude-sonnet-4-6"])
            .expect("mappings delete with one positional should parse without error");
        match cli.command {
            Commands::Mappings(MappingsCommands::Delete {
                anthropic_prefix,
                yes,
            }) => {
                assert_eq!(anthropic_prefix, "claude-sonnet-4-6");
                assert!(!yes, "yes flag must default to false");
            }
            _ => panic!("expected Commands::Mappings(MappingsCommands::Delete)"),
        }
    }

    /// `ccag mappings delete <prefix> --yes` — parses with `yes = true` (skips
    /// confirmation prompt in production code).
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn delete_parses_with_yes_flag() {
        let cli = parse(&["ccag", "mappings", "delete", "claude-sonnet-4-6", "--yes"])
            .expect("mappings delete --yes should parse without error");
        match cli.command {
            Commands::Mappings(MappingsCommands::Delete { yes, .. }) => {
                assert!(yes, "yes flag must be true when --yes is present");
            }
            _ => panic!("expected Commands::Mappings(MappingsCommands::Delete)"),
        }
    }

    /// `ccag mappings delete` with no positional argument must fail.
    #[test]
    fn delete_rejects_no_args() {
        let result = parse(&["ccag", "mappings", "delete"]);
        assert!(
            result.is_err(),
            "delete with no positional must produce a clap parse error"
        );
    }

    // -----------------------------------------------------------------------
    // discover
    // -----------------------------------------------------------------------

    /// `ccag mappings discover <model>` — parses with the model ID captured.
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn discover_parses_model_arg() {
        let cli = parse(&["ccag", "mappings", "discover", "claude-future-9-9"])
            .expect("mappings discover should parse without error");
        match cli.command {
            Commands::Mappings(MappingsCommands::Discover { model }) => {
                assert_eq!(model, "claude-future-9-9");
            }
            _ => panic!("expected Commands::Mappings(MappingsCommands::Discover)"),
        }
    }

    /// `ccag mappings discover` with no positional argument must fail.
    #[test]
    fn discover_rejects_no_args() {
        let result = parse(&["ccag", "mappings", "discover"]);
        assert!(
            result.is_err(),
            "discover with no model arg must produce a clap parse error"
        );
    }

    // -----------------------------------------------------------------------
    // global flags pass through
    // -----------------------------------------------------------------------

    /// Global `--url` and `--token` flags propagate to the subcommand parse
    /// (they are stored on `Cli`, not `MappingsCommands`).
    ///
    /// Compile-fails until Builder adds `Commands::Mappings` + `MappingsCommands`.
    #[test]
    fn global_flags_parse_with_mappings_subcommand() {
        let cli = parse(&[
            "ccag",
            "--url",
            "http://localhost:9090",
            "--token",
            "test-tok",
            "mappings",
            "list",
        ])
        .expect("global flags + mappings list should parse without error");
        assert_eq!(cli.url, Some("http://localhost:9090".to_string()));
        assert_eq!(cli.token, Some("test-tok".to_string()));
        assert!(
            matches!(cli.command, Commands::Mappings(MappingsCommands::List { .. })),
            "expected Commands::Mappings(MappingsCommands::List)"
        );
    }

    // -----------------------------------------------------------------------
    // unknown subcommand
    // -----------------------------------------------------------------------

    /// An unknown subcommand under `mappings` must fail with a clap parse error.
    #[test]
    fn mappings_unknown_subcommand_fails() {
        let result = parse(&["ccag", "mappings", "frobnicate"]);
        assert!(
            result.is_err(),
            "unknown mappings subcommand must produce a clap parse error"
        );
    }
}
