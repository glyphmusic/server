//! Requests from the Worker to its companion.
//!
//! The Worker signs every request with a shared secret instead of sending
//! the secret: `x-glyph-companion-sig` is an HMAC-SHA256 over a label, the
//! method, the request target, a millisecond timestamp and the SHA-256 of
//! the body. A captured request reveals no token, cannot be changed and is
//! refused after [`MAX_SKEW_MS`]; the companion also refuses a signature it
//! has already seen.
//!
//! Two secrets keep the roles apart: `YTDLP_SERVICE_TOKEN` for search,
//! resolving and audio, `COMPANION_ADMIN_TOKEN` for status and updates.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const TIMESTAMP_HEADER: &str = "x-glyph-companion-ts";
pub const SIGNATURE_HEADER: &str = "x-glyph-companion-sig";
/// Cloudflare Access service token headers (optional, checked by Cloudflare).
pub const ACCESS_ID_HEADER: &str = "cf-access-client-id";
pub const ACCESS_SECRET_HEADER: &str = "cf-access-client-secret";
/// Accepted difference between the Worker's and the companion's clocks.
pub const MAX_SKEW_MS: u64 = 60_000;
/// Shortest secret either side accepts.
pub const MIN_TOKEN_LEN: usize = 32;

const LABEL: &str = "glyph-companion-v1";

/// Which secret a companion path needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Search, resolve, import, lyrics, audio.
    Service,
    /// Status, update checks and installs.
    Admin,
}

pub fn role_of(path: &str) -> Role {
    match path {
        "/internal/status" | "/internal/update/check" | "/internal/update/apply" | "/internal/r-dlp/check" | "/internal/r-dlp/apply" => {
            Role::Admin
        }
        _ => Role::Service,
    }
}

fn mac(token: &str, method: &str, target: &str, ts: u64, body: &[u8]) -> Hmac<Sha256> {
    let body_hash: String = Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(token.as_bytes()).expect("hmac accepts any key length");
    m.update(format!("{LABEL}\n{}\n{target}\n{ts}\n{body_hash}", method.to_ascii_uppercase()).as_bytes());
    m
}

/// Headers for one request (`target` = path and query as sent).
pub fn sign(token: &str, method: &str, target: &str, body: &[u8], now_ms: u64) -> [(&'static str, String); 2] {
    let sig: String = mac(token, method, target, now_ms, body).finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    [(TIMESTAMP_HEADER, now_ms.to_string()), (SIGNATURE_HEADER, sig)]
}

/// Why a request was refused (never echoes the secret).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Missing,
    Stale,
    BadSignature,
}

/// Checks the signature headers of one request. `header` looks a header up
/// by lower-case name; a duplicated header counts as missing.
pub fn verify<'a>(
    token: &str,
    method: &str,
    target: &str,
    body: &[u8],
    header: impl Fn(&str) -> Option<&'a str>,
    now_ms: u64,
) -> Result<(), Refused> {
    let ts: u64 = header(TIMESTAMP_HEADER).and_then(|v| v.parse().ok()).ok_or(Refused::Missing)?;
    let sig = header(SIGNATURE_HEADER).ok_or(Refused::Missing)?;
    if ts.abs_diff(now_ms) > MAX_SKEW_MS {
        return Err(Refused::Stale);
    }
    let bytes = decode_hex(sig).ok_or(Refused::BadSignature)?;
    mac(token, method, target, ts, body).verify_slice(&bytes).map_err(|_| Refused::BadSignature)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Signatures seen within the last [`MAX_SKEW_MS`] (bounded), so a captured
/// request cannot be sent a second time.
#[derive(Default)]
pub struct ReplayGuard {
    seen: std::collections::VecDeque<(u64, String)>,
}

impl ReplayGuard {
    const CAP: usize = 50_000;

    /// `false` if `sig` was already used.
    pub fn first_use(&mut self, sig: &str, now_ms: u64) -> bool {
        while self.seen.front().is_some_and(|(t, _)| now_ms.saturating_sub(*t) > 2 * MAX_SKEW_MS) || self.seen.len() >= Self::CAP {
            self.seen.pop_front();
        }
        if self.seen.iter().any(|(_, s)| s == sig) {
            return false;
        }
        self.seen.push_back((now_ms, sig.to_string()));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn lookup<'a>(h: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        h.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn signed_requests_verify_and_changes_fail() {
        let h = sign(TOKEN, "POST", "/internal/resolve", b"{}", 1_000_000);
        assert_eq!(verify(TOKEN, "post", "/internal/resolve", b"{}", |n: &str| lookup(&h, n), 1_000_500), Ok(()));
        assert_eq!(verify(TOKEN, "POST", "/internal/resolve", b"{ }", |n: &str| lookup(&h, n), 1_000_500), Err(Refused::BadSignature));
        assert_eq!(verify(TOKEN, "POST", "/internal/import", b"{}", |n: &str| lookup(&h, n), 1_000_500), Err(Refused::BadSignature));
        assert_eq!(verify(TOKEN, "GET", "/internal/resolve", b"{}", |n: &str| lookup(&h, n), 1_000_500), Err(Refused::BadSignature));
        assert_eq!(verify(&TOKEN.replace('0', "1"), "POST", "/internal/resolve", b"{}", |n: &str| lookup(&h, n), 1_000_500), Err(Refused::BadSignature));
        assert_eq!(verify(TOKEN, "POST", "/internal/resolve", b"{}", |n: &str| lookup(&h, n), 1_000_000 + MAX_SKEW_MS + 1), Err(Refused::Stale));
        assert_eq!(verify(TOKEN, "POST", "/internal/resolve", b"{}", |_| None, 1_000_000), Err(Refused::Missing));
        let bad = [(TIMESTAMP_HEADER, "1000000".to_string()), (SIGNATURE_HEADER, "zz".repeat(32))];
        assert_eq!(verify(TOKEN, "POST", "/internal/resolve", b"{}", |n: &str| lookup(&bad, n), 1_000_000), Err(Refused::BadSignature));
    }

    #[test]
    fn signatures_are_single_use() {
        let mut g = ReplayGuard::default();
        assert!(g.first_use("a", 0));
        assert!(!g.first_use("a", 1_000));
        assert!(g.first_use("b", 1_000));
        // Entries older than twice the window are forgotten (and stale anyway).
        assert!(g.first_use("a", 3 * MAX_SKEW_MS));
    }

    #[test]
    fn roles() {
        assert_eq!(role_of("/internal/update/apply"), Role::Admin);
        assert_eq!(role_of("/internal/status"), Role::Admin);
        assert_eq!(role_of("/internal/audio/abcdefghijk"), Role::Service);
        assert_eq!(role_of("/internal/resolve"), Role::Service);
    }
}
