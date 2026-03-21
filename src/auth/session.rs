use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

use super::oidc::OidcIdentity;

const ISSUER: &str = "ccag";

#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    sub: String,
    idp: String,
    iss: String,
    exp: i64,
    iat: i64,
}

/// Issue a gateway session token (HS256-signed JWT).
pub fn issue(signing_key: &str, identity: &OidcIdentity, ttl_hours: u64) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = SessionClaims {
        sub: identity.sub.clone(),
        idp: identity.idp_name.clone(),
        iss: ISSUER.to_string(),
        iat: now,
        exp: now + (ttl_hours as i64 * 3600),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(signing_key.as_bytes()),
    )
    .expect("JWT encoding should not fail")
}

/// Validate a gateway session token. Returns the identity if valid.
pub fn validate(signing_key: &str, token: &str) -> Result<OidcIdentity, anyhow::Error> {
    // Quick check: gateway tokens are HS256 JWTs with iss=ccag.
    // External OIDC tokens use RS256, so we can reject them fast.
    let header = jsonwebtoken::decode_header(token)?;
    if header.alg != Algorithm::HS256 {
        anyhow::bail!("not a gateway session token");
    }

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[ISSUER]);
    validation.validate_aud = false;

    let token_data = decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(signing_key.as_bytes()),
        &validation,
    )?;

    Ok(OidcIdentity {
        sub: token_data.claims.sub,
        idp_name: token_data.claims.idp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_issue_and_validate() {
        let key = "test-secret-key";
        let identity = OidcIdentity {
            sub: "testuser".to_string(),
            idp_name: "TestIDP".to_string(),
        };

        let token = issue(key, &identity, 24);
        let result = validate(key, &token).unwrap();
        assert_eq!(result.sub, "testuser");
        assert_eq!(result.idp_name, "TestIDP");
    }

    #[test]
    fn test_wrong_key_rejected() {
        let identity = OidcIdentity {
            sub: "user".to_string(),
            idp_name: "IDP".to_string(),
        };
        let token = issue("key1", &identity, 24);
        assert!(validate("key2", &token).is_err());
    }

    #[test]
    fn test_expired_token_rejected() {
        let key = "test-key";
        // Manually create an expired token
        let now = chrono::Utc::now().timestamp();
        let claims = super::SessionClaims {
            sub: "user".to_string(),
            idp: "IDP".to_string(),
            iss: "ccag".to_string(),
            iat: now - 7200,
            exp: now - 3600, // expired 1 hour ago
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(key.as_bytes()),
        )
        .unwrap();
        assert!(validate(key, &token).is_err());
    }
}
