pub mod budget;
pub mod endpoints;
pub mod idp;
pub mod keys;
pub mod model_mappings;
pub mod model_pricing;
pub mod notification_config;
pub mod org_analytics;

pub mod schema;
pub mod scim_groups;
pub mod scim_tokens;
pub mod search_providers;
pub mod sessions;
pub mod settings;
pub mod spend;
pub mod teams;
pub mod users;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Pool, Postgres};

pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .idle_timeout(std::time::Duration::from_secs(600))
        .connect(database_url)
        .await?;

    tracing::info!("Connected to database");
    Ok(pool)
}

/// Generate an RDS IAM auth token using SigV4 presigning.
///
/// This produces a presigned URL (used as the password) that RDS validates
/// against the caller's IAM identity. Tokens expire after 15 minutes.
pub async fn generate_rds_iam_token(
    aws_config: &aws_config::SdkConfig,
    host: &str,
    port: u16,
    user: &str,
) -> anyhow::Result<String> {
    use aws_credential_types::provider::ProvideCredentials;
    use aws_sigv4::http_request::{
        SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
    };
    use aws_sigv4::sign::v4;
    use std::time::SystemTime;

    let credentials = aws_config
        .credentials_provider()
        .ok_or_else(|| anyhow::anyhow!("No AWS credentials provider configured"))?
        .provide_credentials()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to resolve AWS credentials: {e}"))?;

    let region = aws_config
        .region()
        .map(|r| r.as_ref().to_string())
        .unwrap_or_else(|| "us-east-1".to_string());

    let mut signing_settings = SigningSettings::default();
    signing_settings.expires_in = Some(std::time::Duration::from_secs(900));
    signing_settings.signature_location = SignatureLocation::QueryParams;

    let identity = credentials.into();
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(&region)
        .name("rds-db")
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()?;

    // Build the canonical request URL that RDS expects
    let url = format!("https://{host}:{port}/?Action=connect&DBUser={user}",);

    let signable_request =
        SignableRequest::new("GET", &url, std::iter::empty(), SignableBody::Bytes(&[]))?;

    let (signing_instructions, _signature) =
        sign(signable_request, &signing_params.into())?.into_parts();

    // Build the token by appending signing query params to the URL
    let mut signed_url = url::Url::parse(&url)?;
    for (name, value) in signing_instructions.params() {
        signed_url.query_pairs_mut().append_pair(name, value);
    }

    // The token is the full signed URL without the scheme
    let token = signed_url
        .as_str()
        .strip_prefix("https://")
        .unwrap_or(signed_url.as_str())
        .to_string();

    tracing::debug!(token_len = token.len(), "Generated RDS IAM auth token");

    Ok(token)
}

/// Connect to RDS using IAM authentication.
pub async fn connect_iam(
    aws_config: &aws_config::SdkConfig,
    host: &str,
    port: u16,
    db_name: &str,
    user: &str,
) -> anyhow::Result<PgPool> {
    let token = generate_rds_iam_token(aws_config, host, port, user).await?;

    let options = PgConnectOptions::new()
        .host(host)
        .port(port)
        .database(db_name)
        .username(user)
        .password(&token)
        .ssl_mode(PgSslMode::Require);

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .idle_timeout(std::time::Duration::from_secs(600))
        // Recycle connections before IAM token expires (15 min)
        .max_lifetime(std::time::Duration::from_secs(600))
        .connect_with(options)
        .await?;

    tracing::info!("Connected to database (IAM auth)");
    Ok(pool)
}

/// Background loop that refreshes the IAM auth token and swaps the connection pool.
///
/// IAM tokens expire after 15 min. We refresh every 10 min to stay ahead.
/// Old pool connections drain naturally (PgPool is Arc-based).
pub fn start_iam_refresh_loop(
    state: std::sync::Arc<crate::proxy::GatewayState>,
    aws_config: aws_config::SdkConfig,
    host: String,
    port: u16,
    db_name: String,
    user: String,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
        // Skip the first tick (we just connected)
        interval.tick().await;

        loop {
            interval.tick().await;
            tracing::debug!("Refreshing RDS IAM auth token");

            match connect_iam(&aws_config, &host, port, &db_name, &user).await {
                Ok(new_pool) => {
                    let mut pool_guard = state.db_pool.write().await;
                    *pool_guard = new_pool;
                    drop(pool_guard);
                    tracing::info!("Refreshed database connection pool (IAM auth)");
                }
                Err(e) => {
                    tracing::warn!(%e, "Failed to refresh IAM database pool — existing connections still valid");
                }
            }
        }
    });
}

pub async fn run_migrations(pool: &Pool<Postgres>) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    tracing::info!("Database migrations complete");
    Ok(())
}
