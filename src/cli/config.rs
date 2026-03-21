use anyhow::{Context, Result};
use std::path::PathBuf;

/// Persistent CLI config stored at ~/.ccag/config.json
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct CliConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

fn config_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("Could not determine home directory")?
        .join(".ccag"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("token"))
}

fn load_config() -> CliConfig {
    config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &CliConfig) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(config_path()?, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

/// Write a file with restricted permissions (owner-only read/write on Unix)
fn write_private_file(path: &PathBuf, content: &str) -> Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Resolve the gateway URL from: --url flag > CCAG_URL env > ~/.ccag/config.json
pub fn resolve_url(url_flag: Option<String>) -> Result<String> {
    url_flag
        .or_else(|| load_config().url)
        .context("Gateway URL required. Set CCAG_URL env var, pass --url flag, or run: ccag login --url <gateway-url>")
}

/// Client for the CCAG admin API (used by config/keys/users commands)
pub struct AdminClient {
    pub base_url: String,
    pub token: String,
    http: reqwest::Client,
}

impl AdminClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Create from CLI flags / env vars, prompting for login if no token provided
    pub async fn from_options(url: Option<String>, token: Option<String>) -> Result<Self> {
        let url = resolve_url(url)?;

        let token = match token {
            Some(t) => t,
            None => {
                let tp = token_path()?;

                if tp.exists() {
                    let t = std::fs::read_to_string(&tp)?;
                    let client = Self::new(&url, &t);
                    match client.get("/auth/me").await {
                        Ok(_) => t,
                        Err(_) => {
                            eprintln!("Saved token expired. Please log in.");
                            let t = Self::interactive_login(&url).await?;
                            write_private_file(&tp, &t)?;
                            t
                        }
                    }
                } else {
                    let t = Self::interactive_login(&url).await?;
                    let dir = config_dir()?;
                    std::fs::create_dir_all(&dir)?;
                    write_private_file(&tp, &t)?;
                    t
                }
            }
        };

        Ok(Self::new(&url, &token))
    }

    /// Interactive login: prompts for username/password, returns session token
    pub async fn interactive_login(base_url: &str) -> Result<String> {
        let username = dialoguer::Input::<String>::new()
            .with_prompt("Admin username")
            .default("admin".to_string())
            .interact_text()?;

        let password = dialoguer::Password::new()
            .with_prompt("Admin password")
            .interact()?;

        Self::login_with_credentials(base_url, &username, &password).await
    }

    /// Login with explicit credentials (no TTY required)
    pub async fn login_with_credentials(
        base_url: &str,
        username: &str,
        password: &str,
    ) -> Result<String> {
        let http = reqwest::Client::new();
        let resp = http
            .post(format!("{}/auth/login", base_url.trim_end_matches('/')))
            .json(&serde_json::json!({
                "username": username,
                "password": password,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Login failed: {}", resp.status());
        }

        let body: serde_json::Value = resp.json().await?;
        body["token"]
            .as_str()
            .map(|s| s.to_string())
            .context("No token in login response")
    }

    /// Save URL to persistent config
    pub fn save_url(url: &str) -> Result<()> {
        let mut cfg = load_config();
        cfg.url = Some(url.trim_end_matches('/').to_string());
        save_config(&cfg)
    }

    /// Save token to persistent storage (restricted permissions)
    pub fn save_token(token: &str) -> Result<()> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        write_private_file(&token_path()?, token)?;
        Ok(())
    }

    pub async fn get(&self, path: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("API error {status}: {body}");
        }
        Ok(body)
    }

    pub async fn post(&self, path: &str, json: &serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(json)
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("API error {status}: {body}");
        }
        Ok(body)
    }

    pub async fn put(&self, path: &str, json: &serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .http
            .put(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .json(json)
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("API error {status}: {body}");
        }
        Ok(body)
    }

    pub async fn delete(&self, path: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .delete(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("API error {status}: {body}");
        }
        Ok(body)
    }
}
