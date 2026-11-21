//! The web player at `/web` on the share origin (glyph.my/web): Glyph's own
//! UI (`glyph-apps/glyph-ui`, `npm run build:web`) running in the browser.
//! The host keeps the built files (the Worker in its static assets, the
//! native server in `web_dir`) and asks this module what to serve and with
//! which headers. It is a single-page app: every page address under `/web/`
//! that is not a file gets `index.html`, and the UI opens the page itself.
//!
//! The player only ever plays audio straight from the providers (and YouTube
//! through its official embed); the CSP below allows those hosts, never an
//! audio route of this server is used by it.

/// Where the web player lives.
pub const PREFIX: &str = "/web";

/// The app page every page address falls back to.
pub const INDEX: &str = "index.html";

/// CSP of the web player's page. Audio and pictures come from many provider
/// hosts (Audius content nodes are run by many operators), so `media-src` and
/// `img-src` allow any HTTPS host; scripts, frames and API calls are pinned:
/// this origin, YouTube's IFrame API and its privacy embed host.
pub const WEB_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval' https://www.youtube.com https://s.ytimg.com; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https:; media-src https: blob:; connect-src 'self'; frame-src https://www.youtube-nocookie.com https://www.youtube.com; font-src 'self' data:; manifest-src 'self'; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; form-action 'none'; frame-ancestors 'none'";

/// What a request under [`PREFIX`] gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `/web` without the slash: redirect here, so relative paths resolve.
    Redirect(&'static str),
    /// A file of the built player, relative to its folder (no leading `/`).
    File(String),
}

/// Paths owned by the web player (`path` as requested).
pub fn owns(path: &str) -> bool {
    path == PREFIX || path.starts_with("/web/")
}

/// The file for `path`, or `None` when it is not under `/web` or not a safe
/// path (no `..`, backslashes, control characters or empty segments).
pub fn target(path: &str) -> Option<Target> {
    if path == PREFIX {
        return Some(Target::Redirect("/web/"));
    }
    let rest = path.strip_prefix("/web/")?;
    if rest.is_empty() {
        return Some(Target::File(INDEX.into()));
    }
    let rest = rest.trim_end_matches('/');
    let bad = rest.split('/').any(|s| s.is_empty() || s == "." || s == "..")
        || rest.bytes().any(|b| b == b'\\' || b == b'%' || b.is_ascii_control() || b == b':');
    if bad {
        // Encoded or odd page addresses (`/web/artist/AC%2FDC`) are pages, not files.
        return Some(Target::File(INDEX.into()));
    }
    Some(Target::File(rest.to_string()))
}

/// The file to try when `file` is missing: the app page for page addresses,
/// nothing for the built assets (a missing script is a real 404).
pub fn fallback(file: &str) -> Option<&'static str> {
    (file != INDEX && !file.starts_with("assets/")).then_some(INDEX)
}

/// Content type by file name.
pub fn content_type(file: &str) -> &'static str {
    let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" | "toml" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Response headers for `file`. The page, service worker and manifest are
/// checked on every visit (a new build shows at once); the built assets have
/// their hash in the name and never change; anything else is kept for an hour.
pub fn headers(file: &str) -> Vec<(String, String)> {
    let cache = if file == INDEX || file.ends_with(".html") || file == "sw.js" || file.ends_with(".webmanifest") {
        "no-cache"
    } else if file.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    let mut h: Vec<(String, String)> = vec![
        ("content-type".into(), content_type(file).into()),
        ("cache-control".into(), cache.into()),
        ("x-content-type-options".into(), "nosniff".into()),
        // YouTube's embed needs the page's origin as referrer; nothing more leaves.
        ("referrer-policy".into(), "strict-origin-when-cross-origin".into()),
    ];
    if content_type(file).starts_with("text/html") {
        h.push(("content-security-policy".into(), WEB_CSP.into()));
        h.push(("x-frame-options".into(), "DENY".into()));
        h.push(("permissions-policy".into(), "camera=(), microphone=(), geolocation=(), autoplay=(self \"https://www.youtube-nocookie.com\" \"https://www.youtube.com\")".into()));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_paths_to_files() {
        assert!(owns("/web") && owns("/web/") && owns("/web/search") && !owns("/webx") && !owns("/"));
        assert_eq!(target("/web"), Some(Target::Redirect("/web/")));
        assert_eq!(target("/web/"), Some(Target::File("index.html".into())));
        assert_eq!(target("/web/assets/index-abc.js"), Some(Target::File("assets/index-abc.js".into())));
        assert_eq!(target("/web/artist/Tame Impala/"), Some(Target::File("artist/Tame Impala".into())));
        assert_eq!(target("/web/../secret"), Some(Target::File("index.html".into())));
        assert_eq!(target("/web/a//b"), Some(Target::File("index.html".into())));
        assert_eq!(target("/web/artist/AC%2FDC"), Some(Target::File("index.html".into())));
        assert_eq!(target("/other"), None);
    }

    #[test]
    fn page_addresses_fall_back_to_the_app() {
        assert_eq!(fallback("artist/Mr. Oizo"), Some(INDEX));
        assert_eq!(fallback("search"), Some(INDEX));
        assert_eq!(fallback("assets/missing.js"), None);
        assert_eq!(fallback(INDEX), None);
    }

    #[test]
    fn headers_cache_and_protect() {
        let get = |h: &[(String, String)], k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let page = headers(INDEX);
        assert_eq!(get(&page, "cache-control").as_deref(), Some("no-cache"));
        let csp = get(&page, "content-security-policy").unwrap();
        assert!(csp.contains("frame-src https://www.youtube-nocookie.com") && csp.contains("media-src https:") && csp.contains("connect-src 'self'") && csp.contains("'wasm-unsafe-eval'"));
        let js = headers("assets/index-abc.js");
        assert_eq!(get(&js, "content-type").as_deref(), Some("text/javascript; charset=utf-8"));
        assert!(get(&js, "cache-control").unwrap().contains("immutable"));
        assert!(get(&js, "content-security-policy").is_none());
        let sw = headers("sw.js");
        assert_eq!(get(&sw, "cache-control").as_deref(), Some("no-cache"));
        assert_eq!(get(&sw, "content-type").as_deref(), Some("text/javascript; charset=utf-8"));
        let manifest = headers("manifest.webmanifest");
        assert_eq!(get(&manifest, "cache-control").as_deref(), Some("no-cache"));
        assert_eq!(content_type("assets/x.woff2"), "font/woff2");
    }
}
