//! MCP server for the docs: `POST https://docs.glyph.my/mcp` (`/docs/mcp`
//! on other hosts). AI agents search and read the documentation through
//! three tools instead of loading every page:
//!
//! * `search_docs`: the best-matching sections for a question or keywords,
//!   with a snippet, the address and the path for `read_doc`.
//! * `read_doc`: one page (or one of its sections) as clean Markdown.
//! * `list_docs`: every page with its one-line description.
//!
//! Streamable HTTP with JSON answers (no event stream, no session), no
//! sign-in: the docs are public. Pages come from the same GitHub cache as the
//! site, so a push to the docs shows up here within about a minute.

use super::*;
use serde_json::{json, Value};

/// MCP versions this endpoint speaks, newest first.
const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_RESULTS: usize = 5;
const MAX_RESULTS: usize = 10;
/// Sections of one page among the results at most.
const PER_PAGE: usize = 3;
const SNIPPET: usize = 260;

const INSTRUCTIONS: &str = "Glyph's documentation: how to use the Glyph desktop app (search, library, local files, downloads, sound and equalizer, Now Playing, keyboard shortcuts, settings, accounts, devices, plugins), how to self-host the server, the CLI and the API, and how to write plugins. For a how-to question call search_docs with a few English keywords, then read_doc on the best result for the whole page before you answer. Settings paths are written like Settings › Playback › Player. Cite the page address.";

/// The MCP endpoint of the docs.
pub fn owns(req: &Request) -> bool {
    let p = req.path.trim_end_matches('/');
    (is_docs_host(req) && p == "/mcp") || p == "/docs/mcp"
}

fn with_cors(mut r: Response) -> Response {
    r.headers.push(("access-control-allow-origin".into(), "*".into()));
    r.headers.push(("access-control-expose-headers".into(), "mcp-protocol-version".into()));
    r
}

fn empty(status: u16) -> Response {
    Response { status, headers: vec![("cache-control".into(), "no-store".into())], body: Body::Bytes(Vec::new()) }
}

pub async fn handle(svc: &Services, req: &Request) -> Response {
    match req.method {
        Method::Post => {}
        Method::Options => {
            let mut r = with_cors(empty(204));
            r.headers.push(("access-control-allow-methods".into(), "POST, OPTIONS".into()));
            r.headers.push(("access-control-allow-headers".into(), "content-type, accept, mcp-protocol-version, mcp-session-id, authorization".into()));
            r.headers.push(("access-control-max-age".into(), "86400".into()));
            return r;
        }
        // No server-to-client event stream: answers come with each POST.
        _ => {
            let mut r = with_cors(empty(405));
            r.headers.push(("allow".into(), "POST, OPTIONS".into()));
            return r;
        }
    }
    let msg: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return with_cors(Response::json(400, &rpc_error(Value::Null, -32700, "parse error: the body is not JSON"))),
    };
    let answer = match msg {
        // Batches (older protocol versions).
        Value::Array(items) => {
            let mut out = Vec::new();
            for m in &items {
                if let Some(a) = answer(svc, req, m).await {
                    out.push(a);
                }
            }
            (!out.is_empty()).then(|| Value::Array(out))
        }
        m => answer(svc, req, &m).await,
    };
    match answer {
        Some(a) => with_cors(Response::json(200, &a)),
        // Notifications and responses get no answer.
        None => with_cors(empty(202)),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn answer(svc: &Services, req: &Request, msg: &Value) -> Option<Value> {
    let id = msg.get("id").filter(|v| !v.is_null()).cloned()?;
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(id, -32600, "invalid request: no method"));
    };
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = PROTOCOLS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOLS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "glyph-docs", "title": "Glyph docs", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "resources/list" => json!({ "resources": [] }),
        "resources/templates/list" => json!({ "resourceTemplates": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call(svc, req, name, &args).await {
                Ok(r) => r,
                Err(Unknown) => return Some(rpc_error(id, -32602, &format!("unknown tool: {name}"))),
            }
        }
        _ => return Some(rpc_error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tools() -> Value {
    let read_only = json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false });
    json!([
        {
            "name": "search_docs",
            "title": "Search the Glyph docs",
            "description": "Search Glyph's documentation (desktop app, settings, keyboard shortcuts, self-hosting, CLI, API, plugins). Returns the best-matching sections with a snippet, their address and the path for read_doc. Use a few English keywords, e.g. \"change download folder\" or \"equalizer headroom\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords or a short question, in English." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "description": "How many sections to return (default 5)." }
                },
                "required": ["query"]
            },
            "annotations": read_only.clone(),
        },
        {
            "name": "read_doc",
            "title": "Read a docs page",
            "description": "Read one page of Glyph's documentation as Markdown, or only one section of it. `path` is what search_docs or list_docs returned (\"using-glyph/local-files.md\", \"using-glyph/local-files#add-a-folder\") or a docs.glyph.my address.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Page path or address; a #section at the end reads only that section." },
                    "section": { "type": "string", "description": "Optional heading (its text or anchor) to read only that section." }
                },
                "required": ["path"]
            },
            "annotations": read_only.clone(),
        },
        {
            "name": "list_docs",
            "title": "List the docs pages",
            "description": "Every page of Glyph's documentation, grouped as in the navigation, each with its path and a one-line description.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only.clone(),
        }
    ])
}

/// A tool name this server does not have.
struct Unknown;

fn text_result(text: String, structured: Option<Value>) -> Value {
    let mut r = json!({ "content": [{ "type": "text", "text": text }], "isError": false });
    if let Some(s) = structured {
        r["structuredContent"] = s;
    }
    r
}

/// A failed tool call: told to the agent, not a protocol error.
fn tool_error(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// The docs as the site has them now: commit, navigation, pages, and the
/// address the docs live under for this request.
struct Loaded {
    nav: Nav,
    pages: Vec<(String, String)>,
    origin: String,
    rev: String,
    pinned: bool,
}

async fn load(svc: &Services, req: &Request) -> Result<Loaded, String> {
    let base = if is_docs_host(req) { "" } else { "/docs" };
    let origin = format!("{}{base}", crate::site::origin(req));
    let (rev, pinned) = match head_commit(svc).await {
        Some(sha) => (sha, true),
        None => (BRANCH.to_string(), false),
    };
    let summary = match fetch(svc, &rev, "SUMMARY.md", pinned).await {
        Fetched::Found(b) => String::from_utf8_lossy(&b).into_owned(),
        Fetched::Missing => String::new(),
        Fetched::Failed => return Err("The docs could not be loaded from GitHub right now. Try again in a minute.".into()),
    };
    let nav = Nav::parse(&summary, "");
    let pages = all_pages(svc, &rev, pinned, &nav).await;
    if pages.is_empty() {
        return Err("The docs could not be loaded from GitHub right now. Try again in a minute.".into());
    }
    Ok(Loaded { nav, pages, origin, rev, pinned })
}

async fn call(svc: &Services, req: &Request, name: &str, args: &Value) -> Result<Value, Unknown> {
    if !matches!(name, "search_docs" | "read_doc" | "list_docs") {
        return Err(Unknown);
    }
    let docs = match load(svc, req).await {
        Ok(d) => d,
        Err(e) => return Ok(tool_error(&e)),
    };
    Ok(match name {
        "search_docs" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
            if query.is_empty() {
                return Ok(tool_error("`query` is empty: pass a few keywords."));
            }
            let limit = args.get("limit").and_then(Value::as_u64).map_or(DEFAULT_RESULTS, |n| (n as usize).clamp(1, MAX_RESULTS));
            search_tool(&docs, query, limit)
        }
        "read_doc" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("").trim();
            let section = args.get("section").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
            read_tool(svc, &docs, path, section).await
        }
        _ => {
            let mut text = llms_txt(&docs.pages, &docs.nav, &docs.origin);
            text.push_str("\nRead a page with read_doc and its path (for example `using-glyph/local-files.md`), or search with search_docs.\n");
            text_result(text, None)
        }
    })
}

// ------------------------------------------------------------------ sections

/// One searchable part of a page: the text under a `##` / `###` heading
/// (or the start of the page before the first one).
#[derive(Debug, Clone)]
struct Section {
    file: String,
    page: String,
    description: String,
    heading: String,
    /// Anchor of the heading as the site makes it; "" for the page start.
    id: String,
    /// Plain words of the section.
    text: String,
}

/// A heading line's text without Markdown: `` `Ctrl+K` `` -> `Ctrl+K`,
/// `[Install](x.md)` -> `Install`.
fn heading_text(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw.trim().trim_end_matches('#').trim();
    while let Some(i) = rest.find("](") {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find(')') {
            Some(j) => &rest[i + j + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out.chars().filter(|c| !matches!(c, '`' | '*' | '[' | ']' | '_')).collect::<String>().trim().to_string()
}

/// `## Title` -> (2, "Title"), outside code blocks only (the caller tracks
/// fences).
fn heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_end();
    let level = t.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&level).then_some(())?;
    let rest = &t[level..];
    rest.starts_with(' ').then(|| (level, heading_text(rest)))
}

fn split_sections(file: &str, md: &str) -> Vec<Section> {
    let md = md.replace("\r\n", "\n");
    let (description, body) = front_matter(&md);
    let description = description.unwrap_or_default();
    let mut page = String::new();
    let mut out: Vec<Section> = Vec::new();
    let mut cur_heading = String::new();
    let mut cur_id = String::new();
    let mut buf = String::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut fence: Option<&str> = None;
    let flush = |out: &mut Vec<Section>, heading: &str, id: &str, buf: &str, page: &str| {
        let text = plain_text(buf);
        if !text.is_empty() || !heading.is_empty() {
            out.push(Section { file: file.to_string(), page: page.to_string(), description: description.clone(), heading: heading.to_string(), id: id.to_string(), text });
        }
    };
    for line in body.lines() {
        let t = line.trim();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            buf.push_str(line);
            buf.push('\n');
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
            buf.push_str(line);
            buf.push('\n');
            continue;
        }
        if let Some((level, text)) = heading(line) {
            if level == 1 && page.is_empty() {
                page = text;
                continue;
            }
            // Same anchors as the rendered page.
            let mut id = slug(&text);
            let mut n = 1;
            while !used.insert(id.clone()) {
                n += 1;
                id = format!("{}-{n}", slug(&text));
            }
            if level <= 3 {
                flush(&mut out, &cur_heading, &cur_id, &buf, &page);
                buf.clear();
                cur_heading = text;
                cur_id = id;
                continue;
            }
        }
        buf.push_str(line);
        buf.push('\n');
    }
    flush(&mut out, &cur_heading, &cur_id, &buf, &page);
    if page.is_empty() {
        page = file.trim_end_matches(".md").rsplit('/').next().unwrap_or(file).to_string();
    }
    for s in &mut out {
        if s.page.is_empty() {
            s.page = page.clone();
        }
    }
    out
}

// ------------------------------------------------------------------ search

const STOP: &[&str] = &[
    "a", "an", "and", "are", "be", "can", "do", "does", "for", "from", "glyph", "how", "i", "in", "is", "it", "me", "my", "of", "on", "or", "the",
    "there", "this", "to", "what", "when", "where", "which", "with", "you", "your",
];

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Search terms of a query: its words without filler ("how do I ...").
fn query_terms(q: &str) -> Vec<String> {
    let all = words(q);
    let mut kept: Vec<String> = all.iter().filter(|w| !STOP.contains(&w.as_str())).cloned().collect();
    if kept.is_empty() {
        kept = all;
    }
    kept.dedup();
    kept
}

/// A word counts for a term when it is the term, or starts with it for
/// terms of four letters or more ("download" finds "downloads").
fn hits(term: &str, word: &str) -> bool {
    word == term || (term.chars().count() >= 4 && word.starts_with(term))
}

#[derive(Debug, Clone)]
struct Hit {
    section: usize,
    score: f32,
}

fn rank(sections: &[Section], query: &str) -> Vec<Hit> {
    let terms = query_terms(query);
    if terms.is_empty() || sections.is_empty() {
        return Vec::new();
    }
    let docs: Vec<(Vec<String>, Vec<String>, Vec<String>)> = sections
        .iter()
        .map(|s| (words(&s.text), words(&s.heading), words(&format!("{} {}", s.page, s.description))))
        .collect();
    let n = sections.len() as f32;
    let avg = docs.iter().map(|d| d.0.len()).sum::<usize>() as f32 / n;
    let idf: Vec<f32> = terms
        .iter()
        .map(|t| {
            let df = docs.iter().filter(|d| d.0.iter().chain(d.1.iter()).chain(d.2.iter()).any(|w| hits(t, w))).count() as f32;
            (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    let phrase = words(query).join(" ");
    let mut out: Vec<Hit> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, (body, head, page))| {
            let mut score = 0.0;
            let mut matched = 0;
            for (t, w) in terms.iter().zip(&idf) {
                let tf = body.iter().filter(|x| hits(t, x)).count() as f32;
                let in_head = head.iter().any(|x| hits(t, x));
                let in_page = page.iter().any(|x| hits(t, x));
                if tf > 0.0 || in_head || in_page {
                    matched += 1;
                }
                let len = body.len().max(1) as f32;
                score += w * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * len / avg.max(1.0)));
                if in_head {
                    score += 2.0 * w;
                }
                if in_page {
                    score += 1.2 * w;
                }
            }
            if matched == 0 {
                return None;
            }
            // Sections with every term first.
            let cover = matched as f32 / terms.len() as f32;
            score *= cover * cover;
            if phrase.contains(' ') && body.join(" ").contains(&phrase) {
                score *= 1.5;
            }
            Some(Hit { section: i, score })
        })
        .collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// About `SNIPPET` characters of `text` around the first query term.
fn snippet(text: &str, terms: &[String]) -> String {
    let lower = text.to_lowercase();
    let at = terms.iter().filter_map(|t| lower.find(t.as_str())).min().unwrap_or(0);
    // Byte offsets of `lower` match `text` for the characters the docs use;
    // fall back to the start when they do not.
    let at = if text.is_char_boundary(at) { at } else { 0 };
    let start = text[..at].char_indices().rev().nth(SNIPPET / 3).map_or(0, |(i, _)| i);
    let start = if start == 0 { 0 } else { text[start..].find(' ').map_or(start, |s| start + s + 1) };
    let mut s: String = text[start..].chars().take(SNIPPET).collect();
    let cut = s.chars().count() == SNIPPET;
    if cut {
        if let Some(i) = s.rfind(' ') {
            s.truncate(i);
        }
    }
    format!("{}{}{}", if start > 0 { "…" } else { "" }, s.trim(), if cut { "…" } else { "" })
}

fn section_path(s: &Section) -> String {
    if s.id.is_empty() { s.file.clone() } else { format!("{}#{}", s.file, s.id) }
}

fn section_url(s: &Section, origin: &str) -> String {
    let page = page_url(&s.file, origin).unwrap_or_else(|| format!("{origin}/{}", s.file));
    if s.id.is_empty() { page } else { format!("{page}#{}", s.id) }
}

fn search_tool(docs: &Loaded, query: &str, limit: usize) -> Value {
    let sections: Vec<Section> = docs.pages.iter().flat_map(|(f, md)| split_sections(f, md)).collect();
    let terms = query_terms(query);
    let mut per_page: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut picked = Vec::new();
    for h in rank(&sections, query) {
        let s = &sections[h.section];
        let n = per_page.entry(s.file.as_str()).or_default();
        if *n >= PER_PAGE {
            continue;
        }
        *n += 1;
        picked.push((s, h.score));
        if picked.len() >= limit {
            break;
        }
    }
    if picked.is_empty() {
        return text_result(
            format!("Nothing in the Glyph docs matches \"{query}\". Try other or fewer English keywords, or call list_docs to see every page."),
            Some(json!({ "results": [] })),
        );
    }
    let mut text = format!("{} sections of the Glyph docs match \"{query}\" (best first). Read a whole page with read_doc.\n", picked.len());
    let mut results = Vec::new();
    for (i, (s, score)) in picked.iter().enumerate() {
        let title = if s.heading.is_empty() { s.page.clone() } else { format!("{} › {}", s.page, s.heading) };
        let url = section_url(s, &docs.origin);
        let path = section_path(s);
        let snip = snippet(&s.text, &terms);
        text.push_str(&format!("\n{}. {title}\n   {url}\n   read_doc path: {path}\n   {snip}\n", i + 1));
        results.push(json!({ "title": title, "page": s.page, "section": s.heading, "url": url, "path": path, "snippet": snip, "score": (score * 100.0).round() / 100.0 }));
    }
    text_result(text, Some(json!({ "results": results })))
}

// ------------------------------------------------------------------ read

/// The repository file a `read_doc` path means: a file path, a page address
/// (with or without the docs origin, `.md` or `/docs`), or a page name.
fn resolve_page(docs: &Loaded, path: &str) -> Option<(String, Option<String>)> {
    let p = path.trim();
    let p = p.split_once("://").map_or(p, |(_, rest)| rest.split_once('/').map_or("", |(_, r)| r));
    let (p, frag) = match p.split_once('#') {
        Some((a, b)) => (a, Some(b.to_string()).filter(|f| !f.is_empty())),
        None => (p, None),
    };
    let p = p.split('?').next().unwrap_or("").trim_matches('/');
    let p = p.strip_prefix("docs/").unwrap_or(p);
    let p = p.strip_suffix(".md").unwrap_or(p);
    let known = |f: &str| docs.pages.iter().any(|(x, _)| x == f);
    let candidates = if p.is_empty() || p == "docs" { vec!["README.md".to_string()] } else { vec![format!("{p}.md"), format!("{p}/README.md")] };
    if let Some(f) = candidates.iter().find(|f| valid_file(f.as_str()) && known(f.as_str())) {
        return Some((f.clone(), frag));
    }
    // A page name alone ("local-files"), or a file outside the navigation.
    let name = p.rsplit('/').next().unwrap_or(p);
    if let Some((f, _)) = docs.pages.iter().find(|(f, _)| f.trim_end_matches(".md").rsplit('/').next() == Some(name) && !name.is_empty()) {
        return Some((f.clone(), frag));
    }
    candidates.into_iter().find(|f| valid_file(f)).map(|f| (f, frag))
}

/// The lines of `md` under the heading `want` (its anchor or text), up to
/// the next heading of the same or a higher level.
fn cut_section(md: &str, want: &str) -> Option<String> {
    let want_slug = slug(want);
    let mut used: HashSet<String> = HashSet::new();
    let mut fence: Option<String> = None;
    let mut out: Option<(usize, String)> = None;
    let mut title_seen = false;
    for line in md.lines() {
        let t = line.trim();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
        } else if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t[..3].to_string());
        } else if let Some((level, text)) = heading(line) {
            if level == 1 && !title_seen {
                title_seen = true;
                continue;
            }
            let mut id = slug(&text);
            let mut n = 1;
            while !used.insert(id.clone()) {
                n += 1;
                id = format!("{}-{n}", slug(&text));
            }
            if let Some((lvl, _)) = &out {
                if level <= *lvl {
                    break;
                }
            } else if id == want_slug || text.eq_ignore_ascii_case(want) || id == want {
                out = Some((level, String::new()));
            }
        }
        if let Some((_, buf)) = &mut out {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    out.map(|(_, s)| s)
}

async fn read_tool(svc: &Services, docs: &Loaded, path: &str, section: Option<&str>) -> Value {
    if path.is_empty() {
        return tool_error("`path` is empty: pass a page path from search_docs or list_docs, e.g. `using-glyph/local-files.md`.");
    }
    let Some((file, frag)) = resolve_page(docs, path) else {
        return tool_error(&format!("No docs page matches `{path}`. Call list_docs for every page."));
    };
    let md = match docs.pages.iter().find(|(f, _)| *f == file) {
        Some((_, md)) => md.clone(),
        None => match fetch(svc, &docs.rev, &file, docs.pinned).await {
            Fetched::Found(b) => String::from_utf8_lossy(&b).replace("\r\n", "\n"),
            Fetched::Missing => return tool_error(&format!("No docs page matches `{path}`. Call list_docs for every page.")),
            Fetched::Failed => return tool_error("The docs could not be loaded from GitHub right now. Try again in a minute."),
        },
    };
    let url = page_url(&file, &docs.origin).unwrap_or_else(|| format!("{}/{file}", docs.origin));
    let clean = clean_markdown(&md);
    let wanted = section.map(str::to_string).or(frag);
    let (body, url) = match wanted {
        Some(w) => match cut_section(&clean, &w) {
            Some(part) => (part, format!("{url}#{}", slug(&w))),
            None => {
                let heads: Vec<String> = split_sections(&file, &md).into_iter().filter(|s| !s.heading.is_empty()).map(|s| format!("{} (#{})", s.heading, s.id)).collect();
                return tool_error(&format!("`{file}` has no section `{w}`. Its sections: {}.", heads.join(", ")));
            }
        },
        None => (clean, url),
    };
    text_result(format!("Source: {url}\n\n{}", body.trim_end()), Some(json!({ "path": file, "url": url })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use crate::testutil::*;
    use futures::executor::block_on;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const LOCAL: &str = "---\ndescription: Play music files from folders on your computer.\n---\n\n# Local files\n\nGlyph plays MP3 and FLAC files from folders.\n\n## Add a folder\n\nOpen Library and click **Add folder**.\n\n```toml\n## not a heading\n```\n\n## Browse\n\nFolders, artists, albums.\n";
    const DOWNLOADS: &str = "# Downloads and audio quality\n\n## Download folder\n\nChange it under Settings › Cache & Storage › Download folder.\n\n## Quality\n\nLow, Standard, High.\n";

    fn server() -> (Server, Harness) {
        let h = harness(Default::default());
        h.http.on("info/refs", 200, &format!("003f{SHA} refs/heads/main\n0000"));
        h.http.on(&format!("{SHA}/SUMMARY.md"), 200, "* [Welcome](README.md)\n\n## Using Glyph\n\n* [Local files](using-glyph/local-files.md)\n* [Downloads](using-glyph/downloads-and-quality.md)\n");
        h.http.on(&format!("{SHA}/README.md"), 200, "# Welcome to Glyph\n\nA music player.\n");
        h.http.on(&format!("{SHA}/using-glyph/local-files.md"), 200, LOCAL);
        h.http.on(&format!("{SHA}/using-glyph/downloads-and-quality.md"), 200, DOWNLOADS);
        (Server::from_arc(h.svc.clone()), h)
    }

    fn rpc(s: &Server, host: &str, path: &str, body: Value) -> Response {
        let req = Request::new(Method::Post, path, vec![("host".into(), host.into()), ("content-type".into(), "application/json".into())], serde_json::to_vec(&body).unwrap(), Some("1.2.3.4".into()))
            .with_transport(true, false);
        block_on(s.handle(req))
    }

    fn json_of(r: &Response) -> Value {
        serde_json::from_slice(r.bytes().unwrap()).unwrap()
    }

    fn call(s: &Server, name: &str, args: Value) -> Value {
        let r = rpc(s, "docs.glyph.my", "/mcp", json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": name, "arguments": args } }));
        assert_eq!(r.status, 200);
        json_of(&r)["result"].clone()
    }

    #[test]
    fn speaks_mcp() {
        let (s, _h) = server();
        let init = rpc(&s, "docs.glyph.my", "/mcp", json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }));
        assert_eq!(init.status, 200);
        let v = json_of(&init);
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["result"]["serverInfo"]["name"], "glyph-docs");
        assert!(init.headers.iter().any(|(k, v)| k == "access-control-allow-origin" && v == "*"));
        // Notifications get no answer.
        let note = rpc(&s, "docs.glyph.my", "/mcp", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        assert_eq!(note.status, 202);
        let tools = json_of(&rpc(&s, "docs.glyph.my", "/mcp", json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })));
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["search_docs", "read_doc", "list_docs"]);
        let unknown = json_of(&rpc(&s, "docs.glyph.my", "/mcp", json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" })));
        assert_eq!(unknown["error"]["code"], -32601);
        // Also under /docs/mcp on the API host, and never on GET.
        assert_eq!(rpc(&s, "api.glyph.my", "/docs/mcp", json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" })).status, 200);
        let get = block_on(s.handle(Request::new(Method::Get, "/mcp", vec![("host".into(), "docs.glyph.my".into())], vec![], Some("1.2.3.4".into()))));
        assert_eq!(get.status, 405);
    }

    #[test]
    fn searches_and_reads_sections() {
        let (s, _h) = server();
        let found = call(&s, "search_docs", json!({ "query": "how do I add a folder with my music" }));
        let first = &found["structuredContent"]["results"][0];
        assert_eq!(first["path"], "using-glyph/local-files.md#add-a-folder", "{found}");
        assert_eq!(first["url"], "https://docs.glyph.my/using-glyph/local-files#add-a-folder");
        assert!(found["content"][0]["text"].as_str().unwrap().contains("Local files › Add a folder"));

        let dl = call(&s, "search_docs", json!({ "query": "change download folder", "limit": 1 }));
        assert_eq!(dl["structuredContent"]["results"].as_array().unwrap().len(), 1);
        assert_eq!(dl["structuredContent"]["results"][0]["path"], "using-glyph/downloads-and-quality.md#download-folder");

        let none = call(&s, "search_docs", json!({ "query": "zzzz qqqq" }));
        assert!(none["content"][0]["text"].as_str().unwrap().contains("list_docs"));

        let page = call(&s, "read_doc", json!({ "path": "https://docs.glyph.my/using-glyph/local-files" }));
        let text = page["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("Source: https://docs.glyph.my/using-glyph/local-files\n\n# Local files"), "{text}");
        let part = call(&s, "read_doc", json!({ "path": "local-files#add-a-folder" }));
        let part = part["content"][0]["text"].as_str().unwrap();
        assert!(part.contains("## Add a folder") && part.contains("not a heading") && !part.contains("## Browse"), "{part}");
        let missing = call(&s, "read_doc", json!({ "path": "using-glyph/local-files.md", "section": "Nope" }));
        assert_eq!(missing["isError"], true);
        assert!(missing["content"][0]["text"].as_str().unwrap().contains("Add a folder (#add-a-folder)"));

        let list = call(&s, "list_docs", json!({}));
        assert!(list["content"][0]["text"].as_str().unwrap().contains("[Local files](https://docs.glyph.my/using-glyph/local-files.md): Play music files"));
    }

    #[test]
    fn splits_like_the_rendered_page() {
        let secs = split_sections("using-glyph/local-files.md", LOCAL);
        let heads: Vec<(&str, &str)> = secs.iter().map(|s| (s.heading.as_str(), s.id.as_str())).collect();
        assert_eq!(heads, [("", ""), ("Add a folder", "add-a-folder"), ("Browse", "browse")]);
        assert!(secs.iter().all(|s| s.page == "Local files"));
        assert_eq!(heading_text(" `Ctrl+K` and [Install](install.md) "), "Ctrl+K and Install");
        assert_eq!(query_terms("How do I change the download folder?"), ["change", "download", "folder"]);
        assert!(hits("download", "downloads") && !hits("eq", "equalizer"));
    }
}
