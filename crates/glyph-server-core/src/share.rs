//! Short playlist links: `https://glyph.my/p/<id>` instead of a long
//! `glyph:playlist:` code.
//!
//! Only signed-in accounts make links (`POST /v1/share`, see
//! [`crate::accounts`]); each link belongs to its account and is deleted with
//! it. `GET /v1/share/<id>` gives the app the code and the author back;
//! `GET /p/<id>` is a page for people who open the link in a browser: they
//! can listen right there (`/p/<id>/listen/<n>`), and chat apps show a card
//! with the cover (`/p/<id>/cover`). `/embed/p/<id>` and `/oembed` give other
//! sites a player. This module holds the parts that need no storage.

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
    match kind { Some("album") => "album", Some("track") => "track", Some("artist") => "artist", _ => "playlist" }.to_string()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Largest decoded code (same cap as the app's import).
const MAX_JSON: usize = 4 * 1024 * 1024;
/// Songs listed (and playable) on the page and in the embed.
pub const MAX_LISTED: usize = 200;

#[derive(Deserialize)]
struct Code {
    #[serde(default)]
    t: Vec<Song>,
}

#[derive(Deserialize)]
struct Song {
    t: String,
    a: String,
    #[serde(default)]
    al: Option<String>,
    s: glyph_protocol::ProviderId,
    i: String,
    #[serde(default)]
    d: Option<u64>,
    #[serde(default)]
    isrc: Option<String>,
    #[serde(default)]
    x: bool,
}

/// The songs of a `glyph:playlist:` code (empty when it does not decode).
/// Same format as `glyph-core/src/share.rs`.
pub fn songs(code: &str) -> Vec<glyph_protocol::Track> {
    use base64::Engine;
    use std::io::Read;
    let Some(body) = code.trim().strip_prefix(PREFIX) else { return Vec::new() };
    let Ok(packed) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body.trim_end_matches('=')) else { return Vec::new() };
    let mut json = Vec::new();
    if flate2::read::DeflateDecoder::new(&packed[..]).take(MAX_JSON as u64 + 1).read_to_end(&mut json).is_err() || json.len() > MAX_JSON {
        return Vec::new();
    }
    let Ok(code) = serde_json::from_slice::<Code>(&json) else { return Vec::new() };
    code.t
        .into_iter()
        .filter(|s| !s.t.trim().is_empty())
        .map(|s| glyph_protocol::Track {
            id: s.i,
            source: s.s,
            title: s.t,
            artist: s.a,
            album: s.al,
            duration_ms: s.d,
            cover_url: None,
            isrc: s.isrc,
            permalink: None,
            external_ids: Default::default(),
            explicit: s.x,
        })
        .collect()
}

// ---- language ------------------------------------------------------------

/// Page language, picked from the visitor's `Accept-Language`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
}

impl Lang {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
        }
    }
}

/// The first of German / English the visitor lists; English otherwise.
/// Chat-app crawlers send English or nothing, so link cards stay English.
pub fn lang(accept: Option<&str>) -> Lang {
    let mut tags: Vec<(f32, usize, &str)> = accept
        .unwrap_or_default()
        .split(',')
        .enumerate()
        .filter_map(|(i, part)| {
            let mut it = part.split(';');
            let tag = it.next()?.trim();
            let q = it.find_map(|p| p.trim().strip_prefix("q=")).and_then(|q| q.parse().ok()).unwrap_or(1.0);
            (!tag.is_empty() && q > 0.0).then_some((q, i, tag))
        })
        .collect();
    tags.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, _, tag) in tags {
        let base = tag.split('-').next().unwrap_or_default().to_ascii_lowercase();
        match base.as_str() {
            "de" => return Lang::De,
            "en" => return Lang::En,
            _ => {}
        }
    }
    Lang::En
}

/// UI text. Wording follows the app's own strings (`glyph/assets/i18n`).
fn tr(l: Lang, key: &str) -> &'static str {
    let (en, de) = match key {
        "playlist" => ("Playlist", "Playlist"),
        "album" => ("Album", "Album"),
        "shared_by" => ("Shared by @{h}", "Geteilt von @{h}"),
        "play_all" => ("Play all", "Alle abspielen"),
        "shuffle" => ("Shuffle", "Zufallswiedergabe"),
        "prev" => ("Previous", "Zurück"),
        "next" => ("Next", "Weiter"),
        "play" => ("Play", "Abspielen"),
        "mute" => ("Mute", "Stumm"),
        "copy" => ("Copy link", "Link kopieren"),
        "copied" => ("Link copied", "Link kopiert"),
        "download" => ("Download Glyph", "Glyph herunterladen"),
        "idle_title" => ("Nothing playing", "Nichts läuft"),
        "idle_sub" => ("Pick a song to listen", "Wähl einen Song zum Anhören"),
        "song" => ("1 song", "1 Song"),
        "songs" => ("{n} songs", "{n} Songs"),
        "and_more" => ("and more", "und mehr"),
        "listen" => ("Listen in the browser or open it in Glyph.", "Im Browser anhören oder in Glyph öffnen."),
        "unplayable" => ("These songs only play in the Glyph app.", "Diese Songs laufen nur in der Glyph-App."),
        "more" => ("+ {n} more in Glyph", "+ {n} weitere in Glyph"),
        "nf_title" => ("Link not found", "Link nicht gefunden"),
        "nf_sub" => ("This playlist link does not exist or was deleted.", "Diesen Playlist-Link gibt es nicht oder er wurde gelöscht."),
        "what" => ("What is Glyph?", "Was ist Glyph?"),
        "cover_of" => ("Cover of {t}", "Cover von {t}"),
        "more_opts" => ("More options", "Weitere Optionen"),
        "repeat" => ("Repeat", "Wiederholen"),
        "repeat_one" => ("Repeat one", "Song wiederholen"),
        "unmute" => ("Unmute", "Ton an"),
        "copied_song" => ("Song title copied", "Songtitel kopiert"),
        "menu_play" => ("Play", "Abspielen"),
        "menu_copy_song" => ("Copy song title", "Songtitel kopieren"),
        "download_short" => ("Download", "Herunterladen"),
        _ => ("", ""),
    };
    if l == Lang::De { de } else { en }
}

// ---- texts -----------------------------------------------------------------

/// `https://glyph.my/p/AbC123xy` -> (`https://glyph.my`, `AbC123xy`).
fn split_link(link: &str) -> (&str, &str) {
    link.rsplit_once("/p/").unwrap_or(("", link))
}

/// The `/embed/p/<id>` player for a link.
pub fn embed_link(link: &str) -> String {
    let (origin, id) = split_link(link);
    format!("{origin}/embed/p/{id}")
}

fn kind_of(p: &SharedPlaylist, l: Lang) -> &'static str {
    match (p.kind.as_str(), l) { ("artist", Lang::De) => "Künstler", ("artist", _) => "Artist", ("track", _) => "Song", _ => tr(l, if p.kind == "album" { "album" } else { "playlist" }) }
}

/// The name, or "Playlist" / "Album".
fn title_of(p: &SharedPlaylist, l: Lang) -> String {
    if p.name.is_empty() { kind_of(p, l).to_string() } else { p.name.clone() }
}

fn count_text(n: usize, l: Lang) -> String {
    if n == 1 { tr(l, "song").to_string() } else { tr(l, "songs").replace("{n}", &n.to_string()) }
}

/// "Playlist · 12 songs · Shared by @gisela" (unescaped).
fn summary(p: &SharedPlaylist, count: usize, l: Lang) -> String {
    let mut parts = vec![kind_of(p, l).to_string()];
    let n = if count > 0 { count } else { p.songs as usize };
    if n > 0 {
        parts.push(count_text(n, l));
    }
    if let Some(a) = &p.author {
        parts.push(tr(l, "shared_by").replace("{h}", a));
    }
    parts.join(" · ")
}

/// Card text: the summary and the first artists.
fn description(p: &SharedPlaylist, songs: &[glyph_protocol::Track], l: Lang) -> String {
    let mut artists: Vec<&str> = Vec::new();
    for t in songs {
        if !artists.iter().any(|a| a.eq_ignore_ascii_case(&t.artist)) {
            artists.push(&t.artist);
        }
        if artists.len() == 4 {
            break;
        }
    }
    let mut d = summary(p, songs.len(), l);
    if !artists.is_empty() {
        d.push_str(" — ");
        d.push_str(&artists.join(", "));
        if songs.len() > artists.len() {
            d.push(' ');
            d.push_str(tr(l, "and_more"));
        }
        d.push('.');
    }
    d.push(' ');
    d.push_str(tr(l, "listen"));
    d
}

fn duration(ms: Option<u64>) -> String {
    ms.map(|ms| format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)).unwrap_or_else(|| "-:--".into())
}

/// `[ 07 SONGS · 11:50 ]`, the app's list meta line.
fn meta_line(songs: &[glyph_protocol::Track]) -> String {
    let s: u64 = songs.iter().filter_map(|t| t.duration_ms).sum::<u64>() / 1000;
    let total = if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) };
    format!("[ {:02} SONGS · {total} ]", songs.len())
}

// ---- markup ----------------------------------------------------------------

/// A Lucide icon (the same set the app ships in `glyph/assets/icons/ui`).
fn ic(name: &str) -> String {
    let (filled, body) = match name {
        "play" => (true, r#"<polygon points="6 3 20 12 6 21 6 3"/>"#),
        "pause" => (true, r#"<rect x="14" y="4" width="4" height="16" rx="1"/><rect x="6" y="4" width="4" height="16" rx="1"/>"#),
        "skip-back" => (false, r#"<polygon points="19 20 9 12 19 4 19 20"/><line x1="5" x2="5" y1="19" y2="5"/>"#),
        "skip-forward" => (false, r#"<polygon points="5 4 15 12 5 20 5 4"/><line x1="19" x2="19" y1="5" y2="19"/>"#),
        "shuffle" => (false, r#"<path d="M2 18h1.4c1.3 0 2.5-.6 3.3-1.7l6.1-8.6c.7-1.1 2-1.7 3.3-1.7H22"/><path d="m18 2 4 4-4 4"/><path d="M2 6h1.9c1.5 0 2.9.9 3.6 2.2"/><path d="M22 18h-5.9c-1.3 0-2.6-.7-3.3-1.8l-.5-.8"/><path d="m18 14 4 4-4 4"/>"#),
        "repeat" => (false, r#"<path d="m17 2 4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="m7 22-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/>"#),
        "repeat-1" => (false, r#"<path d="m17 2 4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="m7 22-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/><path d="M11 10h1v4"/>"#),
        "link-2" => (false, r#"<path d="M9 17H7A5 5 0 0 1 7 7h2"/><path d="M15 7h2a5 5 0 1 1 0 10h-2"/><line x1="8" x2="16" y1="12" y2="12"/>"#),
        "copy" => (false, r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#),
        "volume-2" => (false, r#"<path d="M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z"/><path d="M16 9a5 5 0 0 1 0 6"/><path d="M19.364 18.364a9 9 0 0 0 0-12.728"/>"#),
        "volume-x" => (false, r#"<path d="M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z"/><line x1="22" x2="16" y1="9" y2="15"/><line x1="16" x2="22" y1="9" y2="15"/>"#),
        "music-2" => (false, r#"<circle cx="8" cy="18" r="4"/><path d="M12 18V2l7 4"/>"#),
        "download" => (false, r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/>"#),
        "ellipsis-vertical" => (false, r#"<circle cx="12" cy="12" r="1"/><circle cx="12" cy="5" r="1"/><circle cx="12" cy="19" r="1"/>"#),
        "check" => (false, r#"<path d="M20 6 9 17l-5-5"/>"#),
        "search" => (false, r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>"#),
        "house" => (false, r#"<path d="M15 21v-8a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v8"/><path d="M3 10a2 2 0 0 1 .709-1.528l7-5.999a2 2 0 0 1 2.582 0l7 5.999A2 2 0 0 1 21 10v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>"#),
        "refresh-cw" => (false, r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>"#),
        "clock" => (false, r#"<circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/>"#),
        "user-round" => (false, r#"<circle cx="12" cy="8" r="5"/><path d="M20 21a8 8 0 0 0-16 0"/>"#),
        "server" => (false, r#"<rect width="20" height="8" x="2" y="2" rx="2" ry="2"/><rect width="20" height="8" x="2" y="14" rx="2" ry="2"/><line x1="6" x2="6.01" y1="6" y2="6"/><line x1="6" x2="6.01" y1="18" y2="18"/>"#),
        "users" => (false, r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>"#),
        "book-open" => (false, r#"<path d="M12 7v14"/><path d="M3 18a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h5a4 4 0 0 1 4 4 4 4 0 0 1 4-4h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3z"/>"#),
        _ => (false, ""),
    };
    format!(
        r#"<svg class="ic" viewBox="0 0 24 24" fill="{}" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{body}</svg>"#,
        if filled { "currentColor" } else { "none" }
    )
}

/// Rich-card tags for Discord, Slack, Telegram, X, iMessage, ... plus oEmbed
/// discovery and JSON-LD.
fn card_meta(p: &SharedPlaylist, songs: &[glyph_protocol::Track], link: &str, l: Lang) -> String {
    let (origin, _) = split_link(link);
    let title = escape(&title_of(p, l));
    let desc = escape(&description(p, songs, l));
    let og_type = match p.kind.as_str() { "album" => "music.album", "track" => "music.song", "artist" => "profile", _ => "music.playlist" };
    let enc: String = url::form_urlencoded::byte_serialize(link.as_bytes()).collect();
    let mut ld = serde_json::json!({
        "@context": "https://schema.org",
        "@type": if p.kind == "album" { "MusicAlbum" } else { "MusicPlaylist" },
        "name": title_of(p, l),
        "url": link,
        "image": format!("{link}/cover"),
        "numTracks": songs.len(),
        "track": songs.iter().take(50).map(|t| serde_json::json!({"@type": "MusicRecording", "name": t.title, "byArtist": {"@type": "MusicGroup", "name": t.artist}})).collect::<Vec<_>>(),
    });
    if let Some(a) = &p.author {
        ld["author"] = serde_json::json!({"@type": "Person", "name": format!("@{a}")});
    }
    // Inside <script>: no raw markup characters at all.
    let ld = ld.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026");
    let audio = if songs.is_empty() {
        String::new()
    } else {
        format!(r#"<meta property="og:audio" content="{link}/listen/0"><meta property="og:audio:type" content="audio/mpeg">"#, link = escape(link))
    };
    format!(
        r##"<meta name="description" content="{desc}">
<link rel="canonical" href="{link}">
<meta name="theme-color" content="#ff5fd2">
<meta property="og:site_name" content="Glyph"><meta property="og:type" content="{og_type}"><meta property="og:url" content="{link}"><meta property="og:locale" content="{locale}">
<meta property="og:title" content="{title}"><meta property="og:description" content="{desc}">
<meta property="og:image" content="{link}/cover"><meta property="og:image:alt" content="{alt}">{audio}
<meta name="twitter:card" content="summary"><meta name="twitter:title" content="{title}"><meta name="twitter:description" content="{desc}"><meta name="twitter:image" content="{link}/cover">
<link rel="alternate" type="application/json+oembed" href="{origin}/oembed?format=json&amp;url={enc}" title="{title}">
<script type="application/ld+json">{ld}</script>"##,
        link = escape(link),
        origin = escape(origin),
        locale = if l == Lang::De { "de_DE" } else { "en_US" },
        alt = escape(&tr(l, "cover_of").replace("{t}", &title_of(p, l))),
    )
}

/// The playlist cover like the app's `list_cover`: a mosaic of the first
/// songs' covers (4 = 2x2, 3 = one big + two small, 2 = halves), one cover
/// for albums and single songs.
fn art(p: &SharedPlaylist, count: usize, base: &str) -> String {
    let n = if p.kind == "album" { 1 } else { count.min(4) };
    let img = |i: usize, class: &str| format!(r#"<img class="{class}" src="{base}/cover/{i}" alt="" decoding="async">"#);
    match n {
        4 => format!(r#"<div class="art m4">{}{}{}{}</div>"#, img(0, "q0"), img(1, "q1"), img(2, "q2"), img(3, "q3")),
        3 => format!(r#"<div class="art m3">{}{}{}</div>"#, img(0, "q0"), img(1, "q1"), img(2, "q2")),
        2 => format!(r#"<div class="art m2">{}{}</div>"#, img(0, "q0"), img(1, "q1")),
        _ => format!(r#"<div class="art"><img class="a1" src="{base}/cover" alt="" decoding="async"></div>"#),
    }
}

/// The app's `song_row`: # (play on hover) · cover · title / artist · album ·
/// (heart) · time · menu.
fn song_rows(songs: &[glyph_protocol::Track], base: &str, l: Lang) -> String {
    let mut out = String::from(r#"<div class="songs" role="list">"#);
    for (n, t) in songs.iter().take(MAX_LISTED).enumerate() {
        out.push_str(&format!(
            r#"<div class="row" role="listitem" tabindex="0" data-n="{n}" data-title="{title}" data-artist="{artist}"><div class="num"><span>{num}</span>{play}</div><img class="cv" src="{base}/cover/{n}" alt="" loading="lazy" decoding="async"><div class="tx"><div class="tl">{e}<span class="tt trunc">{title}</span></div><div class="ar trunc">{artist}</div></div><div class="al trunc">{album}</div><div class="like-space"></div><div class="du">{d}</div><button class="more-btn" type="button" data-tip="{more}" aria-label="{more}">{dots}</button></div>"#,
            num = n + 1,
            play = ic("play"),
            e = if t.explicit { r#"<i class="e">E</i>"# } else { "" },
            title = escape(&t.title),
            artist = escape(&t.artist),
            album = escape(t.album.as_deref().unwrap_or_default()),
            d = duration(t.duration_ms),
            more = tr(l, "more_opts"),
            dots = ic("ellipsis-vertical"),
        ));
    }
    if songs.len() > MAX_LISTED {
        out.push_str(&format!(r#"<div class="tail">{}</div>"#, tr(l, "more").replace("{n}", &(songs.len() - MAX_LISTED).to_string())));
    }
    out.push_str("</div>");
    out
}

/// The app's player bar: now playing left (300 px), transport and seek row
/// centred on the bar, the pill with shuffle / repeat / volume right.
fn player_bar(l: Lang) -> String {
    let btn = |id: &str, icon: &str, tip: &str, class: &str| format!(r#"<button class="ib {class}" id="{id}" type="button" data-tip="{tip}" aria-label="{tip}">{}</button>"#, ic(icon));
    format!(
        r#"<div class="bar" id="bar"><div class="side l"><div class="now"><div class="ncv" id="ncv">{music}</div><div class="nt"><div class="t trunc" id="nt">{idle}</div><div class="a"><span class="trunc" id="na">{idle_sub}</span><span class="src mono" id="nb"></span><span class="pv mono" id="npv">[PREVIEW 0:30]</span></div></div></div></div>
<div class="center"><div class="tr">{prev}<button class="play" id="pp" type="button" aria-label="{play_t}"><i><span class="ip">{play}</span><span class="ia">{pause}</span></i></button>{next}</div>
<div class="seekrow"><span class="tm r" id="t0">0:00</span><div class="thin" id="seek"><span class="f"></span><span class="k"></span><span class="lbl"></span></div><span class="tm" id="t1">-:--</span></div></div>
<div class="side r"><div class="vpill">{shuf}{rep}<div class="vgroup">{mute}<div class="thin vol" id="vol"><span class="f" style="width:100%"></span><span class="k" style="left:100%"></span><span class="lbl"></span></div></div></div></div>
<span class="mini-prog" id="mprog"></span></div>"#,
        music = ic("music-2"),
        idle = tr(l, "idle_title"),
        idle_sub = tr(l, "idle_sub"),
        prev = btn("pv", "skip-back", tr(l, "prev"), ""),
        next = btn("nx", "skip-forward", tr(l, "next"), ""),
        play_t = tr(l, "play"),
        play = ic("play"),
        pause = ic("pause"),
        shuf = btn("bshuf", "shuffle", tr(l, "shuffle"), "s15"),
        rep = btn("brep", "repeat", tr(l, "repeat"), "s15"),
        mute = btn("mute", "volume-2", tr(l, "mute"), "s15"),
    )
}

/// Styles of the playlist page and the embed, built from the app's tokens.
pub(crate) const SHARE_CSS: &str = include_str!("site/share.css");
/// `glyphPlayer(base, count)`: plays `<base>/listen/<n>`, skips songs that
/// cannot be played on the web.
pub const PLAYER_JS: &str = include_str!("site/player.js");
/// `glyphGlow(glow, ascii, titlebarH)`: the app's background.
pub(crate) const GLOW_JS: &str = include_str!("site/glow.js");

/// Pages that play audio: covers and audio come from provider hosts.
const PLAYER_CSP: &str = "default-src 'none'; img-src 'self' data: https:; media-src 'self' https:; connect-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; base-uri 'none'; form-action 'none'";

const FONTS: &str = r#"<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin><link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;700&display=swap">"#;

const BG: &str = r#"<canvas class="bgfx" id="glow" aria-hidden="true"></canvas><canvas class="bgfx" id="ascii" aria-hidden="true"></canvas>"#;

/// `<html>` up to `<body ...>`; the strings the scripts show ride along as
/// data attributes.
fn head(l: Lang, title: &str, meta: &str, body_class: &str) -> String {
    format!(
        r#"<!doctype html><html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex"><meta name="color-scheme" content="dark"><title>{title}</title>
{meta}
<link rel="icon" href="/favicon.ico" sizes="any"><link rel="icon" type="image/png" href="/icon.png"><link rel="apple-touch-icon" href="/apple-touch-icon.png">
{FONTS}<style>{css}</style></head><body class="{body_class}" data-copied="{copied}" data-copied-song="{copied_song}" data-unplayable="{unplayable}" data-menu-play="{m_play}" data-menu-copy="{m_copy}" data-menu-link="{m_link}" data-repeat="{rep}" data-repeat-one="{rep1}" data-mute="{mute}" data-unmute="{unmute}">"#,
        lang = l.code(),
        css = SHARE_CSS,
        copied = tr(l, "copied"),
        copied_song = tr(l, "copied_song"),
        unplayable = tr(l, "unplayable"),
        m_play = tr(l, "menu_play"),
        m_copy = tr(l, "menu_copy_song"),
        m_link = tr(l, "copy"),
        rep = tr(l, "repeat"),
        rep1 = tr(l, "repeat_one"),
        mute = tr(l, "mute"),
        unmute = tr(l, "unmute"),
    )
}

/// Every page response varies with the visitor's language.
pub(crate) fn localized(mut r: Response, l: Lang) -> Response {
    r.headers.push(("vary".into(), "Accept-Language".into()));
    r.headers.push(("content-language".into(), l.code().into()));
    r
}

fn scripts(base_js: &str, count: usize, titlebar: u32) -> String {
    format!(
        r#"<div class="toast" id="toast" role="status">{check}<span></span></div><div class="tip" id="tip" role="tooltip"></div>
<script>{GLOW_JS}
{PLAYER_JS}
glyphGlow(document.getElementById("glow"),document.getElementById("ascii"),{titlebar});
glyphPlayer({base_js},{count});
</script>"#,
        check = ic("check"),
    )
}

// ---- special pages ---------------------------------------------------------

/// A full-page state: the app's empty / error state (`ascii_state`), centred.
struct State<'a> {
    status: u16,
    /// Shown instead of the status in the `[ … ]` tag.
    tag: Option<&'a str>,
    icon: &'a str,
    title: &'a str,
    sub: &'a str,
    /// (icon, label, href); the first is the white pill, the rest back pills.
    actions: &'a [(&'a str, &'a str, &'a str)],
}

fn state_html(l: Lang, s: &State) -> String {
    let actions: String = s
        .actions
        .iter()
        .enumerate()
        .map(|(i, (icon, label, href))| format!(r#"<a class="{}" href="{}">{}{}</a>"#, if i == 0 { "pill" } else { "backpill" }, escape(href), ic(icon), escape(label)))
        .collect();
    format!(
        r#"{head}{BG}<header class="titlebar"><a class="logo" href="/">glyph</a><a class="backpill" href="/download">{dl_ic}{dl}</a></header>
<main class="state"><div class="state-box"><div class="state-row"><div class="state-ic">{icon}</div><div class="state-txt"><span class="tag mono">[ {status} ]</span><h1>{title}</h1><p>{sub}</p></div></div><div class="state-act">{actions}</div></div></main>
<script>{GLOW_JS}
glyphGlow(document.getElementById("glow"),document.getElementById("ascii"),44);
</script></body></html>"#,
        head = head(l, &format!("{} · Glyph", escape(s.title)), "", "page"),
        dl_ic = ic("download"),
        dl = tr(l, "download"),
        icon = ic(s.icon),
        status = s.tag.map_or_else(|| s.status.to_string(), str::to_string),
        title = escape(s.title),
        sub = escape(s.sub),
    )
}

/// The page a browser gets instead of a JSON error: 404, sign-in needed,
/// rate limited, server busy, ... `code` is the error code of the JSON body.
pub fn error_page(status: u16, code: &str, retry_after_s: Option<u64>, l: Lang) -> Response {
    let de = l == Lang::De;
    let t = |en: &'static str, de_: &'static str| if de { de_ } else { en };
    let (icon, title, sub) = match (status, code) {
        (404, _) => ("search", t("Page not found", "Seite nicht gefunden"), t("This page does not exist or was moved.", "Diese Seite gibt es nicht oder sie wurde verschoben.")),
        (401, "account_required") => ("user-round", t("Sign in required", "Anmeldung nötig"), t("This server only works with a Glyph account. Sign in in the app.", "Dieser Server funktioniert nur mit einem Glyph-Konto. Melde dich in der App an.")),
        (401, _) => ("server", t("Private server", "Privater Server"), t("This Glyph server needs an access token. Open it in the Glyph app.", "Dieser Glyph-Server braucht ein Zugangstoken. Öffne ihn in der Glyph-App.")),
        (403, "https_required") => ("server", t("HTTPS required", "HTTPS nötig"), t("Open this address with https://.", "Öffne diese Adresse mit https://.")),
        (403, _) => ("server", t("No access", "Kein Zugriff"), t("You cannot open this page.", "Diese Seite kannst du nicht öffnen.")),
        (429, _) => ("clock", t("Too many requests", "Zu viele Anfragen"), t("Wait a moment, then try again.", "Warte kurz und versuch es dann noch mal.")),
        (503, _) => ("refresh-cw", t("Server busy", "Server ausgelastet"), t("Glyph is busy right now. Try again in a minute.", "Glyph ist gerade ausgelastet. Versuch es in einer Minute noch mal.")),
        (500.., _) => ("refresh-cw", t("Something went wrong", "Etwas ist schiefgelaufen"), t("That did not work. Try again in a minute.", "Das hat nicht geklappt. Versuch es in einer Minute noch mal.")),
        _ => ("music-2", t("That did not work", "Das hat nicht geklappt"), t("The server could not handle this request.", "Der Server konnte diese Anfrage nicht bearbeiten.")),
    };
    let sub = match retry_after_s.filter(|_| status == 429) {
        Some(n) => if de { format!("Versuch es in {n} s noch mal.") } else { format!("Try again in {n} s.") },
        None => sub.to_string(),
    };
    let home = ("house", t("Glyph home", "Zur Startseite"), "/");
    let actions = if status == 429 || status >= 500 {
        // An empty href reloads the address the visitor asked for.
        vec![("refresh-cw", t("Try again", "Nochmal versuchen"), ""), home]
    } else {
        vec![home, ("book-open", t("Docs", "Doku"), "https://docs.glyph.my")]
    };
    let html = state_html(l, &State { status, tag: None, icon, title, sub: &sub, actions: &actions });
    let mut r = localized(crate::site::html_response_csp(status, html, "no-store", PLAYER_CSP, false), l);
    if let Some(n) = retry_after_s {
        r.headers.push(("retry-after".into(), n.to_string()));
    }
    r
}

/// `/jam/<code>`: an invite to a jam (the QR code in the app points here).
/// It opens the app on the jam (`glyph://jam/<code>`); without the app it
/// offers the download. `host` = `None`: the jam is over.
pub fn jam_page(code: &str, host: Option<&str>, l: Lang) -> Response {
    let de = l == Lang::De;
    let Some(host) = host else {
        let (title, sub) = if de {
            ("Jam beendet", "Diesen Jam gibt es nicht mehr, oder der Code stimmt nicht.")
        } else {
            ("Jam ended", "This Jam is over, or the code is wrong.")
        };
        let html = state_html(l, &State { status: 404, tag: None, icon: "users", title, sub, actions: &[("house", tr(l, "what"), "/")] });
        return localized(crate::site::html_response_csp(404, html, "no-store", PLAYER_CSP, false), l);
    };
    let (title, sub, open) = if de {
        (format!("Jam von @{host}"), format!("Hör mit @{host} zusammen: gleiche Songs, gleiche Stelle. Code {code}"), "In Glyph beitreten")
    } else {
        (format!("@{host}'s Jam"), format!("Listen together with @{host}: same songs, same moment. Code {code}"), "Join in Glyph")
    };
    let app = format!("glyph://jam/{code}");
    let html = state_html(
        l,
        &State { status: 200, tag: Some("JAM"), icon: "users", title: &title, sub: &sub, actions: &[("play", open, &app), ("download", tr(l, "download"), "/download")] },
    );
    // Straight into the app when it is installed (link previews stay put).
    let html = html.replacen(
        "</body>",
        "<script>if(!/bot|crawler|spider|preview/i.test(navigator.userAgent))location.href=document.querySelector('.state-act a').href;</script></body>",
        1,
    );
    localized(crate::site::html_response_csp(200, html, "no-store", PLAYER_CSP, false), l)
}

/// Page for a link opened in a browser, laid out like the app's list view:
/// titlebar, cover column left, songs right, the player bar at the bottom.
/// `found` = `None` renders "not found".
pub fn page(found: Option<(&SharedPlaylist, &str)>, l: Lang) -> Response {
    let titlebar = format!(
        r#"<header class="titlebar"><a class="logo" href="/">glyph</a><a class="backpill" href="/download">{}{}</a></header>"#,
        ic("download"),
        tr(l, "download")
    );
    let Some((p, link)) = found else {
        let html = state_html(l, &State { status: 404, tag: None, icon: "link-2", title: tr(l, "nf_title"), sub: tr(l, "nf_sub"), actions: &[("house", tr(l, "what"), "/")] });
        return localized(crate::site::html_response_csp(404, html, "no-store", PLAYER_CSP, false), l);
    };
    let songs = songs(&p.code);
    let count = songs.len().min(MAX_LISTED);
    let base = escape(link);
    let title = escape(&title_of(p, l));
    let by = p.author.as_ref().map(|a| format!(r#"<span class="by">{}</span>"#, escape(&tr(l, "shared_by").replace("{h}", a)))).unwrap_or_default();
    let actions = if count > 0 {
        format!(
            r#"<div class="acts"><button class="pill" id="all" type="button">{play}{all}</button><button class="round" id="shuf" type="button" data-tip="{shuf}" aria-label="{shuf}">{shuffle}</button></div>"#,
            play = ic("play"),
            all = tr(l, "play_all"),
            shuf = tr(l, "shuffle"),
            shuffle = ic("shuffle"),
        )
    } else {
        String::new()
    };
    let html = format!(
        r#"{head}{BG}{titlebar}
<main class="list"><aside class="left">{art}<div class="info"><span class="kind mono">[{kind}]</span><h1 class="title">{title}</h1><div class="subrow">{by}<span class="meta mono">{meta}</span></div></div>
<a class="pill" id="open-app" href="glyph://p/{share_id}">{open_app}</a>{actions}<div class="acts2"><button class="ib" id="cp" type="button" data-tip="{copy}" aria-label="{copy}" data-link="{base}">{link_ic}</button><a class="ib" href="/download" data-tip="{dl}" aria-label="{dl}">{dl_ic}</a></div></aside>
<section class="songs-col">{rows}</section></main>
{bar}{js}</body></html>"#,
        share_id = escape(split_link(link).1),
        open_app = if l == Lang::De { "In Glyph öffnen" } else { "Open in Glyph" },
        head = head(l, &format!("{title} · Glyph"), &card_meta(p, &songs, link, l), "page"),
        art = art(p, songs.len(), &base),
        kind = kind_of(p, l).to_uppercase(),
        meta = meta_line(&songs),
        copy = tr(l, "copy"),
        link_ic = ic("link-2"),
        dl = tr(l, "download_short"),
        dl_ic = ic("download"),
        rows = song_rows(&songs, &base, l),
        bar = if count > 0 { player_bar(l) } else { String::new() },
        js = format!("{}<script>if(!/bot|crawler|spider|preview/i.test(navigator.userAgent))location.href=document.getElementById('open-app').href;</script>", scripts(r#"location.pathname.replace(/\/$/,"")"#, count, 44)),
    );
    // Links die with their account: never cache them.
    localized(crate::site::html_response_csp(200, html, "no-store", PLAYER_CSP, false), l)
}

/// `/embed/p/<id>`: a small player to put in an `<iframe>` (oEmbed, blogs,
/// forums): the list header, the app's mini player and the songs. Any site
/// may frame it.
pub fn embed(found: Option<(&SharedPlaylist, &str)>, l: Lang) -> Response {
    let Some((p, link)) = found else {
        let html = format!(
            r#"{head}<div class="frame">{BG}<div class="empty">{music}<h1>{t}</h1><p>{s}</p></div></div>{js}</body></html>"#,
            head = head(l, "Glyph", "", "embed"),
            music = ic("music-2"),
            t = tr(l, "nf_title"),
            s = tr(l, "nf_sub"),
            js = scripts("\"\"", 0, 0),
        );
        return localized(crate::site::html_response_csp(404, html, "no-store", PLAYER_CSP, true), l);
    };
    let songs = songs(&p.code);
    let count = songs.len().min(MAX_LISTED);
    let base = escape(link);
    let by = p.author.as_ref().map(|a| format!(r#"<span class="by">{}</span>"#, escape(&tr(l, "shared_by").replace("{h}", a)))).unwrap_or_default();
    let html = format!(
        r#"{head}<div class="frame">{BG}<header class="ehd">{art}<div class="info"><span class="kind mono">[{kind}]</span><div class="title">{title}</div><div class="subrow">{by}<span class="meta mono">{meta}</span></div></div><a class="logo" href="{base}" target="_blank" rel="noopener">glyph</a></header>
{bar}<div class="elist">{rows}</div></div>{js}</body></html>"#,
        head = head(l, &format!("{} · Glyph", escape(&title_of(p, l))), "", "embed"),
        art = art(p, songs.len(), &base),
        kind = kind_of(p, l).to_uppercase(),
        title = escape(&title_of(p, l)),
        meta = meta_line(&songs),
        bar = if count > 0 { player_bar(l) } else { String::new() },
        rows = song_rows(&songs, &base, l),
        js = scripts(r#"location.pathname.replace(/^\/embed/,"").replace(/\/$/,"")"#, count, 0),
    );
    localized(crate::site::html_response_csp(200, html, "no-store", PLAYER_CSP, true), l)
}

/// oEmbed (`/oembed?url=<link>`): lets sites and chat apps that speak it
/// show the embed player.
pub fn oembed(p: &SharedPlaylist, link: &str, l: Lang) -> serde_json::Value {
    let (origin, _) = split_link(link);
    serde_json::json!({
        "version": "1.0",
        "type": "rich",
        "provider_name": "Glyph",
        "provider_url": origin,
        "title": title_of(p, l),
        "author_name": p.author.as_ref().map(|a| format!("@{a}")),
        "html": format!(r#"<iframe src="{}" width="100%" height="420" frameborder="0" allow="autoplay; encrypted-media" loading="lazy" title="{} · Glyph"></iframe>"#, escape(&embed_link(link)), escape(&title_of(p, l))),
        "width": 480,
        "height": 420,
        "thumbnail_url": format!("{link}/cover"),
        "thumbnail_width": 500,
        "thumbnail_height": 500,
        "cache_age": 3600,
    })
}

/// The share id in an oEmbed `url` (`https://<host>/p/<id>`, `/embed/p/<id>`).
pub fn id_from_url(url: &str) -> Option<String> {
    let path = url.trim().split(['?', '#']).next()?;
    let id = path.trim_end_matches('/').rsplit_once("/p/")?.1;
    valid_id(id).then(|| id.to_string())
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
        assert_eq!(clean_kind(Some("track")), "track");
        assert_eq!(clean_kind(Some("artist")), "artist");
        assert_eq!(clean_kind(Some("x")), "playlist");
    }

    #[test]
    fn page_escapes() {
        let p = SharedPlaylist { code: String::new(), name: "Road <trip>".into(), songs: 2, kind: "playlist".into(), author: Some("gisela".into()) };
        let r = page(Some((&p, "https://glyph.my/p/AbC123xy")), Lang::En);
        let html = String::from_utf8_lossy(r.bytes().unwrap()).to_string();
        assert!(html.contains("Road &lt;trip&gt;") && !html.contains("<trip>") && html.contains("@gisela"));
        assert!(html.contains("glyph://p/AbC123xy"));
        assert!(html.contains("Open in Glyph"));
        assert!(html.contains("location.href=document.getElementById('open-app').href"));
        assert_eq!(page(None, Lang::En).status, 404);
    }

    #[test]
    fn cards_and_embeds() {
        // The code from `accounts/tests.rs`, made by glyph-core's `share::encode`.
        let code = "glyph:playlist:q1YqU7Iy1FHKU7JSCshJrMzJLC5RMFTSUSpRsoquBpFKESHOQH4i";
        let p = SharedPlaylist { code: code.into(), name: "Mix".into(), songs: 0, kind: "playlist".into(), author: Some("gisela".into()) };
        let link = "https://glyph.my/p/AbC123xy";
        let html = String::from_utf8_lossy(page(Some((&p, link)), Lang::En).bytes().unwrap()).to_string();
        assert!(html.contains(r#"property="og:image" content="https://glyph.my/p/AbC123xy/cover""#));
        assert!(html.contains(r#"content="music.playlist""#) && html.contains("application/json+oembed") && html.contains("application/ld+json"));
        assert_eq!(embed_link(link), "https://glyph.my/embed/p/AbC123xy");
        assert_eq!(id_from_url("https://glyph.my/p/AbC123xy?x=1").as_deref(), Some("AbC123xy"));
        assert_eq!(id_from_url("https://glyph.my/embed/p/AbC123xy/").as_deref(), Some("AbC123xy"));
        assert_eq!(id_from_url("https://glyph.my/p/bad"), None);
        let o = oembed(&p, link, Lang::En);
        assert_eq!(o["type"], "rich");
        assert!(o["html"].as_str().unwrap().contains("/embed/p/AbC123xy"));
        assert!(!embed(Some((&p, link)), Lang::En).headers.iter().any(|(k, _)| k == "x-frame-options"));
        let de = String::from_utf8_lossy(page(Some((&p, link)), Lang::De).bytes().unwrap()).to_string();
        assert!(de.contains(r#"<html lang="de">"#) && de.contains("Geteilt von @gisela"));
        assert!(songs("glyph:playlist:@@@").is_empty());
    }

    #[test]
    fn picks_the_visitors_language() {
        assert_eq!(lang(None), Lang::En);
        assert_eq!(lang(Some("de-DE,de;q=0.9,en;q=0.8")), Lang::De);
        assert_eq!(lang(Some("fr-FR,fr;q=0.9,de;q=0.7,en;q=0.8")), Lang::En);
        assert_eq!(lang(Some("en;q=0.5, de-AT")), Lang::De);
        assert_eq!(lang(Some("ru")), Lang::En);
    }
}
