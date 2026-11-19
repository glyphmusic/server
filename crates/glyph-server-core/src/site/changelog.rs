//! `/changelog`: the CHANGELOG.md of every Glyph repository merged into one
//! list, newest first. The files are snapshots (`scripts/snapshot-changelogs.mjs`)
//! so builds never fetch other repositories; "Unreleased" sections are left out.

/// (repository, GitHub URL, CHANGELOG.md).
const SOURCES: &[(&str, &str, &str)] = &[
    ("desktop", "https://github.com/glyphmusic/apps", include_str!("changelogs/desktop.md")),
    ("server", "https://github.com/glyphmusic/server", include_str!("changelogs/server.md")),
    ("companion", "https://github.com/glyphmusic/companion", include_str!("changelogs/companion.md")),
    ("r-dlp", "https://github.com/glyphmusic/r-dlp", include_str!("changelogs/r-dlp.md")),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub repo: &'static str,
    pub url: &'static str,
    pub version: String,
    /// `YYYY-MM-DD`, empty when the heading has none.
    pub date: String,
    /// Plain lines between the heading and the first item or `###` section.
    pub summary: Vec<String>,
    /// `### Added`, `### Changed`, ... with their items; items before any
    /// `###` land in a section with an empty title.
    pub sections: Vec<(String, Vec<String>)>,
}

impl Release {
    pub fn items(&self) -> impl Iterator<Item = &String> {
        self.sections.iter().flat_map(|(_, items)| items)
    }
}

/// Releases of one CHANGELOG.md: `## 1.2.3 - 2026-10-19` headings (`-` or
/// `—`), an optional summary line, then `### Added`-style sections of `- `
/// items; other text lines inside a section become items too.
pub fn parse(repo: &'static str, url: &'static str, text: &str) -> Vec<Release> {
    let mut out: Vec<Release> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(head) = line.strip_prefix("## ") {
            let (version, date) = head.split_once(" - ").or_else(|| head.split_once(" — ")).unwrap_or((head, ""));
            let version = version.trim().trim_start_matches('v');
            open = !version.eq_ignore_ascii_case("unreleased") && !version.is_empty();
            if open {
                out.push(Release { repo, url, version: version.into(), date: date.trim().into(), summary: Vec::new(), sections: Vec::new() });
            }
            continue;
        }
        if !open || line.trim().is_empty() {
            continue;
        }
        let Some(release) = out.last_mut() else { continue };
        if let Some(title) = line.strip_prefix("### ") {
            release.sections.push((title.trim().into(), Vec::new()));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let item = line.trim_start().strip_prefix("- ");
        if item.is_none() && release.sections.is_empty() {
            release.summary.push(line.trim().into());
            continue;
        }
        if release.sections.is_empty() {
            release.sections.push((String::new(), Vec::new()));
        }
        let items = &mut release.sections.last_mut().expect("pushed").1;
        match item {
            Some(item) => items.push(item.trim().into()),
            // Indented continuation of the previous item.
            None if line.starts_with("  ") && !items.is_empty() => {
                let last = items.last_mut().expect("checked");
                last.push(' ');
                last.push_str(line.trim());
            }
            None => items.push(line.trim().into()),
        }
    }
    for r in &mut out {
        r.sections.retain(|(_, items)| !items.is_empty());
    }
    out.retain(|r| r.items().next().is_some() || !r.summary.is_empty());
    out
}

fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
}

/// Every release of every repository, newest first.
pub fn releases() -> Vec<Release> {
    let mut all: Vec<Release> = SOURCES.iter().flat_map(|(repo, url, text)| parse(repo, url, text)).collect();
    all.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| version_key(&b.version).cmp(&version_key(&a.version))).then_with(|| a.repo.cmp(b.repo)));
    all
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Escapes text and turns `**bold**` into `<strong>`.
fn strong(s: &str) -> String {
    let parts: Vec<&str> = s.split("**").collect();
    if parts.len() % 2 == 0 {
        return escape(s);
    }
    parts.iter().enumerate().map(|(i, p)| if i % 2 == 1 { format!("<strong>{}</strong>", escape(p)) } else { escape(p) }).collect()
}

/// Escapes an item and turns `code` spans into `<code>`, `**bold**` into `<strong>`.
fn inline(s: &str) -> String {
    let parts: Vec<&str> = s.split('`').collect();
    if parts.len() % 2 == 0 {
        return strong(s);
    }
    parts.iter().enumerate().map(|(i, p)| if i % 2 == 1 { format!("<code>{}</code>", escape(p)) } else { strong(p) }).collect()
}

fn anchor(r: &Release) -> String {
    escape(&format!("{}-{}", r.repo, r.version))
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// `(year, month 1-12, day)` of a `YYYY-MM-DD` date.
fn ymd(date: &str) -> Option<(&str, usize, u32)> {
    let mut parts = date.splitn(3, '-');
    let year = parts.next().filter(|y| y.len() == 4)?;
    let month: usize = parts.next()?.parse().ok().filter(|m| (1..=12).contains(m))?;
    let day: u32 = parts.next()?.get(..2)?.parse().ok()?;
    Some((year, month, day))
}

/// `2026-11-18` -> `18 Nov 2026`; anything else stays as written.
fn nice_date(date: &str) -> String {
    match ymd(date) {
        Some((y, m, d)) => format!("{d} {} {y}", &MONTHS[m - 1][..3]),
        None => date.to_string(),
    }
}

/// `2026-11-18` -> `November 2026`; no date -> `Undated`.
fn month_label(date: &str) -> String {
    match ymd(date) {
        Some((y, m, _)) => format!("{} {y}", MONTHS[m - 1]),
        None => "Undated".into(),
    }
}

/// Short name of a repository in chips.
fn label(repo: &str) -> &str {
    match repo {
        "desktop" => "app",
        other => other,
    }
}

/// Long name of a repository in the "newest" rows.
fn long_label(repo: &str) -> &str {
    match repo {
        "desktop" => "Desktop & phone app",
        "server" => "Server & CLI",
        "companion" => "Companion",
        "r-dlp" => "r-dlp",
        other => other,
    }
}

/// The app's icons (glyph-apps/assets/icons/ui), stroke 2.
pub const ICON_CHEVRON: &str = r#"<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9.5 6.5L13.44 10.44Q15 12 13.44 13.56L9.5 17.5"/></svg>"#;
pub const ICON_EXTERNAL: &str = r#"<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M11.5 12.5L18.17 5.83"/><path fill="currentColor" d="M20.95 4.3L20.84 7.15Q20.79 8.44 19.87 7.53L16.47 4.13Q15.56 3.21 16.85 3.16L19.7 3.05Q21 3 20.95 4.3Z"/><path d="M18 14v3a4 4 0 0 1-4 4H7a4 4 0 0 1-4-4v-7a4 4 0 0 1 4-4h3"/></svg>"#;

/// The version list in the window's nav: month labels, then one page entry
/// per release, newest first.
pub fn render_index(releases: &[Release]) -> String {
    let mut html = String::new();
    let mut month = None;
    for r in releases {
        let m = month_label(&r.date);
        if month.as_ref() != Some(&m) {
            if month.is_some() {
                html.push_str("</div>");
            }
            html.push_str(&format!("<div class=\"mon\"><div class=\"gl\">{}</div>", escape(&m)));
            month = Some(m);
        }
        html.push_str(&format!(
            "<a class=\"pg\" href=\"#{id}\" data-repo=\"{repo}\"><span class=\"nm mono\">{ver}</span><span class=\"kc\">{name}</span></a>",
            id = anchor(r),
            repo = escape(r.repo),
            ver = escape(&r.version),
            name = escape(label(r.repo)),
        ));
    }
    if month.is_some() {
        html.push_str("</div>");
    }
    html
}

/// One row per repository with its newest release.
pub fn render_latest(releases: &[Release]) -> String {
    SOURCES
        .iter()
        .filter_map(|(repo, _, _)| releases.iter().find(|r| r.repo == *repo))
        .map(|r| {
            format!(
                "<a class=\"srow\" href=\"#{id}\" data-repo=\"{repo}\"><span class=\"l\"><span class=\"lab\">{name}</span><span class=\"st\">{date}</span></span><span class=\"c\"><span class=\"ver mono\">{ver}</span>{ICON_CHEVRON}</span></a>",
                id = anchor(r),
                repo = escape(r.repo),
                name = escape(long_label(r.repo)),
                ver = escape(&r.version),
                date = escape(&nice_date(&r.date)),
            )
        })
        .collect()
}

pub fn render(releases: &[Release]) -> String {
    if releases.is_empty() {
        return "<p class=\"empty\">No releases yet.</p>".into();
    }
    let mut html = String::new();
    for r in releases {
        let summary: String = r.summary.iter().map(|s| format!("<p class=\"sum\">{}</p>", inline(s))).collect();
        let sections: String = r
            .sections
            .iter()
            .map(|(title, items)| {
                let list: String = items.iter().map(|i| format!("<li>{}</li>", inline(i))).collect();
                let kind = title.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                let head = if title.is_empty() {
                    String::new()
                } else {
                    format!("<h3 class=\"sl {}\">{} <span class=\"n mono\">{}</span></h3>", escape(&kind), escape(title), items.len())
                };
                format!("{head}<ul>{list}</ul>")
            })
            .collect();
        html.push_str(&format!(
            "<article class=\"rel\" data-repo=\"{repo}\" id=\"{id}\"><header class=\"rh\"><a class=\"rv mono\" href=\"#{id}\">{ver}</a><span class=\"kc\">{name}</span><time class=\"st\" datetime=\"{date}\">{nice}</time><a class=\"tbtn gh\" href=\"{url}/releases/tag/v{ver}\">GitHub</a></header><div class=\"prose\">{summary}{sections}</div></article>\n",
            repo = escape(r.repo),
            name = escape(label(r.repo)),
            id = anchor(r),
            url = r.url,
            ver = escape(&r.version),
            date = escape(&r.date),
            nice = escape(&nice_date(&r.date)),
        ));
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_sections_and_skips_unreleased() {
        let text = "# Changelog\n\nIntro line.\n\n## Unreleased\n\n- secret\n\n## 0.3.0 - 2026-10-19\n\nA **big** one.\n\n### Added\n\n- Add `x` <b>.\n  continued\n\n### Fixed\n\n- **Bug** gone.\n\n## 0.2.3 — 2026-10-18\n\n- Older.\n";
        let r = parse("server", "https://github.com/glyphmusic/server", text);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].version, "0.3.0");
        assert_eq!(r[0].date, "2026-10-19");
        assert_eq!(r[0].summary, vec!["A **big** one.".to_string()]);
        assert_eq!(
            r[0].sections,
            vec![("Added".to_string(), vec!["Add `x` <b>. continued".to_string()]), ("Fixed".to_string(), vec!["**Bug** gone.".to_string()])]
        );
        assert_eq!(r[1].date, "2026-10-18");
        assert_eq!(r[1].sections, vec![(String::new(), vec!["Older.".to_string()])]);
        let html = render(&r);
        assert!(html.contains("<code>x</code> &lt;b&gt;") && !html.contains("secret"));
        assert!(html.contains("<h3 class=\"sl fixed\">Fixed <span class=\"n mono\">1</span></h3>") && html.contains("<strong>Bug</strong>"));
        assert!(html.contains("<time class=\"st\" datetime=\"2026-10-19\">19 Oct 2026</time>"));
        assert!(html.contains("<p class=\"sum\">A <strong>big</strong> one.</p>"));
        assert!(html.contains("id=\"server-0.3.0\"") && html.contains("/releases/tag/v0.3.0"));
        let index = render_index(&r);
        assert!(index.contains("href=\"#server-0.2.3\"") && index.contains("<div class=\"gl\">October 2026</div>") && index.matches("class=\"mon\"").count() == 1);
        assert!(render_latest(&r).contains("href=\"#server-0.3.0\""));
        assert_eq!(nice_date("2026-01-05"), "5 Jan 2026");
        assert_eq!(nice_date("soon"), "soon");
        assert_eq!(month_label(""), "Undated");
    }

    #[test]
    fn all_repositories_merge_newest_first() {
        let all = releases();
        for repo in ["desktop", "server", "companion", "r-dlp"] {
            assert!(all.iter().any(|r| r.repo == repo), "{repo} has no releases");
        }
        assert!(all.windows(2).all(|w| w[0].date >= w[1].date));
        assert!(version_key("0.10.0") > version_key("0.9.1"));
    }

    #[test]
    fn server_snapshot_matches_this_repository() {
        // Only checkable in a full checkout; the snapshot is what ships.
        if let Ok(own) = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.md")) {
            assert_eq!(own.replace("\r\n", "\n"), include_str!("changelogs/server.md").replace("\r\n", "\n"), "run node scripts/snapshot-changelogs.mjs");
        }
    }
}
