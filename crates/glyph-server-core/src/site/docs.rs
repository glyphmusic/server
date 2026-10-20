//! The documentation at `docs.glyph.my` (any `docs.*` host) and `/docs` on
//! other hosts: the Markdown of github.com/glyphmusic/docs rendered in the
//! site's style. Files are read from GitHub at request time and cached under
//! the commit `main` points to, so a push shows up within about a minute
//! without a deploy. `SUMMARY.md` is the navigation; GitBook's `hint` and
//! `tabs` blocks and `<details>` are understood, any other raw HTML is shown
//! as text.
//!
//! For search and LLMs: `search.json` (the index the search box loads),
//! `llms.txt` and `llms-full.txt` (llmstxt.org), and every page as Markdown
//! under its file name (`/getting-started/install.md`).

use crate::server::{Body, Request, Response};
use crate::traits::{HttpRequest, Method, Services};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::collections::HashSet;

const REPO: &str = "glyphmusic/docs";
const BRANCH: &str = "main";
/// Public docs address; `glyph.my/docs` redirects here.
pub const DOCS_ORIGIN: &str = "https://docs.glyph.my";
/// How long the commit of `main` is reused: the delay before a push is live.
const REF_TTL_S: u64 = 60;
/// Files under a fixed commit never change. Kept below one hour so the Worker
/// does not write them to Workers KV (1,000 writes a day on the free plan).
const FILE_TTL_S: u64 = 3000;
const MAX_FILE: usize = 512 * 1024;
const HINTS: &[&str] = &["info", "success", "warning", "danger"];

/// Images may come from anywhere over HTTPS (relative ones from GitHub).
const DOCS_CSP: &str = "default-src 'none'; img-src 'self' data: https:; manifest-src 'self'; script-src 'unsafe-inline'; connect-src 'self'; style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

fn host(req: &Request) -> String {
    req.header("host").unwrap_or("").split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase()
}

fn is_docs_host(req: &Request) -> bool {
    host(req).starts_with("docs.")
}

/// `path` without its trailing `/`.
pub fn owns(req: &Request, path: &str) -> bool {
    is_docs_host(req) || path == "/docs" || path.starts_with("/docs/")
}

/// Machine-readable routes next to the pages (relative to the docs root).
fn docs_route(rest: &str) -> bool {
    matches!(rest, "/search.json" | "/llms.txt" | "/llms-full.txt") || rest.ends_with(".md")
}

pub async fn handle(svc: &Services, req: &Request) -> Response {
    let path = req.path.trim_end_matches('/');
    let docs_host = is_docs_host(req);
    // Icons, robots.txt and the like are shared with the main site.
    if docs_host && !path.is_empty() && !docs_route(path) && crate::site::owns(path) {
        return crate::site::handle(req).unwrap_or_else(Response::error);
    }
    if req.method != Method::Get {
        return Response::error(crate::error::ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint"));
    }
    let rest = if docs_host { path } else { path.strip_prefix("/docs").unwrap_or("") };
    if !docs_host && host(req) == "glyph.my" {
        return redirect(&format!("{DOCS_ORIGIN}{rest}"), 301);
    }
    let base = if docs_host { "" } else { "/docs" };
    // Links back to the main site: on docs.example.com that is example.com.
    let site = if docs_host { format!("https://{}", &host(req)["docs.".len()..]) } else { String::new() };
    // Absolute docs address for llms.txt.
    let origin = format!("{}{base}", crate::site::origin(req));
    let slug = rest.trim_start_matches('/');
    let raw = slug.strip_suffix(".md");
    let special = matches!(slug, "search.json" | "llms.txt" | "llms-full.txt");
    let wanted = raw.map(|r| format!("{r}.md")).unwrap_or_else(|| format!("{slug}.md"));
    if !slug.is_empty() && !special && !valid_file(&wanted) {
        return page_response(404, &not_found(&site, base, ""));
    }

    let (rev, rev_ok) = match head_commit(svc).await {
        Some(sha) => (sha, true),
        None => (BRANCH.to_string(), false),
    };
    let summary = match fetch(svc, &rev, "SUMMARY.md", rev_ok).await {
        Fetched::Found(b) => String::from_utf8_lossy(&b).into_owned(),
        Fetched::Missing => String::new(),
        Fetched::Failed => return page_response(503, &unavailable(&site, base)),
    };
    let nav = Nav::parse(&summary, base);
    if special {
        let ttl = if rev_ok { FILE_TTL_S } else { REF_TTL_S };
        let key = format!("docs:gen:{rev}:{base}:{origin}:{slug}");
        let (ctype, cache) = match slug {
            "search.json" => ("application/json", "public, max-age=60"),
            _ => ("text/markdown; charset=utf-8", "public, max-age=300"),
        };
        if let Some(body) = svc.cache.get(&key).await {
            return text_response(200, ctype, cache, body);
        }
        let pages = all_pages(svc, &rev, rev_ok, &nav).await;
        let body = match slug {
            "search.json" => search_index(&pages, base, &rev),
            "llms.txt" => llms_txt(&pages, &nav, &origin),
            _ => llms_full(&pages, &origin),
        }
        .into_bytes();
        svc.cache.put(&key, body.clone(), ttl).await;
        return text_response(200, ctype, cache, body);
    }
    if raw.is_some() {
        return match fetch(svc, &rev, &wanted, rev_ok).await {
            Fetched::Found(b) => text_response(200, "text/markdown; charset=utf-8", "public, max-age=60", clean_markdown(&String::from_utf8_lossy(&b)).into_bytes()),
            Fetched::Missing => text_response(404, "text/plain; charset=utf-8", "no-store", b"not found
".to_vec()),
            Fetched::Failed => text_response(503, "text/plain; charset=utf-8", "no-store", b"docs unavailable
".to_vec()),
        };
    }
    let candidates = if slug.is_empty() { vec!["README.md".to_string()] } else { vec![format!("{slug}.md"), format!("{slug}/README.md")] };
    for file in &candidates {
        match fetch(svc, &rev, file, rev_ok).await {
            Fetched::Found(bytes) => {
                let md = String::from_utf8_lossy(&bytes);
                let ctx = Ctx { dir: parent(file), base, rev: &rev };
                let doc = render_markdown(&md, &ctx);
                return page_response(200, &page(&site, base, file, &doc, &nav));
            }
            Fetched::Missing => continue,
            Fetched::Failed => return page_response(503, &unavailable(&site, base)),
        }
    }
    page_response(404, &not_found(&site, base, &summary))
}

fn text_response(status: u16, ctype: &str, cache: &str, body: Vec<u8>) -> Response {
    Response {
        status,
        headers: vec![
            ("content-type".into(), ctype.into()),
            ("cache-control".into(), cache.into()),
            ("x-content-type-options".into(), "nosniff".into()),
            ("access-control-allow-origin".into(), "*".into()),
        ],
        body: Body::Bytes(body),
    }
}

fn redirect(to: &str, status: u16) -> Response {
    Response {
        status,
        headers: vec![("location".into(), if to.is_empty() { "/".into() } else { to.into() }), ("cache-control".into(), "public, max-age=3600".into())],
        body: Body::Bytes(Vec::new()),
    }
}

fn page_response(status: u16, html: &str) -> Response {
    let cache = if status == 200 { "public, max-age=60" } else { "no-store" };
    crate::site::html_response_csp(status, html.to_string(), cache, DOCS_CSP, false)
}

// ------------------------------------------------------------------ GitHub

/// The commit `main` points to, from the smart-HTTP ref advertisement (no
/// API quota, unlike api.github.com).
async fn head_commit(svc: &Services) -> Option<String> {
    let key = format!("docs:ref:{REPO}:{BRANCH}");
    if let Some(v) = svc.cache.get(&key).await {
        return String::from_utf8(v).ok();
    }
    let url = format!("https://github.com/{REPO}.git/info/refs?service=git-upload-pack");
    let r = svc.http.send(HttpRequest::get(url).header("user-agent", "glyph-server")).await.ok()?;
    if r.status != 200 {
        return None;
    }
    let sha = parse_ref(&r.body, BRANCH)?;
    svc.cache.put(&key, sha.clone().into_bytes(), REF_TTL_S).await;
    Some(sha)
}

fn parse_ref(body: &[u8], branch: &str) -> Option<String> {
    let want = format!(" refs/heads/{branch}");
    String::from_utf8_lossy(body).lines().find_map(|line| {
        let line = line.split('\0').next().unwrap_or("");
        let before = line.strip_suffix(want.as_str())?;
        let sha = before.get(before.len().checked_sub(40)?..)?;
        sha.bytes().all(|b| b.is_ascii_hexdigit()).then(|| sha.to_ascii_lowercase())
    })
}

enum Fetched {
    Found(Vec<u8>),
    Missing,
    Failed,
}

/// `pinned`: `rev` is a commit, so the answer can be cached for long.
async fn fetch(svc: &Services, rev: &str, file: &str, pinned: bool) -> Fetched {
    let key = format!("docs:file:{rev}:{file}");
    if let Some(v) = svc.cache.get(&key).await {
        return match v.split_first() {
            Some((b'1', data)) => Fetched::Found(data.to_vec()),
            _ => Fetched::Missing,
        };
    }
    let url = format!("https://raw.githubusercontent.com/{REPO}/{rev}/{file}");
    let ttl = if pinned { FILE_TTL_S } else { REF_TTL_S };
    match svc.http.send(HttpRequest::get(url).header("user-agent", "glyph-server")).await {
        Ok(r) if r.status == 200 && r.body.len() <= MAX_FILE => {
            let mut v = Vec::with_capacity(r.body.len() + 1);
            v.push(b'1');
            v.extend_from_slice(&r.body);
            svc.cache.put(&key, v, ttl).await;
            Fetched::Found(r.body)
        }
        Ok(r) if r.status == 404 => {
            svc.cache.put(&key, b"0".to_vec(), ttl).await;
            Fetched::Missing
        }
        _ => Fetched::Failed,
    }
}

// ------------------------------------------------------------------ paths

/// A repository file path we are willing to fetch.
fn valid_file(f: &str) -> bool {
    !f.is_empty()
        && f.len() <= 200
        && !f.starts_with('/')
        && f.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
        && f.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

fn parent(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(d, _)| d)
}

/// `p` relative to directory `dir` (or the root with a leading `/`).
fn resolve(dir: &str, p: &str) -> Option<String> {
    let mut parts: Vec<&str> = if p.starts_with('/') { Vec::new() } else { dir.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    let mut f = parts.join("/");
    if p.ends_with('/') || f.is_empty() {
        f = if f.is_empty() { "README.md".into() } else { format!("{f}/README.md") };
    }
    valid_file(&f).then_some(f)
}

struct Ctx<'a> {
    /// Directory of the page being rendered.
    dir: &'a str,
    /// URL prefix of the docs: `""` or `"/docs"`.
    base: &'a str,
    rev: &'a str,
}

/// URL of a repository Markdown file.
fn page_url(file: &str, base: &str) -> Option<String> {
    let s = file.strip_suffix(".md")?;
    let s = s.strip_suffix("README").unwrap_or(s).trim_end_matches('/');
    Some(if s.is_empty() { format!("{base}/") } else { format!("{base}/{s}") })
}

fn link(dest: &str, ctx: &Ctx) -> String {
    let d = dest.trim();
    let lower = d.to_ascii_lowercase();
    if d.starts_with('#') || lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") {
        return d.to_string();
    }
    if lower.contains(':') {
        return "#".into(); // javascript:, data:, ...
    }
    let (p, frag) = d.split_once('#').unwrap_or((d, ""));
    let Some(file) = resolve(ctx.dir, p) else { return "#".into() };
    let url = page_url(&file, ctx.base).unwrap_or_else(|| format!("https://github.com/{REPO}/blob/{BRANCH}/{file}"));
    if frag.is_empty() { url } else { format!("{url}#{frag}") }
}

fn image(dest: &str, ctx: &Ctx) -> String {
    let d = dest.trim();
    if d.to_ascii_lowercase().starts_with("https://") {
        return d.to_string();
    }
    match resolve(ctx.dir, d.split(['#', '?']).next().unwrap_or("")) {
        Some(file) if !d.contains(':') => format!("https://raw.githubusercontent.com/{REPO}/{}/{file}", ctx.rev),
        _ => String::new(),
    }
}

// ------------------------------------------------------------------ Markdown

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn allowed_tag(t: &str) -> bool {
    const TAGS: &[&str] = &[
        "<details>", "</details>", "<summary>", "</summary>", "<div class=\"tabs\">", "<div class=\"tab\">",
        "<p class=\"tab-title\">", "</p>", "</div>", "<br>", "<br/>", "<br />",
    ];
    TAGS.contains(&t) || t.strip_prefix("<div class=\"hint ").and_then(|r| r.strip_suffix("\">")).is_some_and(|s| HINTS.contains(&s))
}

/// Raw HTML from the Markdown: the few tags the docs use pass, everything
/// else (scripts, styles, attributes) is shown as text.
fn sanitize_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&escape(&rest[..i]));
        let tail = &rest[i..];
        match tail.find('>') {
            Some(j) if allowed_tag(&tail[..=j]) => {
                out.push_str(&tail[..=j]);
                rest = &tail[j + 1..];
            }
            _ => {
                out.push_str("&lt;");
                rest = &tail[1..];
            }
        }
    }
    out.push_str(&escape(rest));
    out
}

/// `title="Native"` -> `Native`.
fn attr<'a>(args: &'a str, name: &str) -> Option<&'a str> {
    let start = args.find(&format!("{name}=\""))? + name.len() + 2;
    let len = args[start..].find('"')?;
    Some(&args[start..start + len])
}

/// GitBook's `{% hint %}` and `{% tabs %}` blocks as HTML blocks; other
/// `{% ... %}` lines are dropped. Code blocks are left alone.
fn gitbook_blocks(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 256);
    let mut fence: Option<&str> = None;
    for line in md.lines() {
        let t = line.trim();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
        } else if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
        } else if let Some(tag) = t.strip_prefix("{%").and_then(|x| x.strip_suffix("%}")) {
            let tag = tag.trim();
            let (name, args) = tag.split_once(' ').unwrap_or((tag, ""));
            let html = match name {
                "hint" => format!("<div class=\"hint {}\">", attr(args, "style").filter(|s| HINTS.contains(s)).unwrap_or("info")),
                "tabs" => "<div class=\"tabs\">".into(),
                "tab" => format!("<div class=\"tab\">\n<p class=\"tab-title\">{}</p>", attr(args, "title").unwrap_or("")),
                "endhint" | "endtab" | "endtabs" => "</div>".into(),
                _ => String::new(),
            };
            if !html.is_empty() {
                out.push('\n');
                out.push_str(&html);
                out.push_str("\n\n");
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Front matter `description:` and the rest of the file.
fn front_matter(md: &str) -> (Option<String>, &str) {
    let Some(rest) = md.strip_prefix("---\n") else { return (None, md) };
    let Some(end) = rest.find("\n---") else { return (None, md) };
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
    let desc = rest[..end]
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().trim_matches(['"', '\'']).to_string())
        .filter(|d| !d.is_empty());
    (desc, body)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if matches!(c, ' ' | '-' | '_') && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "section".into() } else { out }
}

struct Doc {
    title: String,
    description: Option<String>,
    html: String,
    /// `(id, text)` of every `##` heading.
    sections: Vec<(String, String)>,
    /// `(id, text)` of every `##` and `###` heading, for search.
    headings: Vec<(String, String)>,
}

fn render_markdown(md: &str, ctx: &Ctx) -> Doc {
    let md = md.replace("\r\n", "\n");
    let (description, body) = front_matter(&md);
    let body = gitbook_blocks(body);
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events: Vec<Event> = Parser::new_ext(&body, opts)
        .map(|ev| match ev {
            Event::Html(h) => Event::Html(sanitize_html(&h).into()),
            Event::InlineHtml(h) => Event::InlineHtml(sanitize_html(&h).into()),
            Event::Start(Tag::Link { link_type, dest_url, title, id }) => Event::Start(Tag::Link { link_type, dest_url: link(&dest_url, ctx).into(), title, id }),
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => Event::Start(Tag::Image { link_type, dest_url: image(&dest_url, ctx).into(), title, id }),
            e => e,
        })
        .collect();

    let mut title = None;
    let mut html = String::new();
    let mut sections = Vec::new();
    let mut headings = Vec::new();
    let mut used = HashSet::new();
    let mut i = 0;
    while i < events.len() {
        if let Event::Start(Tag::Heading { level, .. }) = &events[i] {
            let level = *level as usize;
            let end = events[i..].iter().position(|e| matches!(e, Event::End(TagEnd::Heading(_)))).map_or(events.len(), |p| i + p);
            let inner = &events[i + 1..end];
            let text: String = inner.iter().filter_map(|e| match e { Event::Text(t) | Event::Code(t) => Some(t.as_ref()), _ => None }).collect();
            if level == 1 && title.is_none() {
                title = Some(text);
            } else {
                let mut id = slug(&text);
                let mut n = 1;
                while !used.insert(id.clone()) {
                    n += 1;
                    id = format!("{}-{n}", slug(&text));
                }
                let mut inner_html = String::new();
                pulldown_cmark::html::push_html(&mut inner_html, inner.iter().cloned());
                if level <= 3 {
                    headings.push((id.clone(), text.clone()));
                }
                if level == 2 {
                    sections.push((id.clone(), text));
                }
                html.push_str(&format!("<h{level} id=\"{id}\"><a class=\"anchor\" href=\"#{id}\">{inner_html}</a></h{level}>\n"));
            }
            i = end + 1;
            continue;
        }
        let start = i;
        while i < events.len() && !matches!(events[i], Event::Start(Tag::Heading { .. })) {
            i += 1;
        }
        pulldown_cmark::html::push_html(&mut html, events[start..i].iter().cloned());
    }
    Doc { title: title.unwrap_or_else(|| "Glyph docs".into()), description, html, sections, headings }
}

// ------------------------------------------------------------------ navigation

enum Entry {
    Group(String),
    Item { title: String, href: String, file: String, depth: usize },
}

struct Nav(Vec<Entry>);

impl Nav {
    /// `## Group` headings and `* [Title](file.md)` items of `SUMMARY.md`.
    fn parse(summary: &str, base: &str) -> Nav {
        let ctx = Ctx { dir: "", base, rev: BRANCH };
        let mut out = Vec::new();
        for line in summary.replace("\r\n", "\n").lines() {
            if let Some(h) = line.strip_prefix("## ") {
                out.push(Entry::Group(h.trim().to_string()));
                continue;
            }
            let t = line.trim_start();
            let depth = (line.len() - t.len()) / 2;
            let Some(item) = t.strip_prefix("* ").or_else(|| t.strip_prefix("- ")) else { continue };
            let Some((title, rest)) = item.strip_prefix('[').and_then(|r| r.split_once("](")) else { continue };
            let Some(target) = rest.split(')').next() else { continue };
            let Some(file) = resolve("", target.split('#').next().unwrap_or("")) else { continue };
            out.push(Entry::Item { title: title.to_string(), href: link(target, &ctx), file, depth });
        }
        Nav(out)
    }

    fn items(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.0.iter().filter_map(|e| match e {
            Entry::Item { title, href, file, .. } => Some((title.as_str(), href.as_str(), file.as_str())),
            Entry::Group(_) => None,
        })
    }

    fn render(&self, current: &str) -> String {
        let mut html = String::from("<ul>");
        for e in &self.0 {
            match e {
                Entry::Group(g) => html.push_str(&format!("</ul><p class=\"grp\">{}</p><ul>", escape(g))),
                Entry::Item { title, href, file, depth } => {
                    let cur = if file == current { " aria-current=\"page\"" } else { "" };
                    html.push_str(&format!("<li class=\"d{}\"><a href=\"{}\"{cur}>{}</a></li>", (*depth).min(3), escape(href), escape(title)));
                }
            }
        }
        html.push_str("</ul>");
        html.replace("<ul></ul>", "")
    }

    /// Previous and next page around `current`.
    fn pager(&self, current: &str) -> String {
        let items: Vec<_> = self.items().collect();
        let Some(i) = items.iter().position(|(_, _, f)| *f == current) else { return String::new() };
        let mut html = String::new();
        if let Some((t, h, _)) = i.checked_sub(1).and_then(|p| items.get(p)) {
            html.push_str(&format!("<a class=\"prev\" href=\"{}\"><span>previous</span>{}</a>", escape(h), escape(t)));
        }
        if let Some((t, h, _)) = items.get(i + 1) {
            html.push_str(&format!("<a class=\"next\" href=\"{}\"><span>next</span>{}</a>", escape(h), escape(t)));
        }
        html
    }
}

// ------------------------------------------------------------------ search and LLMs

/// Every page in the navigation, `(file, markdown)`, in navigation order.
async fn all_pages(svc: &Services, rev: &str, pinned: bool, nav: &Nav) -> Vec<(String, String)> {
    let mut files: Vec<String> = Vec::new();
    for (_, _, f) in nav.items() {
        if !files.iter().any(|x| x == f) {
            files.push(f.to_string());
        }
    }
    if files.is_empty() {
        files.push("README.md".into());
    }
    let got = futures::future::join_all(files.iter().map(|f| fetch(svc, rev, f, pinned))).await;
    files
        .into_iter()
        .zip(got)
        .filter_map(|(f, r)| match r {
            Fetched::Found(b) => Some((f, String::from_utf8_lossy(&b).replace("\r\n", "\n"))),
            _ => None,
        })
        .collect()
}

/// The Markdown without GitBook syntax: hints become quotes, tabs bold
/// labels, the front-matter description a quote under the title.
fn clean_markdown(md: &str) -> String {
    let md = md.replace("\r\n", "\n");
    let (description, body) = front_matter(&md);
    let mut out = String::with_capacity(body.len() + 64);
    let mut fence: Option<String> = None;
    let mut quote = false;
    let mut described = description.is_none();
    for line in body.lines() {
        let t = line.trim();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
        } else if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t[..3].to_string());
        } else if let Some(tag) = t.strip_prefix("{%").and_then(|x| x.strip_suffix("%}")) {
            let tag = tag.trim();
            let (name, args) = tag.split_once(' ').unwrap_or((tag, ""));
            match name {
                "hint" => {
                    let label = match attr(args, "style") {
                        Some("warning") => "Warning",
                        Some("danger") => "Careful",
                        Some("success") => "Tip",
                        _ => "Note",
                    };
                    out.push_str(&format!("\n> **{label}:**\n"));
                    quote = true;
                }
                "endhint" => {
                    quote = false;
                    out.push('\n');
                }
                "tab" => out.push_str(&format!("\n**{}**\n\n", attr(args, "title").unwrap_or(""))),
                _ => {}
            }
            continue;
        }
        if quote {
            out.push_str(if t.is_empty() { ">" } else { "> " });
        }
        out.push_str(line);
        out.push('\n');
        if !described && line.starts_with("# ") {
            out.push_str(&format!("\n> {}\n", description.as_deref().unwrap_or("")));
            described = true;
        }
    }
    format!("{}\n", out.trim())
}

/// Searchable text of a page: no Markdown syntax, link targets or tags.
fn plain_text(md: &str) -> String {
    let (_, body) = front_matter(md);
    let mut out = String::new();
    for line in clean_markdown(body).lines() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            continue;
        }
        let mut s = String::with_capacity(t.len());
        let mut rest = t;
        while let Some(i) = rest.find("](") {
            s.push_str(&rest[..i]);
            rest = match rest[i..].find(')') {
                Some(j) => &rest[i + j + 1..],
                None => &rest[i + 2..],
            };
        }
        s.push_str(rest);
        let mut clean = String::with_capacity(s.len());
        let mut in_tag = false;
        for c in s.chars() {
            match c {
                '<' => in_tag = true,
                '>' if in_tag => in_tag = false,
                _ if in_tag => {}
                '#' | '*' | '>' | '|' | '`' | '[' | ']' => clean.push(' '),
                c => clean.push(c),
            }
        }
        for w in clean.split_whitespace() {
            if !w.chars().all(|c| c == '-') {
                out.push_str(w);
                out.push(' ');
            }
        }
    }
    out.chars().take(30_000).collect::<String>().trim_end().to_string()
}

/// `search.json`: what the search box on every page looks through.
fn search_index(pages: &[(String, String)], base: &str, rev: &str) -> String {
    let entries: Vec<serde_json::Value> = pages
        .iter()
        .map(|(file, md)| {
            let ctx = Ctx { dir: parent(file), base, rev };
            let doc = render_markdown(md, &ctx);
            serde_json::json!({
                "t": doc.title,
                "u": page_url(file, base).unwrap_or_default(),
                "d": doc.description.unwrap_or_default(),
                "h": doc.headings,
                "x": plain_text(md),
            })
        })
        .collect();
    serde_json::json!({ "pages": entries }).to_string()
}

/// `llms.txt` (llmstxt.org): what the docs cover and where each page is as
/// Markdown.
fn llms_txt(pages: &[(String, String)], nav: &Nav, origin: &str) -> String {
    let mut out = String::from(
        "# Glyph docs\n\n> Documentation for Glyph, a free, open-source desktop music player for Windows, macOS and Linux that searches many legal music sources at once, and for its self-hostable server and `glyph` CLI, the YouTube companion and r-dlp.\n\n",
    );
    out.push_str(&format!(
        "Every page is also available as Markdown: add `.md` to its address. All pages in one file: [llms-full.txt]({origin}/llms-full.txt). Source: https://github.com/{REPO}\n"
    ));
    let describe = |file: &str| -> String {
        pages.iter().find(|(f, _)| f == file).and_then(|(_, md)| front_matter(md).0).map(|d| format!(": {d}")).unwrap_or_default()
    };
    let mut open = false;
    for e in &nav.0 {
        match e {
            Entry::Group(g) => {
                out.push_str(&format!("\n## {g}\n\n"));
                open = true;
            }
            Entry::Item { title, file, .. } => {
                if !open {
                    out.push_str("\n## Start\n\n");
                    open = true;
                }
                out.push_str(&format!("- [{title}]({origin}/{file}){}\n", describe(file)));
            }
        }
    }
    out
}

/// `llms-full.txt`: every page's Markdown, in navigation order.
fn llms_full(pages: &[(String, String)], origin: &str) -> String {
    let mut out = String::from("# Glyph docs (all pages)\n");
    for (file, md) in pages {
        out.push_str(&format!("\n---\n\nSource: {origin}/{file}\n\n"));
        out.push_str(&clean_markdown(md));
    }
    out
}

// ------------------------------------------------------------------ pages

#[allow(clippy::too_many_arguments)]
fn fill(site: &str, base: &str, title: &str, description: &str, canonical: &str, markdown: &str, nav: &str, body: &str) -> String {
    include_str!("docs.html")
        .replace("/*BASE*/", crate::site::BASE_CSS)
        .replace("/*BRAND*/", &crate::site::brand_icon_css("nav .brand"))
        .replace("{{TITLE}}", &escape(title))
        .replace("{{DESC}}", &escape(description))
        .replace("{{CANON}}", &escape(canonical))
        .replace("{{SITE}}", &escape(site))
        .replace("{{DOCS}}", &escape(if base.is_empty() { "/" } else { base }))
        .replace("{{BASE}}", &escape(base))
        .replace("{{MD}}", &escape(markdown))
        .replace("{{NAV}}", nav)
        .replace("{{BODY}}", body)
}

fn page(site: &str, base: &str, file: &str, doc: &Doc, nav: &Nav) -> String {
    let path = page_url(file, "").unwrap_or_default();
    let mut body = format!("<h1>{}</h1>", escape(&doc.title));
    if let Some(d) = &doc.description {
        body.push_str(&format!("<p class=\"lead\">{}</p>", escape(d)));
    }
    if doc.sections.len() >= 3 {
        let list: String = doc.sections.iter().map(|(id, t)| format!("<li><a href=\"#{id}\">{}</a></li>", escape(t))).collect();
        body.push_str(&format!("<nav class=\"onpage\" aria-label=\"On this page\"><ul>{list}</ul></nav>"));
    }
    body.push_str(&doc.html);
    body.push_str(&format!(
        "<div class=\"pager\">{}</div><p class=\"edit\"><a href=\"https://github.com/{REPO}/edit/{BRANCH}/{f}\">edit this page on github</a> · <a href=\"{base}/{f}\">markdown</a></p>",
        nav.pager(file),
        f = escape(file)
    ));
    let description = doc.description.clone().unwrap_or_else(|| "Documentation for Glyph, the free music player.".into());
    fill(site, base, &doc.title, &description, &format!("{DOCS_ORIGIN}{path}"), &format!("{base}/{file}"), &nav.render(file), &body)
}

fn not_found(site: &str, base: &str, summary: &str) -> String {
    let nav = Nav::parse(summary, base);
    let home = if base.is_empty() { "/" } else { base };
    let body = format!("<h1>not found</h1><p class=\"lead\">This page does not exist (any more). Search above, or start at the <a href=\"{home}\">docs home</a>.</p>");
    fill(site, base, "Not found", "Page not found.", DOCS_ORIGIN, &format!("{base}/llms.txt"), &nav.render(""), &body)
}

fn unavailable(site: &str, base: &str) -> String {
    let body = format!("<h1>docs unavailable</h1><p class=\"lead\">The docs could not be loaded right now. Try again in a minute, or read them on <a href=\"https://github.com/{REPO}\">GitHub</a>.</p>");
    fill(site, base, "Docs unavailable", "The docs could not be loaded.", DOCS_ORIGIN, &format!("{base}/llms.txt"), "", &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use crate::testutil::*;
    use futures::executor::block_on;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn ctx() -> Ctx<'static> {
        Ctx { dir: "getting-started", base: "", rev: SHA }
    }

    #[test]
    fn reads_the_branch_commit() {
        let body = format!("001e# service=git-upload-pack\n0000015b{SHA} HEAD\0multi_ack side-band-64k\n003f{SHA} refs/heads/main\n0000");
        assert_eq!(parse_ref(body.as_bytes(), "main").as_deref(), Some(SHA));
        assert_eq!(parse_ref(body.as_bytes(), "dev"), None);
    }

    #[test]
    fn rewrites_links() {
        let c = ctx();
        assert_eq!(link("install.md", &c), "/install");
        assert_eq!(link("../using-glyph/sound.md#play-on-another-computer-mpd", &c), "/using-glyph/sound#play-on-another-computer-mpd");
        assert_eq!(link("../README.md", &c), "/");
        assert_eq!(link("https://github.com/glyphmusic", &c), "https://github.com/glyphmusic");
        assert_eq!(link("javascript:alert(1)", &c), "#");
        assert_eq!(link("../../../etc/passwd", &c), "#");
        assert_eq!(link("#tips", &c), "#tips");
        let d = Ctx { base: "/docs", ..ctx() };
        assert_eq!(link("first-steps.md", &d), "/docs/first-steps");
        assert_eq!(image("../.gitbook/assets/shot.png", &c), format!("https://raw.githubusercontent.com/glyphmusic/docs/{SHA}/.gitbook/assets/shot.png"));
        assert_eq!(image("data:image/png;base64,xx", &c), "");
    }

    #[test]
    fn keeps_only_known_html() {
        assert_eq!(sanitize_html("<details>\n<summary>Is it <b>free</b>?</summary>"), "<details>\n<summary>Is it &lt;b&gt;free&lt;/b&gt;?</summary>");
        assert_eq!(sanitize_html("<script>x</script>"), "&lt;script&gt;x&lt;/script&gt;");
        assert_eq!(sanitize_html("<div class=\"hint warning\">"), "<div class=\"hint warning\">");
        assert_eq!(sanitize_html("<div class=\"hint x\" onclick=\"y\">"), "&lt;div class=&quot;hint x&quot; onclick=&quot;y&quot;&gt;");
    }

    #[test]
    fn renders_gitbook_markdown() {
        let md = "---\ndescription: A short lead.\n---\n\n# Install\n\nText with [a link](first-steps.md).\n\n{% hint style=\"warning\" %}\nCareful.\n{% endhint %}\n\n## Build from source\n\n```bash\n{% not a tag %}\n```\n\n{% tabs %}\n{% tab title=\"Native\" %}\nOne\n{% endtab %}\n{% endtabs %}\n\n## Build from source\n\n<script>alert(1)</script>\n";
        let doc = render_markdown(md, &ctx());
        assert_eq!(doc.title, "Install");
        assert_eq!(doc.description.as_deref(), Some("A short lead."));
        assert!(doc.html.contains("href=\"/first-steps\""));
        assert!(doc.html.contains("<div class=\"hint warning\">") && doc.html.contains("Careful."));
        assert!(doc.html.contains("{% not a tag %}"), "code blocks stay as written");
        assert!(doc.html.contains("<p class=\"tab-title\">Native</p>"));
        assert!(doc.html.contains("id=\"build-from-source\"") && doc.html.contains("id=\"build-from-source-2\""));
        assert!(!doc.html.contains("<script>") && !doc.html.contains("<h1"));
        assert_eq!(doc.sections.len(), 2);
    }

    #[test]
    fn navigation_from_summary() {
        let nav = Nav::parse("# Table of contents\n\n* [Welcome](README.md)\n\n## Getting started\n\n* [Install](getting-started/install.md)\n  * [Linux](getting-started/linux.md)\n", "/docs");
        let html = nav.render("getting-started/install.md");
        assert!(html.contains("<p class=\"grp\">Getting started</p>"));
        assert!(html.contains("<a href=\"/docs/getting-started/install\" aria-current=\"page\">Install</a>"));
        assert!(html.contains("class=\"d1\""));
        let pager = nav.pager("getting-started/install.md");
        assert!(pager.contains("href=\"/docs/\"") && pager.contains("href=\"/docs/getting-started/linux\""));
        assert_eq!(slug("Play on another computer (MPD)"), "play-on-another-computer-mpd");
    }

    #[test]
    fn serves_pages_from_github() {
        let h = harness(Default::default());
        h.http.on("info/refs", 200, &format!("003f{SHA} refs/heads/main\n0000"));
        h.http.on(&format!("{SHA}/SUMMARY.md"), 200, "* [Welcome](README.md)\n* [Install](getting-started/install.md)\n");
        h.http.on(&format!("{SHA}/README.md"), 200, "# Welcome to Glyph\n\nHello.\n");
        h.http.on(&format!("{SHA}/getting-started/install.md"), 200, "# Install\n\nDownload it.\n");
        let s = Server::from_arc(h.svc.clone());
        let get = |host: &str, p: &str| Request::new(Method::Get, p, vec![("host".into(), host.into())], vec![], Some("1.2.3.4".into())).with_transport(true, false);
        let text = |r: Response| String::from_utf8_lossy(r.bytes().unwrap()).to_string();

        let home = block_on(s.handle(get("docs.glyph.my", "/")));
        assert_eq!(home.status, 200);
        let html = text(home);
        assert!(html.contains("<h1>Welcome to Glyph</h1>") && html.contains("href=\"https://glyph.my/\"") && !html.contains("{{"));

        let install = block_on(s.handle(get("docs.glyph.my", "/getting-started/install")));
        assert_eq!(install.status, 200);
        assert!(text(install).contains("aria-current=\"page\">Install"));

        assert_eq!(block_on(s.handle(get("docs.glyph.my", "/nope"))).status, 404);
        assert_eq!(block_on(s.handle(get("docs.glyph.my", "/a%2e%2e/b"))).status, 404);
        assert_eq!(block_on(s.handle(get("docs.glyph.my", "/favicon.ico"))).status, 200);

        let moved = block_on(s.handle(get("glyph.my", "/docs/getting-started/install")));
        assert_eq!(moved.status, 301);
        assert!(moved.headers.iter().any(|(k, v)| k == "location" && v == "https://docs.glyph.my/getting-started/install"));

        let local = block_on(s.handle(get("localhost:8787", "/docs/getting-started/install")));
        assert_eq!(local.status, 200);
        assert!(text(local).contains("href=\"/docs/\""));

        // Cached under the commit: a second view does not ask GitHub again.
        let before = h.http.call_count("raw.githubusercontent.com");
        block_on(s.handle(get("docs.glyph.my", "/")));
        assert_eq!(h.http.call_count("raw.githubusercontent.com"), before);
    }

    #[test]
    fn search_and_llm_files() {
        let h = harness(Default::default());
        h.http.on("info/refs", 200, &format!("003f{SHA} refs/heads/main\n0000"));
        h.http.on(&format!("{SHA}/SUMMARY.md"), 200, "* [Welcome](README.md)\n\n## Self-hosting\n\n* [Limits](self-hosting/limits.md)\n");
        h.http.on(&format!("{SHA}/README.md"), 200, "# Welcome to Glyph\n\nHello.\n");
        h.http.on(
            &format!("{SHA}/self-hosting/limits.md"),
            200,
            "---\ndescription: Rate limits and bans.\n---\n\n# Limits\n\n## Device limit\n\nSee [the CLI](cli.md).\n\n{% hint style=\"warning\" %}\nBe careful.\n{% endhint %}\n",
        );
        let s = Server::from_arc(h.svc.clone());
        let get = |p: &str| Request::new(Method::Get, p, vec![("host".into(), "docs.glyph.my".into())], vec![], Some("1.2.3.4".into())).with_transport(true, false);
        let text = |p: &str| {
            let r = block_on(s.handle(get(p)));
            assert_eq!(r.status, 200, "{p}");
            String::from_utf8_lossy(r.bytes().unwrap()).to_string()
        };

        let llms = text("/llms.txt");
        assert!(llms.starts_with("# Glyph docs\n\n> "), "{llms}");
        assert!(llms.contains("## Self-hosting\n\n- [Limits](https://docs.glyph.my/self-hosting/limits.md): Rate limits and bans."));
        assert!(llms.contains("https://docs.glyph.my/llms-full.txt"));

        let md = text("/self-hosting/limits.md");
        assert!(md.starts_with("# Limits\n\n> Rate limits and bans.\n"), "{md}");
        assert!(md.contains("> **Warning:**\n> Be careful.") && !md.contains("{%"));

        let full = text("/llms-full.txt");
        assert!(full.contains("Source: https://docs.glyph.my/README.md") && full.contains("# Limits"));

        let index: serde_json::Value = serde_json::from_str(&text("/search.json")).unwrap();
        let pages = index["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[1]["u"], "/self-hosting/limits");
        assert_eq!(pages[1]["h"][0][0], "device-limit");
        assert!(pages[1]["x"].as_str().unwrap().contains("See the CLI"));

        let page = text("/self-hosting/limits");
        assert!(page.contains("rel=\"alternate\" type=\"text/markdown\" href=\"/self-hosting/limits.md\"") && page.contains("/search.json"));
        assert_eq!(block_on(s.handle(get("/nope.md"))).status, 404);
    }

    #[test]
    fn github_down_is_a_503() {
        let h = harness(Default::default());
        h.http.on("info/refs", 500, "");
        h.http.on("raw.githubusercontent.com", 502, "");
        let s = Server::from_arc(h.svc.clone());
        let r = block_on(s.handle(Request::new(Method::Get, "/", vec![("host".into(), "docs.glyph.my".into())], vec![], Some("1.2.3.4".into()))));
        assert_eq!(r.status, 503);
    }
}
