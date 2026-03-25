use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::db::schema::IdentityProvider;

/// Configuration for a single IDP.
#[derive(Debug, Clone)]
pub struct IdpConfig {
    pub name: String,
    pub issuer: String,
    pub audience: Option<String>,
    pub jwks_url: Option<String>,
    pub auto_provision: bool,
    pub default_role: String,
    pub allowed_domains: Option<Vec<String>>,
    /// Which JWT claim to use as the user identifier.
    /// None or "auto" = fallback chain: email > preferred_username > upn > name > sub.
    /// Explicit values: "email", "preferred_username", "upn", "oid", "name", "sub".
    pub user_claim: Option<String>,
}

impl IdpConfig {
    pub fn from_env() -> Option<Self> {
        let issuer = std::env::var("OIDC_ISSUER").ok()?;
        let audience = std::env::var("OIDC_AUDIENCE").ok();
        let jwks_url = std::env::var("OIDC_JWKS_URL").ok();
        let user_claim = std::env::var("OIDC_USER_CLAIM").ok();

        // Derive a friendly name from the issuer URL, or allow explicit override
        let name = std::env::var("OIDC_NAME").unwrap_or_else(|_| {
            if issuer.contains("okta") {
                "Okta".to_string()
            } else if issuer.contains("login.microsoftonline") {
                "Azure AD".to_string()
            } else if issuer.contains("accounts.google") {
                "Google".to_string()
            } else {
                "SSO".to_string()
            }
        });

        Some(Self {
            name,
            issuer,
            audience,
            jwks_url,
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: None,
            user_claim,
        })
    }

    pub fn from_db_row(row: &IdentityProvider) -> Self {
        Self {
            name: row.name.clone(),
            issuer: row.issuer_url.clone(),
            audience: row.audience.clone(),
            jwks_url: row.jwks_url.clone(),
            auto_provision: row.auto_provision,
            default_role: row.default_role.clone(),
            allowed_domains: row.allowed_domains.clone(),
            user_claim: row.user_claim.clone(),
        }
    }

    /// Resolve the JWKS URL (explicit or derived from issuer discovery).
    fn effective_jwks_url(&self) -> String {
        self.jwks_url.clone().unwrap_or_else(|| {
            let base = self.issuer.trim_end_matches('/');
            format!("{base}/.well-known/openid-configuration")
        })
    }
}

/// JWT claims extracted from validated tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcClaims {
    pub sub: String,
    pub email: Option<String>,
    pub preferred_username: Option<String>,
    pub upn: Option<String>,
    pub oid: Option<String>,
    pub name: Option<String>,
    pub iss: String,
    pub aud: OidcAudience,
    pub exp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OidcAudience {
    Single(String),
    Multiple(Vec<String>),
}

/// Result of a successful OIDC validation.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    pub sub: String,
    /// Best available human-readable identifier: email > preferred_username > upn > sub.
    pub email: Option<String>,
    pub idp_name: String,
}

impl OidcIdentity {
    /// Returns the best available user identifier for DB storage and display.
    /// Prefers email/preferred_username/upn over the opaque `sub` claim,
    /// since some IDPs (e.g. Entra ID) use non-human-readable sub values.
    pub fn user_id(&self) -> &str {
        self.email.as_deref().unwrap_or(&self.sub)
    }
}

/// Resolve the user identifier from JWT claims based on the IDP's `user_claim` config.
///
/// When `user_claim` is None or "auto", uses a fallback chain:
///   email > preferred_username > upn > name
/// When set to a specific claim name, extracts that claim.
/// Returns None if the configured/fallback claim is not present in the token.
fn resolve_user_claim(user_claim: &Option<String>, claims: &OidcClaims) -> Option<String> {
    match user_claim.as_deref() {
        None | Some("auto") | Some("") => {
            // Fallback chain (industry standard for multi-IDP compatibility)
            claims
                .email
                .clone()
                .or_else(|| claims.preferred_username.clone())
                .or_else(|| claims.upn.clone())
                .or_else(|| claims.name.clone())
        }
        Some("email") => claims.email.clone(),
        Some("preferred_username") => claims.preferred_username.clone(),
        Some("upn") => claims.upn.clone(),
        Some("oid") => claims.oid.clone(),
        Some("name") => claims.name.clone(),
        Some("sub") => Some(claims.sub.clone()),
        Some(other) => {
            tracing::warn!(
                user_claim = other,
                "Unknown user_claim value, falling back to auto"
            );
            claims
                .email
                .clone()
                .or_else(|| claims.preferred_username.clone())
                .or_else(|| claims.upn.clone())
                .or_else(|| claims.name.clone())
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JwksResponse {
    pub keys: Vec<JwkKey>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JwkKey {
    pub kid: Option<String>,
    pub kty: String,
    pub alg: Option<String>,
    pub n: Option<String>,
    pub e: Option<String>,
    #[serde(rename = "use")]
    pub key_use: Option<String>,
}

/// Multi-IDP OIDC validator. Manages JWKS caches per issuer.
pub struct MultiIdpValidator {
    /// Map of issuer URL -> (IdpConfig, cached JWKS)
    idps: Arc<RwLock<HashMap<String, IdpEntry>>>,
    http_client: reqwest::Client,
}

struct IdpEntry {
    config: IdpConfig,
    jwks_cache: Option<JwksResponse>,
}

impl Default for MultiIdpValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiIdpValidator {
    pub fn new() -> Self {
        Self {
            idps: Arc::new(RwLock::new(HashMap::new())),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    /// Load IDPs from env + DB. Call on startup and when cache version changes.
    pub async fn load_idps(&self, configs: Vec<IdpConfig>) {
        let mut map = self.idps.write().await;
        // Keep existing JWKS caches for IDPs that haven't changed issuer
        let mut new_map = HashMap::new();
        for config in configs {
            let existing_jwks = map.get(&config.issuer).and_then(|e| e.jwks_cache.clone());
            new_map.insert(
                config.issuer.clone(),
                IdpEntry {
                    config,
                    jwks_cache: existing_jwks,
                },
            );
        }
        *map = new_map;
    }

    pub async fn idp_count(&self) -> usize {
        self.idps.read().await.len()
    }

    /// Validate a JWT token against all configured IDPs.
    /// Tries to match the token's `iss` claim to a known IDP.
    pub async fn validate_token(&self, token: &str) -> anyhow::Result<OidcIdentity> {
        // Peek at the token to get the issuer without full validation
        let header = decode_header(token)?;
        let kid = header.kid.clone();

        // Decode without validation to peek at issuer
        let mut peek_validation = Validation::default();
        peek_validation.insecure_disable_signature_validation();
        peek_validation.validate_aud = false;
        let peek: jsonwebtoken::TokenData<OidcClaims> =
            decode(token, &DecodingKey::from_secret(b""), &peek_validation)?;
        let issuer = &peek.claims.iss;

        // Find matching IDP
        let idps = self.idps.read().await;
        let entry = idps
            .get(issuer)
            .ok_or_else(|| anyhow::anyhow!("No configured IDP for issuer: {issuer}"))?;

        let config = entry.config.clone();
        let cached_jwks = entry.jwks_cache.clone();
        drop(idps);

        // Get JWKS (from cache or fetch)
        let jwks = match cached_jwks {
            Some(jwks) => jwks,
            None => {
                let jwks = self.fetch_jwks(&config).await?;
                // Cache it
                let mut idps = self.idps.write().await;
                if let Some(entry) = idps.get_mut(issuer) {
                    entry.jwks_cache = Some(jwks.clone());
                }
                jwks
            }
        };

        // Find the matching key
        let jwk = self.find_key(&jwks.keys, kid.as_deref()).or({
            // Try without kid match (some IDPs don't set kid)
            None
        });

        let jwk = match jwk {
            Some(k) => k,
            None => {
                // Key rotation — invalidate cache and retry
                let jwks = self.fetch_jwks(&config).await?;
                let mut idps = self.idps.write().await;
                if let Some(entry) = idps.get_mut(issuer) {
                    entry.jwks_cache = Some(jwks.clone());
                }
                drop(idps);
                self.find_key(&jwks.keys, kid.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("No matching JWK found for kid: {:?}", kid))?
            }
        };

        // Validate the token
        let n = jwk
            .n
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("JWK missing 'n'"))?;
        let e = jwk
            .e
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("JWK missing 'e'"))?;
        let decoding_key = DecodingKey::from_rsa_components(n, e)?;

        let alg = match jwk.alg.as_deref() {
            Some("RS256") | None => Algorithm::RS256,
            Some("RS384") => Algorithm::RS384,
            Some("RS512") => Algorithm::RS512,
            Some(other) => anyhow::bail!("Unsupported JWK algorithm: {other}"),
        };

        let mut validation = Validation::new(alg);
        validation.set_issuer(&[&config.issuer]);
        if let Some(ref aud) = config.audience {
            validation.set_audience(&[aud]);
        } else {
            validation.validate_aud = false;
        }

        let token_data = decode::<OidcClaims>(token, &decoding_key, &validation)?;
        let claims = token_data.claims;

        // Check allowed domains (exact domain match after @)
        if let Some(ref domains) = config.allowed_domains
            && !domains.is_empty()
        {
            let email = claims.email.as_deref().unwrap_or("");
            let user_domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
            let domain_ok = domains.iter().any(|d| {
                let d = d.strip_prefix('@').unwrap_or(d);
                user_domain.eq_ignore_ascii_case(d)
            });
            if !domain_ok {
                anyhow::bail!("Email domain not in allowed list for IDP {}", config.name);
            }
        }

        // Resolve user identifier based on IDP's user_claim config.
        // "auto" (or unset) uses fallback chain: email > preferred_username > upn > name > sub.
        let resolved_email = resolve_user_claim(&config.user_claim, &claims);

        Ok(OidcIdentity {
            sub: claims.sub,
            email: resolved_email,
            idp_name: config.name.clone(),
        })
    }

    fn find_key(&self, keys: &[JwkKey], kid: Option<&str>) -> Option<JwkKey> {
        keys.iter()
            .find(|k| {
                if let (Some(token_kid), Some(key_kid)) = (kid, k.kid.as_deref()) {
                    token_kid == key_kid
                } else {
                    k.kty == "RSA" && k.key_use.as_deref() != Some("enc")
                }
            })
            .cloned()
    }

    async fn fetch_jwks(&self, config: &IdpConfig) -> anyhow::Result<JwksResponse> {
        let url = config.effective_jwks_url();

        // Enforce HTTPS for JWKS/discovery endpoints to prevent MitM
        // (allow localhost/127.0.0.1 for local development and testing)
        if !url.starts_with("https://")
            && !url.starts_with("http://localhost")
            && !url.starts_with("http://127.0.0.1")
        {
            anyhow::bail!("JWKS URL must use HTTPS: {}", url);
        }

        // If URL points to discovery, resolve jwks_uri first
        let jwks_url = if url.contains("openid-configuration") {
            let discovery: serde_json::Value =
                self.http_client.get(&url).send().await?.json().await?;
            discovery["jwks_uri"]
                .as_str()
                .ok_or_else(|| {
                    anyhow::anyhow!("No jwks_uri in discovery document for {}", config.name)
                })?
                .to_string()
        } else {
            url
        };

        let jwks: JwksResponse = self.http_client.get(&jwks_url).send().await?.json().await?;

        tracing::info!(idp = %config.name, keys = jwks.keys.len(), "Fetched JWKS");
        Ok(jwks)
    }

    /// Invalidate all JWKS caches (called periodically).
    pub async fn invalidate_all_caches(&self) {
        let mut idps = self.idps.write().await;
        for entry in idps.values_mut() {
            entry.jwks_cache = None;
        }
    }

    /// Start JWKS refresh loop (hourly).
    pub fn start_refresh_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let validator = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                validator.invalidate_all_caches().await;
                tracing::debug!("Invalidated all JWKS caches for refresh");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use jsonwebtoken::{EncodingKey, Header};

    /// Generate a fresh RSA key pair at runtime and return (EncodingKey, JwkKey).
    /// Avoids storing private key material in source code.
    fn generate_test_key(kid: &str) -> (EncodingKey, JwkKey) {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::traits::PublicKeyParts;

        let mut rng = rsa::rand_core::OsRng;
        let private_key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let public_key = private_key.to_public_key();

        let pem = private_key
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .unwrap();
        let encoding_key = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();

        let n = URL_SAFE_NO_PAD.encode(public_key.n().to_bytes_be());
        let e = URL_SAFE_NO_PAD.encode(public_key.e().to_bytes_be());

        let jwk = JwkKey {
            kid: Some(kid.to_string()),
            kty: "RSA".to_string(),
            alg: Some("RS256".to_string()),
            n: Some(n),
            e: Some(e),
            key_use: Some("sig".to_string()),
        };
        (encoding_key, jwk)
    }

    fn test_key_1(kid: &str) -> (EncodingKey, JwkKey) {
        generate_test_key(kid)
    }

    fn test_key_2(kid: &str) -> (EncodingKey, JwkKey) {
        generate_test_key(kid)
    }

    /// Create a validator pre-loaded with an IDP and its JWKS cache.
    async fn setup_validator(
        issuer: &str,
        audience: Option<&str>,
        kid: &str,
    ) -> (MultiIdpValidator, EncodingKey) {
        let (encoding_key, jwk) = test_key_1(kid);
        let validator = MultiIdpValidator::new();

        let config = IdpConfig {
            name: "test-idp".to_string(),
            issuer: issuer.to_string(),
            audience: audience.map(String::from),
            jwks_url: None,
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: None,
            user_claim: None,
        };

        validator.load_idps(vec![config]).await;

        // Inject JWKS cache directly
        {
            let mut idps = validator.idps.write().await;
            if let Some(entry) = idps.get_mut(issuer) {
                entry.jwks_cache = Some(JwksResponse { keys: vec![jwk] });
            }
        }

        (validator, encoding_key)
    }

    fn make_jwt(encoding_key: &EncodingKey, kid: &str, claims: &OidcClaims) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, claims, encoding_key).unwrap()
    }

    fn valid_claims(issuer: &str) -> OidcClaims {
        OidcClaims {
            sub: "user-123".to_string(),
            email: Some("user@example.com".to_string()),
            preferred_username: None,
            upn: None,
            oid: None,
            name: None,
            iss: issuer.to_string(),
            aud: OidcAudience::Single("test-audience".to_string()),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as u64,
        }
    }

    #[tokio::test]
    async fn valid_jwt_validates_successfully() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (validator, encoding_key) = setup_validator(issuer, Some("test-audience"), kid).await;

        let claims = valid_claims(issuer);
        let token = make_jwt(&encoding_key, kid, &claims);

        let result = validator.validate_token(&token).await;
        assert!(result.is_ok(), "Expected valid token: {:?}", result.err());

        let identity = result.unwrap();
        assert_eq!(identity.sub, "user-123");
        assert_eq!(identity.email.as_deref(), Some("user@example.com"));
        assert_eq!(identity.user_id(), "user@example.com");
        assert_eq!(identity.idp_name, "test-idp");
    }

    #[tokio::test]
    async fn expired_jwt_is_rejected() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (validator, encoding_key) = setup_validator(issuer, Some("test-audience"), kid).await;

        let claims = OidcClaims {
            exp: (chrono::Utc::now() - chrono::Duration::hours(1)).timestamp() as u64,
            ..valid_claims(issuer)
        };
        let token = make_jwt(&encoding_key, kid, &claims);

        let result = validator.validate_token(&token).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("ExpiredSignature"),
            "Expected expired signature error, got: {err}"
        );
    }

    #[tokio::test]
    async fn wrong_audience_is_rejected() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (validator, encoding_key) =
            setup_validator(issuer, Some("correct-audience"), kid).await;

        let claims = OidcClaims {
            aud: OidcAudience::Single("wrong-audience".to_string()),
            ..valid_claims(issuer)
        };
        let token = make_jwt(&encoding_key, kid, &claims);

        let result = validator.validate_token(&token).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("InvalidAudience"),
            "Expected audience error, got: {err}"
        );
    }

    #[tokio::test]
    async fn unknown_issuer_is_rejected() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (validator, encoding_key) = setup_validator(issuer, Some("test-audience"), kid).await;

        let claims = OidcClaims {
            iss: "https://unknown-idp.example.com".to_string(),
            ..valid_claims(issuer)
        };
        let token = make_jwt(&encoding_key, kid, &claims);

        let result = validator.validate_token(&token).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("No configured IDP"),
            "Expected unknown IDP error, got: {err}"
        );
    }

    #[tokio::test]
    async fn malformed_jwt_is_rejected() {
        let validator = MultiIdpValidator::new();

        let result = validator.validate_token("not.a.jwt").await;
        assert!(result.is_err());

        let result = validator.validate_token("").await;
        assert!(result.is_err());

        let result = validator.validate_token("abc123").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn no_audience_config_skips_audience_validation() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (validator, encoding_key) = setup_validator(issuer, None, kid).await;

        let claims = OidcClaims {
            aud: OidcAudience::Single("any-audience".to_string()),
            ..valid_claims(issuer)
        };
        let token = make_jwt(&encoding_key, kid, &claims);

        let result = validator.validate_token(&token).await;
        assert!(
            result.is_ok(),
            "Expected valid token without audience check: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn multi_idp_routing() {
        let issuer_a = "https://idp-a.example.com";
        let issuer_b = "https://idp-b.example.com";
        let kid_a = "key-a";
        let kid_b = "key-b";

        let (key_a, jwk_a) = test_key_1(kid_a);
        let (key_b, jwk_b) = test_key_2(kid_b);

        let validator = MultiIdpValidator::new();

        let configs = vec![
            IdpConfig {
                name: "IDP-A".to_string(),
                issuer: issuer_a.to_string(),
                audience: Some("aud-a".to_string()),
                jwks_url: None,
                auto_provision: true,
                default_role: "member".to_string(),
                allowed_domains: None,
                user_claim: None,
            },
            IdpConfig {
                name: "IDP-B".to_string(),
                issuer: issuer_b.to_string(),
                audience: Some("aud-b".to_string()),
                jwks_url: None,
                auto_provision: true,
                default_role: "member".to_string(),
                allowed_domains: None,
                user_claim: None,
            },
        ];
        validator.load_idps(configs).await;

        // Inject JWKS caches
        {
            let mut idps = validator.idps.write().await;
            idps.get_mut(issuer_a).unwrap().jwks_cache = Some(JwksResponse { keys: vec![jwk_a] });
            idps.get_mut(issuer_b).unwrap().jwks_cache = Some(JwksResponse { keys: vec![jwk_b] });
        }

        // Token from IDP-A
        let claims_a = OidcClaims {
            sub: "user-a".to_string(),
            aud: OidcAudience::Single("aud-a".to_string()),
            ..valid_claims(issuer_a)
        };
        let token_a = make_jwt(&key_a, kid_a, &claims_a);
        let result_a = validator.validate_token(&token_a).await.unwrap();
        assert_eq!(result_a.sub, "user-a");
        assert_eq!(result_a.idp_name, "IDP-A");

        // Token from IDP-B
        let claims_b = OidcClaims {
            sub: "user-b".to_string(),
            aud: OidcAudience::Single("aud-b".to_string()),
            ..valid_claims(issuer_b)
        };
        let token_b = make_jwt(&key_b, kid_b, &claims_b);
        let result_b = validator.validate_token(&token_b).await.unwrap();
        assert_eq!(result_b.sub, "user-b");
        assert_eq!(result_b.idp_name, "IDP-B");
    }

    #[tokio::test]
    async fn jwks_fetch_from_mock_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (encoding_key, jwk) = test_key_1("mock-key");

        let mock_server = MockServer::start().await;

        let jwks_body = serde_json::json!({
            "keys": [{
                "kid": jwk.kid,
                "kty": jwk.kty,
                "alg": jwk.alg,
                "n": jwk.n,
                "e": jwk.e,
                "use": jwk.key_use,
            }]
        });

        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&jwks_body))
            .mount(&mock_server)
            .await;

        let issuer = format!("{}/issuer", mock_server.uri());
        let validator = MultiIdpValidator::new();

        let config = IdpConfig {
            name: "mock-idp".to_string(),
            issuer: issuer.clone(),
            audience: None,
            jwks_url: Some(format!("{}/jwks", mock_server.uri())),
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: None,
            user_claim: None,
        };
        validator.load_idps(vec![config]).await;

        let claims = OidcClaims {
            sub: "fetched-user".to_string(),
            email: None,
            preferred_username: None,
            upn: None,
            oid: None,
            name: None,
            iss: issuer.clone(),
            aud: OidcAudience::Single("any".to_string()),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as u64,
        };
        let token = make_jwt(&encoding_key, "mock-key", &claims);

        let result = validator.validate_token(&token).await;
        assert!(
            result.is_ok(),
            "Expected valid token via JWKS fetch: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().sub, "fetched-user");
    }

    #[tokio::test]
    async fn allowed_domains_enforced() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (encoding_key, jwk) = test_key_1(kid);

        let validator = MultiIdpValidator::new();
        let config = IdpConfig {
            name: "domain-restricted".to_string(),
            issuer: issuer.to_string(),
            audience: None,
            jwks_url: None,
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: Some(vec!["@allowed.com".to_string()]),
            user_claim: None,
        };
        validator.load_idps(vec![config]).await;
        {
            let mut idps = validator.idps.write().await;
            idps.get_mut(issuer).unwrap().jwks_cache = Some(JwksResponse { keys: vec![jwk] });
        }

        // Allowed domain should pass
        let claims_ok = OidcClaims {
            sub: "user-ok".to_string(),
            email: Some("user@allowed.com".to_string()),
            preferred_username: None,
            upn: None,
            oid: None,
            name: None,
            iss: issuer.to_string(),
            aud: OidcAudience::Single("any".to_string()),
            exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as u64,
        };
        let token_ok = make_jwt(&encoding_key, kid, &claims_ok);
        assert!(validator.validate_token(&token_ok).await.is_ok());

        // Wrong domain should fail
        let claims_bad = OidcClaims {
            email: Some("user@blocked.com".to_string()),
            ..claims_ok
        };
        let token_bad = make_jwt(&encoding_key, kid, &claims_bad);
        let err = validator
            .validate_token(&token_bad)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Email domain not in allowed list"),
            "Expected domain error, got: {err}"
        );
    }

    #[tokio::test]
    async fn invalidate_all_caches_clears_jwks() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (_, jwk) = test_key_1(kid);

        let validator = MultiIdpValidator::new();
        let config = IdpConfig {
            name: "test".to_string(),
            issuer: issuer.to_string(),
            audience: None,
            jwks_url: None,
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: None,
            user_claim: None,
        };
        validator.load_idps(vec![config]).await;
        {
            let mut idps = validator.idps.write().await;
            idps.get_mut(issuer).unwrap().jwks_cache = Some(JwksResponse { keys: vec![jwk] });
        }

        {
            let idps = validator.idps.read().await;
            assert!(idps.get(issuer).unwrap().jwks_cache.is_some());
        }

        validator.invalidate_all_caches().await;

        {
            let idps = validator.idps.read().await;
            assert!(idps.get(issuer).unwrap().jwks_cache.is_none());
        }
    }

    #[tokio::test]
    async fn load_idps_preserves_existing_jwks_cache() {
        let issuer = "https://test-idp.example.com";
        let kid = "key-1";
        let (_, jwk) = test_key_1(kid);

        let validator = MultiIdpValidator::new();
        let config = IdpConfig {
            name: "test".to_string(),
            issuer: issuer.to_string(),
            audience: None,
            jwks_url: None,
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: None,
            user_claim: None,
        };
        validator.load_idps(vec![config.clone()]).await;
        {
            let mut idps = validator.idps.write().await;
            idps.get_mut(issuer).unwrap().jwks_cache = Some(JwksResponse { keys: vec![jwk] });
        }

        // Reload same IDP — cache should be preserved
        validator.load_idps(vec![config]).await;

        let idps = validator.idps.read().await;
        let entry = idps.get(issuer).unwrap();
        assert!(
            entry.jwks_cache.is_some(),
            "JWKS cache should survive reload"
        );
        assert_eq!(entry.jwks_cache.as_ref().unwrap().keys.len(), 1);
    }

    #[test]
    fn effective_jwks_url_derives_from_issuer() {
        let config = IdpConfig {
            name: "test".to_string(),
            issuer: "https://auth.example.com".to_string(),
            audience: None,
            jwks_url: None,
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: None,
            user_claim: None,
        };
        assert_eq!(
            config.effective_jwks_url(),
            "https://auth.example.com/.well-known/openid-configuration"
        );
    }

    #[test]
    fn effective_jwks_url_uses_explicit_override() {
        let config = IdpConfig {
            name: "test".to_string(),
            issuer: "https://issuer.example.com".to_string(),
            audience: None,
            jwks_url: Some("https://custom-jwks.example.com/keys".to_string()),
            auto_provision: true,
            default_role: "member".to_string(),
            allowed_domains: None,
            user_claim: None,
        };
        assert_eq!(
            config.effective_jwks_url(),
            "https://custom-jwks.example.com/keys"
        );
    }

    #[test]
    fn find_key_matches_by_kid() {
        let validator = MultiIdpValidator::new();
        let keys = vec![
            JwkKey {
                kid: Some("key-1".to_string()),
                kty: "RSA".to_string(),
                alg: Some("RS256".to_string()),
                n: Some("n1".to_string()),
                e: Some("e1".to_string()),
                key_use: Some("sig".to_string()),
            },
            JwkKey {
                kid: Some("key-2".to_string()),
                kty: "RSA".to_string(),
                alg: Some("RS256".to_string()),
                n: Some("n2".to_string()),
                e: Some("e2".to_string()),
                key_use: Some("sig".to_string()),
            },
        ];

        let found = validator.find_key(&keys, Some("key-2"));
        assert!(found.is_some());
        assert_eq!(found.unwrap().n.unwrap(), "n2");
    }

    #[test]
    fn find_key_without_kid_uses_rsa_sig_fallback() {
        let validator = MultiIdpValidator::new();
        let keys = vec![
            JwkKey {
                kid: None,
                kty: "RSA".to_string(),
                alg: Some("RS256".to_string()),
                n: Some("n-sig".to_string()),
                e: Some("e-sig".to_string()),
                key_use: Some("sig".to_string()),
            },
            JwkKey {
                kid: None,
                kty: "RSA".to_string(),
                alg: None,
                n: Some("n-enc".to_string()),
                e: Some("e-enc".to_string()),
                key_use: Some("enc".to_string()),
            },
        ];

        let found = validator.find_key(&keys, None);
        assert!(found.is_some());
        assert_eq!(found.unwrap().n.unwrap(), "n-sig");
    }
}
