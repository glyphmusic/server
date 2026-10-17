//! Short playlist links: `https://glyph.my/p/<id>` instead of a long
//! `glyph:playlist:` code.
//!
//! Only signed-in accounts make links (`POST /v1/share`, see
//! [`crate::accounts`]); each link belongs to its account and is deleted with
//! it. `GET /v1/share/<id>` gives the app the code and the author back;
//! `GET /p/<id>` is a small page for people who open the link in a browser.
//! This module holds the parts that need no storage.

use crate::server::{Body, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "glyph:playlist:";
/// A code of ~10 000 songs.
pub const MAX_CODE_BYTES: usize = 1024 * 1024;
/// New links per account per hour.
pub const CREATES_PER_HOUR: u32 = 30;
const ID_LEN: usize = 8;
/// Used when the short id is already taken by another link.
pub const LONG_ID_LEN: usize = 12;

#[derive(Debug, Deserialize)]
pub struct ShareRequest {
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub songs: Option<u32>,
    /// "album" for an artist's single / album; anything else is a playlist.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ShareResponse {
    pub id: String,
    pub url: String,
}

/// What `GET /v1/share/<id>` answers.
#[derive(Debug, Serialize, Deserialize)]
pub struct SharedPlaylist {
    pub code: String,
    pub name: String,
    pub songs: u32,
    pub kind: String,
    /// Handle of the account that shared it; `None` for albums.
    pub author: Option<String>,
}

pub fn max_body_bytes(path: &str) -> Option<usize> {
    (path == "/v1/share").then_some(MAX_CODE_BYTES + 4096)
}

/// `true` for a short id: 8 or 12 of `[A-Za-z0-9]`.
pub fn valid_id(id: &str) -> bool {
    (id.len() == ID_LEN || id.len() == LONG_ID_LEN) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Id of `owner`'s link to `code`: sharing the same playlist again keeps it.
pub fn id_for(owner: &str, code: &str, len: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut h = Sha256::new();
    h.update(owner.as_bytes());
    h.update(b"\n");
    h.update(code.as_bytes());
    h.finalize().iter().take(len).map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect()
}

/// The trimmed code, or `None` when `code` is not a `glyph:playlist:` code.
pub fn clean_code(code: &str) -> Option<String> {
    let code = code.trim();
    let body = code.strip_prefix(PREFIX)?;
    (!body.is_empty() && code.len() <= MAX_CODE_BYTES && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| code.to_string())
}

pub fn clean_name(name: Option<String>) -> String {
    name.unwrap_or_default().trim().chars().filter(|c| !c.is_control()).take(100).collect()
}

pub fn clean_kind(kind: Option<&str>) -> String {
    if kind == Some("album") { "album" } else { "playlist" }.to_string()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Landing page for a link opened in a browser: how to open it in Glyph.
/// `found` = `None` renders "not found".
pub fn page(found: Option<(&SharedPlaylist, &str)>) -> Response {
    let (status, body) = match found {
        Some((p, link)) => {
            let title = if p.name.is_empty() { "Playlist".to_string() } else { escape(&p.name) };
            let mut meta = Vec::new();
            if let Some(a) = &p.author {
                meta.push(format!("Shared by @{}", escape(a)));
            }
            match p.songs {
                0 => {}
                1 => meta.push("1 song".into()),
                n => meta.push(format!("{n} songs")),
            }
            let meta = if meta.is_empty() { String::new() } else { format!("<p class=\"m\">{}</p>", meta.join(" · ")) };
            (
                200,
                format!(
                    r#"<h1>{title}</h1>{meta}
<p>Paste this link into the search bar of Glyph to add the playlist.</p>
<div class="l"><code id="l">{link}</code><button onclick="navigator.clipboard.writeText(document.getElementById('l').textContent).then(()=>this.textContent='Copied')">Copy link</button></div>
<p class="m">No Glyph yet? <a href="https://github.com/glyphmusic/desktop/releases/latest">Download it</a>.</p>"#,
                    link = escape(link)
                ),
            )
        }
        None => (404, "<h1>Link not found</h1><p>This playlist link does not exist or was deleted.</p>".to_string()),
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
            ("cache-control".into(), "no-store".into()),
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

    #[test]
    fn ids_and_codes() {
        let a = id_for("u1", "glyph:playlist:abc", 8);
        assert!(valid_id(&a));
        assert_eq!(a, id_for("u1", "glyph:playlist:abc", 8));
        assert_ne!(a, id_for("u2", "glyph:playlist:abc", 8));
        assert!(!valid_id("short") && !valid_id("AAAA/AAA"));
        assert_eq!(clean_code(" glyph:playlist:q1Y-_ ").as_deref(), Some("glyph:playlist:q1Y-_"));
        for bad in ["https://evil.example", "glyph:playlist:", "glyph:playlist:<script>"] {
            assert_eq!(clean_code(bad), None, "{bad}");
        }
        assert_eq!(clean_kind(Some("album")), "album");
        assert_eq!(clean_kind(Some("x")), "playlist");
    }

    #[test]
    fn page_escapes() {
        let p = SharedPlaylist { code: String::new(), name: "Road <trip>".into(), songs: 2, kind: "playlist".into(), author: Some("gisela".into()) };
        let r = page(Some((&p, "https://glyph.my/p/AbC123xy")));
        let html = String::from_utf8_lossy(r.bytes().unwrap()).to_string();
        assert!(html.contains("Road &lt;trip&gt;") && !html.contains("<trip>") && html.contains("@gisela"));
        assert_eq!(page(None).status, 404);
    }
}
