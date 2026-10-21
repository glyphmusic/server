//! The public site on the share origin (`https://glyph.my`): the landing page
//! at `/`, `/download`, `/changelog` (every release of every repository), the
//! docs (`docs.rs`, on `docs.*` hosts and `/docs`), and
//! what crawlers and chat apps look for (robots.txt, sitemap.xml, llms.txt, icons, the link-card image). The `/p/` playlist page
//! (`share.rs`) uses the same stylesheet and ASCII note.

pub mod changelog;
pub mod docs;

use crate::error::ApiErr;
use crate::server::{Body, Request, Response};
use crate::traits::Method;

/// Stylesheet shared by every page of the site.
pub const BASE_CSS: &str = include_str!("site/base.css");
/// `glyphNote(canvas, rowH)`: the app's splash animation.
pub const NOTE_JS: &str = include_str!("site/note.js");
/// The app icon, shown next to the brand name.
const ICON_PNG: &[u8] = include_bytes!("site/icon-64.png");

/// `url(data:...)` of the app icon for CSS.
pub fn icon_css_url() -> String {
    use base64::Engine;
    format!("url(data:image/png;base64,{})", base64::engine::general_purpose::STANDARD.encode(ICON_PNG))
}

/// CSS for the brand link: the app icon before the name.
pub fn brand_icon_css(sel: &str) -> String {
    format!("{sel}::before{{content:\"\";display:inline-block;width:1.6em;height:1.6em;margin-right:.5em;vertical-align:-.45em;border-radius:.25em;background:{} center/contain no-repeat}}", icon_css_url())
}

/// CSP for the site's HTML pages: inline script and style, the web font.
pub const PAGE_CSP: &str = "default-src 'none'; img-src 'self' data:; manifest-src 'self'; script-src 'unsafe-inline'; connect-src https://api.github.com; style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Paths owned by this module (`path` without its trailing `/`).
pub fn owns(path: &str) -> bool {
    path.is_empty() || path == "/download" || path == "/changelog" || shot(path).is_some() || file(path).is_some()
}

/// Files at fixed paths: (path, content type, cache seconds, bytes). Text
/// files have `{ORIGIN}` replaced by the origin the request came to.
const FILES: &[(&str, &str, u32, &[u8])] = &[
    ("/robots.txt", "text/plain; charset=utf-8", 3600, include_bytes!("site/robots.txt")),
    ("/sitemap.xml", "application/xml; charset=utf-8", 3600, include_bytes!("site/sitemap.xml")),
    ("/llms.txt", "text/markdown; charset=utf-8", 3600, include_bytes!("site/llms.txt")),
    ("/.well-known/security.txt", "text/plain; charset=utf-8", 86400, include_bytes!("site/security.txt")),
    ("/site.webmanifest", "application/manifest+json", 86400, include_bytes!("site/site.webmanifest")),
    ("/og.png", "image/png", 86400, include_bytes!("site/og.png")),
    ("/favicon.ico", "image/x-icon", 604800, include_bytes!("site/glyph.ico")),
    ("/icon.png", "image/png", 604800, include_bytes!("site/icon-512.png")),
    ("/apple-touch-icon.png", "image/png", 604800, include_bytes!("site/icon-256.png")),
];

fn file(path: &str) -> Option<&'static (&'static str, &'static str, u32, &'static [u8])> {
    FILES.iter().find(|f| f.0 == path)
}

/// `https://<host>` of the request, `https://glyph.my` without a usable Host.
pub(crate) fn origin(req: &Request) -> String {
    match req.header("host").filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))) {
        Some(h) if req.local && !req.secure => format!("http://{h}"),
        Some(h) => format!("https://{h}"),
        None => "https://glyph.my".into(),
    }
}

/// Screenshots of the real app (demo profile, `GLYPH_DEMO=1`) on the landing page.
const SHOTS: &[(&str, &[u8])] = &[
    ("/shots/home.jpg", include_bytes!("site/shots/home.jpg")),
    ("/shots/lyrics.jpg", include_bytes!("site/shots/lyrics.jpg")),
    ("/shots/list.jpg", include_bytes!("site/shots/list.jpg")),
    ("/shots/search.jpg", include_bytes!("site/shots/search.jpg")),
];

fn shot(path: &str) -> Option<&'static [u8]> {
    SHOTS.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
}

pub fn handle(req: &Request) -> Result<Response, ApiErr> {
    if req.method != Method::Get {
        return Err(ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint"));
    }
    if let Some(bytes) = shot(req.path.trim_end_matches('/')) {
        return Ok(Response {
            status: 200,
            headers: vec![
                ("content-type".into(), "image/jpeg".into()),
                ("cache-control".into(), "public, max-age=86400".into()),
                ("x-content-type-options".into(), "nosniff".into()),
            ],
            body: Body::Bytes(bytes.to_vec()),
        });
    }
    if let Some((path, mime, max_age, bytes)) = file(req.path.trim_end_matches('/')) {
        let body = if mime.starts_with("image/") {
            bytes.to_vec()
        } else {
            let o = origin(req);
            let host = o.split_once("://").map_or(o.as_str(), |x| x.1).to_string();
            String::from_utf8_lossy(bytes).replace("{ORIGIN_HOST}", &host).replace("{ORIGIN}", &o).into_bytes()
        };
        let mut headers = vec![
            ("content-type".into(), (*mime).into()),
            ("cache-control".into(), format!("public, max-age={max_age}")),
            ("x-content-type-options".into(), "nosniff".into()),
        ];
        if *path == "/llms.txt" || *path == "/og.png" {
            headers.push(("access-control-allow-origin".into(), "*".into()));
        }
        return Ok(Response { status: 200, headers, body: Body::Bytes(body) });
    }
    if req.path.trim_end_matches('/') == "/changelog" {
        let releases = changelog::releases();
        let html = include_str!("site/changelog.html")
            .replace("/*BASE*/", BASE_CSS)
            .replace("/*BRAND*/", &brand_icon_css("nav .brand"))
            .replace("<!--COUNT-->", &format!("{} releases.", releases.len()))
            .replace("<!--VERSIONS-->", &changelog::render_index(&releases))
            .replace("<!--RELEASES-->", &changelog::render(&releases));
        return Ok(html_response(200, html, "public, max-age=300"));
    }
    if req.path.trim_end_matches('/') == "/download" {
        let html = include_str!("site/download.html").replace("/*BASE*/", BASE_CSS).replace("/*BRAND*/", &brand_icon_css("nav .brand"));
        return Ok(html_response(200, html, "public, max-age=300"));
    }
    let html = include_str!("site/index.html").replace("/*BASE*/", BASE_CSS).replace("/*NOTE*/", NOTE_JS).replace("/*BRAND*/", &brand_icon_css("nav .brand"));
    Ok(html_response(200, html, "public, max-age=300"))
}

/// API prefixes whose errors stay JSON even in a browser.
const API_PREFIXES: &[&str] = &["/v1/", "/admin", "/internal/", "/oauth/", "/mcp", "/.well-known/oauth-"];

/// `Some(language)` when a failed request should get a special page instead
/// of the JSON error: a browser (asks for HTML) loading a page, not an API call.
pub fn page_lang(req: &Request) -> Option<crate::share::Lang> {
    let html = req.header("accept").is_some_and(|a| a.contains("text/html"));
    (req.method == Method::Get && html && !API_PREFIXES.iter().any(|p| req.path.starts_with(p)))
        .then(|| crate::share::lang(req.header("accept-language")))
}

/// Swaps a JSON error for the matching special page (`share::error_page`)
/// when [`page_lang`] said the visitor is a browser.
pub fn as_page(lang: Option<crate::share::Lang>, r: Response) -> Response {
    let json = r.headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("application/json"));
    let Some(l) = lang.filter(|_| r.status >= 400 && json) else { return r };
    let err: Option<glyph_protocol::ApiError> = r.bytes().and_then(|b| serde_json::from_slice(b).ok());
    let code = err.as_ref().map_or("", |e| e.code.as_str());
    crate::share::error_page(r.status, code, err.as_ref().and_then(|e| e.retry_after_s), l)
}

/// An HTML page of the site with its security headers.
pub fn html_response(status: u16, html: String, cache: &str) -> Response {
    html_response_csp(status, html, cache, PAGE_CSP, false)
}

/// Like [`html_response`] with its own CSP; `framable` lets any site put the
/// page in an iframe (the embed player).
pub fn html_response_csp(status: u16, html: String, cache: &str, csp: &str, framable: bool) -> Response {
    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "text/html; charset=utf-8".into()),
        ("cache-control".into(), cache.into()),
        ("x-content-type-options".into(), "nosniff".into()),
        ("referrer-policy".into(), "no-referrer".into()),
    ];
    if framable {
        headers.push(("content-security-policy".into(), format!("{csp}; frame-ancestors *")));
    } else {
        headers.push(("x-frame-options".into(), "DENY".into()));
        let csp = if csp.contains("frame-ancestors") { csp.to_string() } else { format!("{csp}; frame-ancestors 'none'") };
        headers.push(("content-security-policy".into(), csp));
    }
    Response { status, headers, body: Body::Bytes(html.into_bytes()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn landing_page() {
        let h = harness(Default::default());
        let s = Server::from_arc(h.svc.clone());
        let get = |p: &str| Request::new(Method::Get, p, vec![], vec![], Some("1.2.3.4".into()));
        let r = block_on(s.handle(get("/")));
        let html = String::from_utf8_lossy(r.bytes().unwrap()).to_string();
        assert_eq!(r.status, 200);
        assert!(html.contains("function glyphNote") && html.contains("--pink") && !html.contains("/*NOTE*/"));
        assert_eq!(block_on(s.handle(get("/shots/home.webp"))).status, 404);
        let shot = block_on(s.handle(get("/shots/home.jpg")));
        assert_eq!(shot.status, 200);
        let log = block_on(s.handle(get("/changelog")));
        let log_html = String::from_utf8_lossy(log.bytes().unwrap()).to_string();
        assert_eq!(log.status, 200);
        assert!(log_html.contains("data-repo=\"r-dlp\"") && !log_html.contains("<!--RELEASES-->") && !log_html.contains("/*BASE*/"));
        let dl = block_on(s.handle(get("/download")));
        let dl_html = String::from_utf8_lossy(dl.bytes().unwrap()).to_string();
        assert_eq!(dl.status, 200);
        assert!(dl_html.contains("releases/latest") && !dl_html.contains("/*BASE*/"));
        assert!(shot.bytes().unwrap().starts_with(&[0xff, 0xd8]));
    }

    #[test]
    fn special_pages_for_browsers() {
        let h = harness(Default::default());
        let s = Server::from_arc(h.svc.clone());
        let browser = |p: &str, lang: &str| Request::new(Method::Get, p, vec![("accept".into(), "text/html,*/*;q=0.8".into()), ("accept-language".into(), lang.into())], vec![], Some("1.2.3.4".into()));
        let r = block_on(s.handle(browser("/nope", "en")));
        let html = String::from_utf8_lossy(r.bytes().unwrap()).to_string();
        assert_eq!(r.status, 404);
        assert!(r.headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("text/html")));
        assert!(html.contains("Page not found") && html.contains("[ 404 ]") && html.contains("glyphGlow") && html.contains("noindex"));
        let de = String::from_utf8_lossy(block_on(s.handle(browser("/nope", "de-DE"))).bytes().unwrap()).to_string();
        assert!(de.contains("Seite nicht gefunden") && de.contains(r#"<html lang="de">"#));
        // API calls and non-browsers keep the JSON error.
        let api = block_on(s.handle(browser("/v1/nope", "en")));
        assert!(api.headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("application/json")));
        let curl = block_on(s.handle(Request::new(Method::Get, "/nope", vec![], vec![], Some("1.2.3.4".into()))));
        assert!(curl.headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("application/json")));
        let busy = crate::share::error_page(429, "rate_limited", Some(7), crate::share::Lang::En);
        let busy_html = String::from_utf8_lossy(busy.bytes().unwrap()).to_string();
        assert!(busy_html.contains("Try again in 7 s.") && busy.headers.iter().any(|(k, v)| k == "retry-after" && v == "7"));
        assert!(String::from_utf8_lossy(crate::share::error_page(503, "queue_full", None, crate::share::Lang::En).bytes().unwrap()).contains("Server busy"));
    }

    #[test]
    fn crawler_files() {
        let h = harness(Default::default());
        let s = Server::from_arc(h.svc.clone());
        let get = |p: &str| Request::new(Method::Get, p, vec![("host".into(), "glyph.my".into())], vec![], Some("1.2.3.4".into())).with_transport(true, false);
        let text = |p: &str| {
            let r = block_on(s.handle(get(p)));
            assert_eq!(r.status, 200, "{p}");
            String::from_utf8_lossy(r.bytes().unwrap()).to_string()
        };
        let robots = text("/robots.txt");
        assert!(robots.contains("Sitemap: https://glyph.my/sitemap.xml") && robots.contains("Disallow: /v1/"));
        assert!(text("/sitemap.xml").contains("<loc>https://glyph.my/download</loc>"));
        assert!(text("/llms.txt").starts_with("# Glyph") && !text("/llms.txt").contains("{ORIGIN}"));
        assert!(text("/.well-known/security.txt").contains("Contact:"));
        assert!(text("/site.webmanifest").contains("\"name\": \"Glyph\""));
        for p in ["/og.png", "/icon.png", "/apple-touch-icon.png"] {
            assert!(block_on(s.handle(get(p))).bytes().unwrap().starts_with(&[0x89, b'P', b'N', b'G']), "{p}");
        }
        assert_eq!(block_on(s.handle(get("/favicon.ico"))).status, 200);
        let home = text("/");
        assert!(home.contains("og:image") && home.contains("application/ld+json") && home.contains("rel=\"manifest\""));
    }
}
