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

        // Database URL resolution:
        // 1. DATABASE_HOST + RDS_IAM_AUTH → passwordless URL (IAM token at runtime)
        // 2. DATABASE_HOST + DB_PASSWORD  → URL with password (CDK with Secrets Manager)
        // 3. DATABASE_URL                 → direct URL (Docker Compose)
        let database_url = if let Some(host) = &database_host {
            if rds_iam_auth {
                format!(
                    "postgres://{}@{}:{}/{}",
                    database_user, host, database_port, database_name,
                )
            } else {
                let password = std::env::var("DB_PASSWORD").map_err(|_| {
                    anyhow::anyhow!(
                        "DB_PASSWORD is required when DATABASE_HOST is set and RDS_IAM_AUTH is not enabled"
                    )
                })?;
                format!(
                    "postgres://{}:{}@{}:{}/{}",
                    database_user, password, host, database_port, database_name,
                )
            }
        } else {
            std::env::var("DATABASE_URL")
                .map_err(|_| anyhow::anyhow!("DATABASE_URL or DATABASE_HOST is required"))?
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

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // --- resolve_routing_prefix: pure function, no env vars ---

    #[test]
    fn resolve_us_east() {
        assert_eq!(GatewayConfig::resolve_routing_prefix("us-east-1"), "us");
    }

    #[test]
    fn resolve_us_west() {
        assert_eq!(GatewayConfig::resolve_routing_prefix("us-west-2"), "us");
    }

    #[test]
    fn resolve_us_gov() {
        assert_eq!(
            GatewayConfig::resolve_routing_prefix("us-gov-west-1"),
            "us-gov"
        );
    }

    #[test]
    fn resolve_ca_central() {
        assert_eq!(GatewayConfig::resolve_routing_prefix("ca-central-1"), "us");
    }

    #[test]
    fn resolve_eu_west() {
        assert_eq!(GatewayConfig::resolve_routing_prefix("eu-west-1"), "eu");
    }

    #[test]
    fn resolve_eu_central() {
        assert_eq!(GatewayConfig::resolve_routing_prefix("eu-central-1"), "eu");
    }

    #[test]
    fn resolve_ap_southeast_2_is_au() {
        assert_eq!(
            GatewayConfig::resolve_routing_prefix("ap-southeast-2"),
            "au"
        );
    }

    #[test]
    fn resolve_ap_southeast_4_is_au() {
        assert_eq!(
            GatewayConfig::resolve_routing_prefix("ap-southeast-4"),
            "au"
        );
    }

    #[test]
    fn resolve_ap_northeast_1_is_jp() {
        assert_eq!(
            GatewayConfig::resolve_routing_prefix("ap-northeast-1"),
            "jp"
        );
    }

    #[test]
    fn resolve_ap_south_is_apac() {
        assert_eq!(GatewayConfig::resolve_routing_prefix("ap-south-1"), "apac");
    }

    #[test]
    fn resolve_me_south_is_apac() {
        assert_eq!(GatewayConfig::resolve_routing_prefix("me-south-1"), "apac");
    }

    #[test]
    fn resolve_unknown_falls_back_to_us() {
        assert_eq!(
            GatewayConfig::resolve_routing_prefix("unknown-region-1"),
            "us"
        );
    }

    // --- from_env: requires env var isolation ---

    /// Clear all CCAG env vars to ensure test isolation.
    fn clear_env() {
        for key in [
            "PROXY_HOST",
            "PROXY_PORT",
            "ADMIN_USERNAME",
            "ADMIN_PASSWORD",
            "DATABASE_URL",
            "DATABASE_HOST",
            "DATABASE_PORT",
            "DATABASE_NAME",
            "DATABASE_USER",
            "DB_PASSWORD",
            "RDS_IAM_AUTH",
            "ADMIN_USERS",
            "BUDGET_NOTIFICATION_URL",
        ] {
            // SAFETY: tests using from_env are #[serial] so no concurrent env access.
            unsafe { std::env::remove_var(key) };
        }
    }

    #[test]
    #[serial]
    fn from_env_with_database_url() {
        clear_env();
        unsafe { std::env::set_var("DATABASE_URL", "postgres://u:p@host/db") };
        let config = GatewayConfig::from_env().unwrap();
        assert_eq!(config.database_url, "postgres://u:p@host/db");
        // Check defaults
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.admin_username, "admin");
        assert_eq!(config.admin_password, "admin");
    }

    #[test]
    #[serial]
    fn from_env_with_database_host_iam() {
        clear_env();
        unsafe {
            std::env::set_var("DATABASE_HOST", "mydb.rds.amazonaws.com");
            std::env::set_var("RDS_IAM_AUTH", "true");
        }
        let config = GatewayConfig::from_env().unwrap();
        assert_eq!(
            config.database_url,
            "postgres://proxy@mydb.rds.amazonaws.com:5432/proxy"
        );
        assert!(config.rds_iam_auth);
    }

    #[test]
    #[serial]
    fn from_env_with_database_host_password() {
        clear_env();
        unsafe {
            std::env::set_var("DATABASE_HOST", "mydb.rds.amazonaws.com");
            std::env::set_var("DB_PASSWORD", "secret123");
        }
        let config = GatewayConfig::from_env().unwrap();
        assert_eq!(
            config.database_url,
            "postgres://proxy:secret123@mydb.rds.amazonaws.com:5432/proxy"
        );
    }

    #[test]
    #[serial]
    fn from_env_database_host_no_password_errors() {
        clear_env();
        unsafe { std::env::set_var("DATABASE_HOST", "mydb.rds.amazonaws.com") };
        let result = GatewayConfig::from_env();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("DB_PASSWORD"),
            "error should mention DB_PASSWORD: {msg}"
        );
    }

    #[test]
    #[serial]
    fn from_env_no_database_errors() {
        clear_env();
        let result = GatewayConfig::from_env();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("DATABASE_URL") || msg.contains("DATABASE_HOST"),
            "error should mention required vars: {msg}"
        );
    }

    #[test]
    #[serial]
    fn from_env_custom_port() {
        clear_env();
        unsafe {
            std::env::set_var("DATABASE_URL", "postgres://u:p@h/d");
            std::env::set_var("PROXY_PORT", "9090");
        }
        let config = GatewayConfig::from_env().unwrap();
        assert_eq!(config.port, 9090);
    }

    #[test]
    #[serial]
    fn admin_users_parsing() {
        clear_env();
        unsafe {
            std::env::set_var("DATABASE_URL", "postgres://u:p@h/d");
            std::env::set_var("ADMIN_USERS", "alice, bob, charlie");
        }
        let config = GatewayConfig::from_env().unwrap();
        assert_eq!(config.admin_users, vec!["alice", "bob", "charlie"]);
    }

    #[test]
    #[serial]
    fn admin_users_empty_string() {
        clear_env();
        unsafe {
            std::env::set_var("DATABASE_URL", "postgres://u:p@h/d");
            std::env::set_var("ADMIN_USERS", "");
        }
        let config = GatewayConfig::from_env().unwrap();
        assert!(config.admin_users.is_empty());
    }

    #[test]
    fn listen_addr_formats_correctly() {
        let config = GatewayConfig {
            host: "0.0.0.0".to_string(),
            port: 8080,
            admin_username: String::new(),
            admin_password: String::new(),
            bedrock_routing_prefix: String::new(),
            database_url: String::new(),
            admin_users: vec![],
            notification_url: None,
            rds_iam_auth: false,
            database_host: None,
            database_port: 5432,
            database_name: "proxy".to_string(),
            database_user: "proxy".to_string(),
        };
        let addr = config.listen_addr();
        assert_eq!(addr.ip().to_string(), "0.0.0.0");
        assert_eq!(addr.port(), 8080);
    }
}
