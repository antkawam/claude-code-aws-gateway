use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub host: String,
    pub port: u16,
    pub admin_username: String,
    pub admin_password: String,
    /// Bedrock inference profile prefix, auto-detected from AWS region.
    pub bedrock_routing_prefix: String,
    pub database_url: String,
    /// Bootstrap admin users (OIDC subjects) — auto-provisioned as admin on first login.
    pub admin_users: Vec<String>,
    /// Budget notification URL: webhook (https://...) or SNS topic ARN.
    pub notification_url: Option<String>,
    /// Use RDS IAM authentication instead of password.
    pub rds_iam_auth: bool,
    /// Database host (for IAM auth token generation).
    pub database_host: Option<String>,
    /// Database port (for IAM auth token generation).
    pub database_port: u16,
    /// Database name.
    pub database_name: String,
    /// Database user.
    pub database_user: String,
}

impl GatewayConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let rds_iam_auth = std::env::var("RDS_IAM_AUTH")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let database_host = std::env::var("DATABASE_HOST").ok();
        let database_port: u16 = std::env::var("DATABASE_PORT")
            .unwrap_or_else(|_| "5432".to_string())
            .parse()
            .unwrap_or(5432);
        let database_name = std::env::var("DATABASE_NAME").unwrap_or_else(|_| "proxy".to_string());
        let database_user = std::env::var("DATABASE_USER").unwrap_or_else(|_| "proxy".to_string());

        // When RDS_IAM_AUTH is enabled, we don't need a password — token is generated at runtime.
        let database_url = if rds_iam_auth && database_host.is_some() {
            // Placeholder URL — the actual connection uses IAM token via PgConnectOptions
            format!(
                "postgres://{}@{}:{}/{}",
                database_user,
                database_host.as_deref().unwrap(),
                database_port,
                database_name,
            )
        } else {
            std::env::var("DATABASE_URL").map_err(|_| {
                anyhow::anyhow!("DATABASE_URL is required when RDS_IAM_AUTH is not enabled.")
            })?
        };

        Ok(Self {
            host: std::env::var("PROXY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: std::env::var("PROXY_PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()?,
            admin_username: std::env::var("ADMIN_USERNAME").unwrap_or_else(|_| "admin".to_string()),
            admin_password: std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string()),
            admin_users: std::env::var("ADMIN_USERS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            // Placeholder — will be resolved from AWS SDK region at startup
            bedrock_routing_prefix: String::new(),
            database_url,
            notification_url: std::env::var("BUDGET_NOTIFICATION_URL").ok(),
            rds_iam_auth,
            database_host,
            database_port,
            database_name,
            database_user,
        })
    }

    /// Resolve the routing prefix from the AWS SDK's configured region.
    pub fn resolve_routing_prefix(aws_region: &str) -> String {
        let prefix = match aws_region {
            r if r.starts_with("us-gov") => "us-gov",
            r if r.starts_with("us-") || r.starts_with("ca-") => "us",
            r if r.starts_with("eu-") => "eu",
            "ap-southeast-2" | "ap-southeast-4" => "au",
            "ap-northeast-1" => "jp",
            r if r.starts_with("ap-") || r.starts_with("me-") => "apac",
            _ => "us",
        };
        prefix.to_string()
    }

    pub fn listen_addr(&self) -> SocketAddr {
        format!("{}:{}", self.host, self.port).parse().unwrap()
    }
}
