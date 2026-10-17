//! The public site on the share origin (`https://glyph.my`): the landing page
//! at `/`. The `/p/` playlist page (`share.rs`) uses the same stylesheet and
//! ASCII note.

use crate::error::ApiErr;
use crate::server::{Body, Request, Response};
use crate::traits::Method;

/// Stylesheet shared by every page of the site.
pub const BASE_CSS: &str = include_str!("site/base.css");
/// `glyphNote(canvas, rowH)`: the app's splash animation.
pub const NOTE_JS: &str = include_str!("site/note.js");

/// CSP for the site's HTML pages: inline script and style, the web font.
pub const PAGE_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Paths owned by this module (`path` without its trailing `/`).
pub fn owns(path: &str) -> bool {
    path.is_empty()
}

pub fn handle(req: &Request) -> Result<Response, ApiErr> {
    if req.method != Method::Get {
        return Err(ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint"));
    }
    let html = include_str!("site/index.html").replace("/*BASE*/", BASE_CSS).replace("/*NOTE*/", NOTE_JS);
    Ok(html_response(200, html, "public, max-age=300"))
}

/// An HTML page of the site with its security headers.
pub fn html_response(status: u16, html: String, cache: &str) -> Response {
    Response {
        status,
        headers: vec![
            ("content-type".into(), "text/html; charset=utf-8".into()),
            ("cache-control".into(), cache.into()),
            ("x-content-type-options".into(), "nosniff".into()),
            ("x-frame-options".into(), "DENY".into()),
            ("referrer-policy".into(), "no-referrer".into()),
            ("content-security-policy".into(), PAGE_CSP.into()),
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
    }
}
