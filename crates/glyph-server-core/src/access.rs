//! Optional server access, independent of account/admin authentication.
use crate::{ApiErr, Request, Response, Server};
use sha2::{Digest, Sha256};
pub const HEADER: &str = "x-glyph-server-authorization";

pub fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
}

impl Server {
    /// Called by both the core router and streaming adapters, before cache/provider access.
    /// Only GET health stays public. Admin/internal routes keep their own credentials.
    pub fn check_access(&self, req: &Request) -> Result<(), Response> {
        let svc = self.services();
        let Some(expected) = svc.config.server_bearer_hash.as_deref() else { return Ok(()) };
        if req.path.trim_end_matches('/') == "/v1/health" && req.method == crate::Method::Get
            || req.path.starts_with("/admin/v1/") || req.path.starts_with("/internal/") {
            return Ok(());
        }
        if !req.secure && !req.local {
            return Err(Response::error(ApiErr::new(403, "https_required", "server access requires HTTPS")));
        }
        // Account Authorization must remain independent. Never accept query/cookie credentials.
        let mut values = req.headers.iter().filter(|(k,_)| k == HEADER);
        let supplied = values.next().and_then(|(_,v)| v.strip_prefix("Bearer "));
        let valid = values.next().is_none() && supplied.is_some_and(|v| {
            let hash = token_hash(v);
            hash.len() == expected.len() && hash.bytes().zip(expected.bytes()).fold(0u8, |d,(a,b)| d | (a ^ b)) == 0
        });
        if valid { return Ok(()) }
        let mut response = Response::error(ApiErr::new(401, "server_auth_required", "this server requires an access token"));
        response.headers.push(("www-authenticate".into(), "Bearer realm=\"Glyph server\"".into()));
        Err(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Method, testutil::*};
    use futures::executor::block_on;
    fn request(path: &str, token: Option<&str>) -> Request {
        Request::new(Method::Get,path,token.map(|t| vec![(HEADER.into(),format!("Bearer {t}"))]).unwrap_or_default(),vec![],Some("1.2.3.4".into())).with_transport(true,false)
    }
    #[test] fn protects_every_surface_and_retains_public_mode() {
        let h = harness(Default::default());
        let server = Server::from_arc(h.svc.clone());
        assert!(server.check_access(&request("/v1/audio/youtube/7wtfhZwyrcc",None)).is_ok());
        let token = "test-bearer-with-at-least-32-characters";
        server.update_config(|c| c.server_bearer_hash = Some(token_hash(token)));
        let health = block_on(server.handle(request("/v1/health",None)));
        assert_eq!(health.status,200);
        assert!(serde_json::from_slice::<serde_json::Value>(health.bytes().unwrap()).unwrap()["server_auth_required"].as_bool().unwrap());
        for path in ["/v1/search?q=test", "/v1/resolve", "/v1/audio/youtube/7wtfhZwyrcc", "/v1/download/audius/123", "/v1/upstream?url=https://example.com", "/v1/profile", "/v1/sync/export", "/p/test", "/v1/access", "/v1/health/other"] {
            assert_eq!(server.check_access(&request(path,None)).err().unwrap().status,401,"{path}");
            assert_eq!(server.check_access(&request(path,Some("wrong"))).err().unwrap().status,401);
            assert!(server.check_access(&request(path,Some(token))).is_ok());
        }
        let mut r = request("/v1/access",Some(token));
        r.headers.push((HEADER.into(),format!("Bearer {token}")));
        assert!(server.check_access(&r).is_err(),"duplicate credentials are ambiguous");
        r = request("/v1/access",Some(token)); r.secure=false;
        assert_eq!(server.check_access(&r).err().unwrap().status,403);
        r.local=true; assert!(server.check_access(&r).is_ok());
        r = request("/v1/access?token=test-bearer-with-at-least-32-characters",None);
        r.headers.push(("authorization".into(),format!("Bearer {token}")));
        assert!(server.check_access(&r).is_err(),"account authorization never unlocks server gate");
        assert_eq!(block_on(server.handle(request("/v1/access",Some(token)))).status,200);
        assert_eq!(h.http.call_count("youtube"),0);
    }
}
