//! Short playlist links: `https://glyph.my/p/<id>` instead of a long
//! `glyph:playlist:` code.
//!
//! The app sends the code it would otherwise copy (`POST /v1/share`); the
//! server keeps it under a short id derived from its hash (the same playlist
//! always gets the same link) and answers with the link. `GET /v1/share/<id>`
//! gives the app the code back; `GET /p/<id>` is a small page for people who
//! open the link in a browser. Codes live in the long-lived cache (Workers KV).

use crate::cache::{self, DAY};
use crate::error::ApiErr;
use crate::server::{Body, Request, Response};
use crate::traits::{Method, RateDecision, Services};
use glyph_protocol::INSTALL_ID_HEADER;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "glyph:playlist:";
/// A code of ~10 000 songs.
pub const MAX_CODE_BYTES: usize = 1024 * 1024;
const TTL_S: u64 = 365 * DAY;
/// New links per install (or IP) per hour.
const CREATES_PER_HOUR: u32 = 30;
const ID_LEN: usize = 8;
/// Used when the short id is already taken by another code.
const LONG_ID_LEN: usize = 12;

#[derive(Debug, Deserialize)]
pub struct ShareRequest {
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub songs: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ShareResponse {
    pub id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    songs: u32,
}

/// Paths owned by this module.
pub fn owns(path: &str) -> bool {
    path == "/v1/share" || path.starts_with("/v1/share/") || path.starts_with("/p/")
}

pub fn max_body_bytes(path: &str) -> Option<usize> {
    (path == "/v1/share").then_some(MAX_CODE_BYTES + 4096)
}

/// `true` for a short id: 8 or 12 of `[A-Za-z0-9]`.
pub fn valid_id(id: &str) -> bool {
    (id.len() == ID_LEN || id.len() == LONG_ID_LEN) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn id_for(code: &str, len: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    Sha256::digest(code.as_bytes()).iter().take(len).map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect()
}

fn key(id: &str) -> String {
    format!("share:v1:{id}")
}

/// Where links point: the configured share origin, else this server.
fn origin(svc: &Services, req: &Request) -> String {
    if let Some(o) = svc.config.share_origin.as_deref() {
        return o.trim_end_matches('/').to_string();
    }
    let host = req.header("host").filter(|h| !h.is_empty() && h.len() <= 253 && !h.contains(['/', ' ', '"', '<'])).unwrap_or("localhost");
    format!("{}://{host}", if req.secure { "https" } else { "http" })
}

pub async fn handle(svc: &Services, req: Request) -> Result<Response, ApiErr> {
    let path = req.path.trim_end_matches('/').to_string();
    if path == "/v1/share" {
        if req.method != Method::Post {
            return Err(ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint"));
        }
        return create(svc, &req).await.map(|r| Response::json(200, &r));
    }
    if req.method != Method::Get {
        return Err(ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint"));
    }
    if let Some(id) = path.strip_prefix("/v1/share/") {
        let e = load(svc, id).await?;
        return Ok(Response::json(200, &serde_json::json!({ "code": e.code, "name": e.name, "songs": e.songs })));
    }
    let id = path.strip_prefix("/p/").unwrap_or_default();
    Ok(match load(svc, id).await {
        Ok(e) => page(200, &e.name, e.songs, &format!("{}/p/{id}", origin(svc, &req))),
        Err(_) => page(404, "", 0, ""),
    })
}

async fn load(svc: &Services, id: &str) -> Result<Entry, ApiErr> {
    if !valid_id(id) {
        return Err(ApiErr::not_found("no such playlist link"));
    }
    cache::get_json::<Entry>(&*svc.cache, &key(id)).await.ok_or_else(|| ApiErr::not_found("no such playlist link"))
}

async fn create(svc: &Services, req: &Request) -> Result<ShareResponse, ApiErr> {
    if req.body.len() > MAX_CODE_BYTES + 4096 {
        return Err(ApiErr::new(413, "payload_too_large", "playlist too large"));
    }
    let r: ShareRequest = serde_json::from_slice(&req.body).map_err(|_| ApiErr::bad_request("request body is not valid JSON for this endpoint"))?;
    let code = r.code.trim().to_string();
    let body = code.strip_prefix(PREFIX).unwrap_or_default();
    if body.is_empty() || code.len() > MAX_CODE_BYTES || !body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(ApiErr::bad_request("code must be a glyph:playlist: code"));
    }
    let voter = req
        .header(INSTALL_ID_HEADER)
        .filter(|id| !id.is_empty() && id.len() <= 64)
        .map(|id| format!("id:{id}"))
        .or_else(|| req.client_ip.as_ref().map(|ip| format!("ip:{ip}")))
        .unwrap_or_else(|| "unknown".into());
    if let RateDecision::Limited { retry_after_s } = svc.limiter.check(&format!("share:{voter}"), CREATES_PER_HOUR, 3600).await {
        return Err(ApiErr::rate_limited(retry_after_s));
    }
    let entry = Entry {
        name: r.name.unwrap_or_default().trim().chars().filter(|c| !c.is_control()).take(100).collect(),
        songs: r.songs.unwrap_or(0),
        code,
    };
    let mut id = id_for(&entry.code, ID_LEN);
    if cache::get_json::<Entry>(&*svc.cache, &key(&id)).await.is_some_and(|e| e.code != entry.code) {
        id = id_for(&entry.code, LONG_ID_LEN);
    }
    // Written every time: sharing again keeps the link alive another year.
    cache::put_json(&*svc.cache, &key(&id), &entry, TTL_S).await;
    Ok(ShareResponse { url: format!("{}/p/{id}", origin(svc, req)), id })
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Landing page for a link opened in a browser: how to open it in Glyph.
fn page(status: u16, name: &str, songs: u32, link: &str) -> Response {
    let body = if status == 200 {
        let title = if name.is_empty() { "Playlist".to_string() } else { escape(name) };
        let count = match songs {
            0 => String::new(),
            1 => "<p class=\"m\">1 song</p>".into(),
            n => format!("<p class=\"m\">{n} songs</p>"),
        };
        format!(
            r#"<h1>{title}</h1>{count}
<p>Paste this link into the search bar of Glyph to add the playlist.</p>
<div class="l"><code id="l">{link}</code><button onclick="navigator.clipboard.writeText(document.getElementById('l').textContent).then(()=>this.textContent='Copied')">Copy link</button></div>
<p class="m">No Glyph yet? <a href="https://github.com/glyphmusic/desktop/releases/latest">Download it</a>.</p>"#,
            link = escape(link)
        )
    } else {
        "<h1>Link not found</h1><p>This playlist link does not exist or has expired.</p>".to_string()
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex"><title>Glyph</title>
<style>:root{{color-scheme:dark light}}body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#0b0b0c;color:#ececec;font:16px/1.5 system-ui,sans-serif}}main{{max-width:520px;padding:32px 20px}}h1{{font-size:28px;margin:0 0 4px;word-break:break-word}}.m{{color:#9a9a9a;margin:0 0 16px}}.l{{display:flex;gap:8px;align-items:center;margin:20px 0}}code{{flex:1;padding:10px 12px;border-radius:8px;background:#1a1a1c;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}button{{padding:10px 14px;border:0;border-radius:8px;background:#ececec;color:#0b0b0c;font:inherit;cursor:pointer}}a{{color:inherit}}</style>
</head><body><main><p class="m">Glyph</p>{body}</main></body></html>"#
    );
    Response {
        status,
        headers: vec![
            ("content-type".into(), "text/html; charset=utf-8".into()),
            ("cache-control".into(), "public, max-age=300".into()),
            ("x-content-type-options".into(), "nosniff".into()),
            ("x-frame-options".into(), "DENY".into()),
            ("referrer-policy".into(), "no-referrer".into()),
            ("content-security-policy".into(), "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".into()),
        ],
        body: Body::Bytes(html.into_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use crate::testutil::*;
    use futures::executor::block_on;

    fn post(code: &str) -> Request {
        let body = serde_json::to_vec(&serde_json::json!({ "code": code, "name": "Road <trip>", "songs": 2 })).unwrap();
        Request::new(Method::Post, "/v1/share", vec![("host".into(), "api.example".into())], body, Some("1.2.3.4".into()))
    }

    #[test]
    fn create_resolve_and_page() {
        let mut h = harness(Default::default());
        std::sync::Arc::get_mut(&mut h.svc).unwrap().config.share_origin = Some("https://glyph.my/".into());
        let s = Server::from_arc(h.svc.clone());
        let r = block_on(s.handle(post("glyph:playlist:q1YqU7Iy1FHKU7JSCshJrMzJLC5RMFTSUSpRsoquBpFKESHOQH4i")));
        assert_eq!(r.status, 200);
        let made: ShareResponse = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert!(valid_id(&made.id));
        assert_eq!(made.url, format!("https://glyph.my/p/{}", made.id));
        // The same playlist keeps its link.
        let again: ShareResponse = serde_json::from_slice(block_on(s.handle(post("glyph:playlist:q1YqU7Iy1FHKU7JSCshJrMzJLC5RMFTSUSpRsoquBpFKESHOQH4i"))).bytes().unwrap()).unwrap();
        assert_eq!(again.id, made.id);

        let get = |p: &str| Request::new(Method::Get, p, vec![], vec![], Some("1.2.3.4".into()));
        let r = block_on(s.handle(get(&format!("/v1/share/{}", made.id))));
        let v: serde_json::Value = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert!(v["code"].as_str().unwrap().starts_with(PREFIX));
        let r = block_on(s.handle(get(&format!("/p/{}", made.id))));
        let html = String::from_utf8_lossy(r.bytes().unwrap()).to_string();
        assert!(r.status == 200 && html.contains("Road &lt;trip&gt;") && !html.contains("<trip>"));
        assert_eq!(block_on(s.handle(get("/p/nope"))).status, 404);
        assert_eq!(block_on(s.handle(get("/v1/share/AAAAAAAA"))).status, 404);
    }

    #[test]
    fn rejects_anything_but_codes() {
        let h = harness(Default::default());
        let s = Server::from_arc(h.svc.clone());
        for bad in ["https://evil.example", "glyph:playlist:", "glyph:playlist:<script>"] {
            assert_eq!(block_on(s.handle(post(bad))).status, 400, "{bad}");
        }
    }
}
