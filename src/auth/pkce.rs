use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// A PKCE (RFC 7636) verifier/challenge pair for one login attempt.
/// `verifier` is kept server-side only; `challenge` is the value sent to the IDP.
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a fresh PKCE pair using the S256 challenge method.
pub fn generate() -> PkceChallenge {
    let bytes: [u8; 32] = rand::random();
    let verifier = URL_SAFE_NO_PAD.encode(bytes);

    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);

    PkceChallenge {
        verifier,
        challenge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_is_43_to_128_chars_unreserved() {
        // RFC 7636 §4.1: 43-128 chars from [A-Z a-z 0-9 - . _ ~]. Base64url (no pad) of
        // 32 random bytes is 43 chars and satisfies the charset.
        let pkce = generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(
            pkce.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn challenge_is_sha256_of_verifier() {
        let pkce = generate();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn each_call_is_random() {
        let a = generate();
        let b = generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }
}
