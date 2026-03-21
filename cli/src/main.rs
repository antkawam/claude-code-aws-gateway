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
            let who = me["email"]
                .as_str()
                .or(me["username"].as_str())
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
        Commands::Status(args) => commands::status::run(args, cli.url).await,
        Commands::Logs(args) => commands::logs::run(args).await,
        Commands::Update => commands::update::run().await,
    }
}
