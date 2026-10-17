//! Admin authentication: Argon2id password hashes and stateless HMAC tokens.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_PASSWORD_LEN: usize = 256;
pub const MIN_PASSWORD_LEN: usize = 10;
pub const ACCESS_TTL_S: u64 = 15 * 60;
pub const REFRESH_TTL_S: u64 = 30 * 86_400;

/// OWASP-recommended Argon2id parameters (19 MiB, 2 passes, 1 lane).
pub fn default_params() -> Params {
    Params::new(19_456, 2, 1, None).expect("valid argon2 params")
}

/// Hash with a caller-supplied salt (16+ random bytes) so this crate needs no
/// RNG on wasm. Returns a PHC string (`$argon2id$...`).
pub fn hash_password(password: &str, salt: &[u8]) -> Result<String, String> {
    hash_with(password, salt, default_params())
}

pub fn hash_with(password: &str, salt: &[u8], params: Params) -> Result<String, String> {
    if salt.len() < 16 {
        return Err("salt must be at least 16 bytes".into());
    }
    let salt = SaltString::encode_b64(salt).map_err(|e| e.to_string())?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Constant-time verification (inside argon2). Wrong/oversized input -> false.
pub fn verify_password(password: &str, phc: &str) -> bool {
    if password.len() > MAX_PASSWORD_LEN {
        return false;
    }
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// True when `phc` is a well-formed Argon2id hash.
pub fn is_valid_hash(phc: &str) -> bool {
    PasswordHash::new(phc).map(|h| h.algorithm.as_str() == "argon2id").unwrap_or(false)
}

/// Cheap sanity check shown by the setup wizard.
pub fn password_problem(pw: &str) -> Option<&'static str> {
    if pw.chars().count() < MIN_PASSWORD_LEN {
        Some("the password must have at least 10 characters")
    } else if pw.len() > MAX_PASSWORD_LEN {
        Some("the password is too long (max 256 bytes)")
    } else {
        None
    }
}

// ------------------------------------------------------------------ tokens

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claims {
    /// `a` access, `r` refresh.
    pub k: char,
    pub sid: String,
    pub iat: u64,
    pub exp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    Expired,
    WrongKind,
}

/// Signing key derived from the password hash: changing the password
/// invalidates every token.
pub fn token_key(password_hash: &str) -> Vec<u8> {
    Sha256::digest(format!("glyph-admin-token-v1:{password_hash}").as_bytes()).to_vec()
}

type HmacSha256 = Hmac<Sha256>;

fn mac(key: &[u8]) -> HmacSha256 {
    <HmacSha256 as Mac>::new_from_slice(key).expect("hmac accepts any key length")
}

/// Session ids only need to be unique, not secret.
pub fn session_id(key: &[u8], now_ms: u64, who: &str) -> String {
    let mut h = Sha256::new();
    h.update(key);
    h.update(now_ms.to_le_bytes());
    h.update(who.as_bytes());
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

pub fn issue(key: &[u8], kind: char, sid: &str, now_ms: u64, ttl_s: u64) -> String {
    let claims = Claims { k: kind, sid: sid.to_string(), iat: now_ms / 1000, exp: now_ms / 1000 + ttl_s };
    let payload = B64.encode(serde_json::to_vec(&claims).expect("claims serialize"));
    let mut m = mac(key);
    m.update(payload.as_bytes());
    format!("{payload}.{}", B64.encode(m.finalize().into_bytes()))
}

pub fn verify_token(key: &[u8], token: &str, kind: char, now_ms: u64) -> Result<Claims, TokenError> {
    let (payload, sig) = token.split_once('.').ok_or(TokenError::Malformed)?;
    let sig = B64.decode(sig).map_err(|_| TokenError::Malformed)?;
    let mut m = mac(key);
    m.update(payload.as_bytes());
    // Constant-time comparison.
    m.verify_slice(&sig).map_err(|_| TokenError::BadSignature)?;
    let claims: Claims = B64
        .decode(payload)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(TokenError::Malformed)?;
    if claims.k != kind {
        return Err(TokenError::WrongKind);
    }
    if claims.exp <= now_ms / 1000 {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

#[cfg(test)]
pub(crate) fn cheap_hash(pw: &str) -> String {
    hash_with(pw, &[7u8; 16], Params::new(8, 1, 1, None).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = cheap_hash("correct horse battery");
        assert!(h.starts_with("$argon2id$"));
        assert!(is_valid_hash(&h));
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("correct horse batterx", &h));
        assert!(!verify_password("", &h));
        assert!(!verify_password(&"a".repeat(MAX_PASSWORD_LEN + 1), &h));
        assert!(!verify_password("x", "not a hash"));
        assert!(!is_valid_hash("not a hash"));
        assert!(!is_valid_hash("$argon2i$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNo"));
        assert!(hash_password("x", &[1; 4]).is_err());
    }

    #[test]
    fn default_params_hash_roundtrip() {
        let h = hash_password("a long enough password", &[9u8; 16]).unwrap();
        assert!(h.contains("m=19456,t=2,p=1"));
        assert!(verify_password("a long enough password", &h));
    }

    #[test]
    fn password_policy() {
        assert!(password_problem("short").is_some());
        assert!(password_problem("long enough pw").is_none());
    }

    #[test]
    fn token_roundtrip_and_expiry() {
        let key = token_key(&cheap_hash("pw pw pw pw pw"));
        let t = issue(&key, 'a', "sid1", 1_000_000, ACCESS_TTL_S);
        let c = verify_token(&key, &t, 'a', 1_000_000 + 1000).unwrap();
        assert_eq!((c.sid.as_str(), c.k), ("sid1", 'a'));
        // still valid just before expiry, expired at expiry
        assert!(verify_token(&key, &t, 'a', 1_000_000 + (ACCESS_TTL_S - 1) * 1000).is_ok());
        assert_eq!(verify_token(&key, &t, 'a', 1_000_000 + ACCESS_TTL_S * 1000), Err(TokenError::Expired));
    }

    #[test]
    fn token_rejects_tampering_wrong_kind_and_other_keys() {
        let key = token_key("hash-a");
        let other = token_key("hash-b");
        let t = issue(&key, 'a', "s", 0, 100);
        assert_eq!(verify_token(&other, &t, 'a', 0), Err(TokenError::BadSignature));
        assert_eq!(verify_token(&key, &t, 'r', 0), Err(TokenError::WrongKind));
        // flip a payload character
        let mut bad = t.clone().into_bytes();
        bad[3] = if bad[3] == b'A' { b'B' } else { b'A' };
        assert_eq!(verify_token(&key, &String::from_utf8(bad).unwrap(), 'a', 0), Err(TokenError::BadSignature));
        assert_eq!(verify_token(&key, "garbage", 'a', 0), Err(TokenError::Malformed));
        assert_eq!(verify_token(&key, "a.b", 'a', 0), Err(TokenError::Malformed));
        // refresh token cannot be used as access token
        let r = issue(&key, 'r', "s", 0, 100);
        assert_eq!(verify_token(&key, &r, 'a', 0), Err(TokenError::WrongKind));
    }

    #[test]
    fn session_ids_differ() {
        let k = token_key("h");
        assert_ne!(session_id(&k, 1, "ip"), session_id(&k, 2, "ip"));
        assert_eq!(session_id(&k, 1, "ip").len(), 16);
    }
}
