//! Short playlist links: `https://glyph.my/p/<id>` instead of a long
//! `glyph:playlist:` code.
//!
//! Only signed-in accounts make links (`POST /v1/share`, see
//! [`crate::accounts`]); each link belongs to its account and is deleted with
//! it. `GET /v1/share/<id>` gives the app the code and the author back;
//! `GET /p/<id>` is a small page for people who open the link in a browser.
//! This module holds the parts that need no storage.

use crate::server::Response;
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
/// `found` = `None` renders "not found". Same look as the site at `/` (`site.rs`).
pub fn page(found: Option<(&SharedPlaylist, &str)>) -> Response {
    let (status, head_title, body) = match found {
        Some((p, link)) => {
            let title = if p.name.is_empty() { "Playlist".to_string() } else { escape(&p.name) };
            let mut meta = Vec::new();
            if let Some(a) = &p.author {
                meta.push(format!("shared by @{}", escape(a)));
            }
            match p.songs {
                0 => {}
                1 => meta.push("1 song".into()),
                n => meta.push(format!("{n} songs")),
            }
            let meta = if meta.is_empty() { String::new() } else { format!("<p class=\"dim\">{}</p>", meta.join(" · ")) };
            let body = format!(
                r#"<pre class="rule" data-rule="shared playlist"></pre>
<h1>{title}</h1>{meta}
<p>Paste this link into the search bar of Glyph to add the playlist.</p>
<div class="l"><code id="l">{link}</code><button class="act" id="cp" type="button">copy</button></div>
<div class="row"><a class="act alt" href="https://github.com/glyphmusic/desktop/releases/latest">no glyph yet? download it</a></div>"#,
                link = escape(link)
            );
            (200, format!("{title} · Glyph"), body)
        }
        None => (
            404,
            "Glyph".to_string(),
            r#"<pre class="rule" data-rule="not found"></pre>
<h1>Link not found</h1><p class="dim">This playlist link does not exist or was deleted.</p>
<div class="row"><a class="act alt" href="/">what is glyph?</a></div>"#
                .to_string(),
        ),
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex"><meta name="color-scheme" content="dark"><title>{head_title}</title>
<meta property="og:title" content="{head_title}"><meta property="og:description" content="A playlist to open in Glyph, the desktop music player.">
<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin><link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600;700&display=swap">
<style>{css}
body{{padding:4vh max(16px,env(safe-area-inset-left,0px)) 48px}}main{{max-width:58ch;margin:0 auto;display:flex;flex-direction:column;gap:18px}}
.top{{display:flex;justify-content:space-between}}.top a{{text-decoration:none;font-weight:700}}.top a::before{{content:"♪ ";color:var(--pink)}}
canvas{{display:block;width:100%;height:240px}}h1{{font-size:26px;line-height:1.2;margin:0;word-break:break-word}}h1+p{{margin-top:-12px}}p{{margin:0}}
.l{{display:flex;gap:1ch;align-items:stretch}}code{{flex:1;min-width:0;font:inherit;padding:9px 2ch;border:1px dashed var(--dim);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;user-select:all}}.l .act::after{{content:none}}.row{{display:flex}}</style>
</head><body><main><div class="top"><a href="/">glyph</a><span class="dim">glyph.my</span></div><canvas id="note" role="img" aria-label="The Glyph note, drawn in ASCII characters"></canvas>{body}</main>
<script>{js}
glyphNote(document.getElementById("note"),6);
document.querySelectorAll("[data-rule]").forEach(el=>{{const b=document.createElement("b");b.textContent=el.dataset.rule;el.append("-- ",b," "+"-".repeat(120))}});
const cp=document.getElementById("cp");if(cp)cp.onclick=()=>navigator.clipboard.writeText(document.getElementById("l").textContent).then(()=>cp.textContent="copied");
</script></body></html>"#,
        css = crate::site::BASE_CSS,
        js = crate::site::NOTE_JS,
    );
    // Links die with their account: never cache them.
    crate::site::html_response(status, html, "no-store")
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
