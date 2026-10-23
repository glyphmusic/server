//! `/plugins` and `/plugins/<id>`: the plugin catalog on the website, laid
//! out like the app's Settings > Plugins page. The page itself is static:
//! the visitor's browser loads the catalog straight from GitHub (the index of
//! glyphmusic/plugins and every repo with the topic `glyph-plugin`), the same
//! way the app does, so nothing here depends on this server's state.
//! "Install in Glyph" opens `glyph://plugin/<id>`.

use crate::server::{Request, Response};
use crate::share::Lang;

const HTML: &str = include_str!("plugins.html");

/// Like the site's CSP, plus the two GitHub hosts the page reads.
const CSP: &str = "default-src 'none'; img-src 'self' data:; manifest-src 'self'; script-src 'unsafe-inline'; connect-src https://api.github.com https://raw.githubusercontent.com; style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Plugin ids as the app checks them (`glyph_core::plugins::valid_id`).
fn valid_id(id: &str) -> bool {
    (2..=40).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--")
}

/// `path` without its trailing `/`.
pub fn owns(path: &str) -> bool {
    path == "/plugins" || path.strip_prefix("/plugins/").is_some_and(valid_id)
}

/// Texts, worded like the app's (`glyph/assets/i18n`, `plugins.*`).
const STRINGS: &[(&str, &str, &str)] = &[
    ("page_title", "Glyph plugins", "Glyph-Plugins"),
    ("page_desc", "Themes and extensions for Glyph, made by people on GitHub. Install them in the app with one click.", "Themes und Erweiterungen für Glyph, gemacht von Leuten auf GitHub. Mit einem Klick in der App installieren."),
    ("title", "Plugins", "Plugins"),
    ("sub", "Themes, extensions and more, made by people on GitHub.", "Themes, Erweiterungen und mehr, gemacht von Leuten auf GitHub."),
    ("download", "Download Glyph", "Glyph herunterladen"),
    ("catalog", "Plugin catalog", "Plugin-Katalog"),
    ("loading", "Loading from GitHub…", "Wird von GitHub geladen…"),
    ("source", "Loaded from GitHub in your browser.", "Direkt in deinem Browser von GitHub geladen."),
    ("partial", "GitHub was not fully reachable. Some entries may be missing.", "GitHub war nicht ganz erreichbar. Manche Einträge fehlen vielleicht."),
    ("error", "GitHub not reachable: {e}", "GitHub nicht erreichbar: {e}"),
    ("refresh", "Refresh", "Aktualisieren"),
    ("checked", "Listed", "Gelistet"),
    ("checked_desc", "Listed in glyphmusic/plugins. Each version was checked against the plugin rules (valid file, pinned version). That is no review of quality or safety.", "Steht in glyphmusic/plugins. Jede Version wurde gegen die Plugin-Regeln geprüft (gültige Datei, feste Version). Das ist keine Prüfung von Qualität oder Sicherheit."),
    ("community", "Community", "Community"),
    ("community_desc", "GitHub repositories with the topic glyph-plugin. They are not in the list, so only plugins without code (themes, texts) can be installed from here.", "GitHub-Repos mit dem Topic glyph-plugin. Sie stehen nicht in der Liste. Deshalb lassen sich von hier nur Plugins ohne Code (Themes, Texte) installieren."),
    ("none", "No listed plugins yet.", "Noch keine gelisteten Plugins."),
    ("risk_note", "Plugins are made by other people, not by Glyph. We don't review what they do and can't say whether a plugin is good or bad. You install them at your own risk.", "Plugins stammen von anderen Leuten, nicht von Glyph. Wir prüfen nicht, was sie tun, und können nicht sagen, ob ein Plugin gut oder schlecht ist. Du installierst sie auf eigenes Risiko."),
    ("risk_listed", "This plugin was made by {author}, not by Glyph. Being listed only means its file follows the plugin rules. It is no review of quality or safety. You install it at your own risk.", "Dieses Plugin ist von {author}, nicht von Glyph. Gelistet heißt nur, dass seine Datei den Plugin-Regeln entspricht. Das ist keine Prüfung von Qualität oder Sicherheit. Du installierst es auf eigenes Risiko."),
    ("risk_community", "This plugin was made by {author}, not by Glyph. It comes straight from a GitHub repository and nobody from Glyph has looked at it. You install it at your own risk.", "Dieses Plugin ist von {author}, nicht von Glyph. Es kommt direkt aus einem GitHub-Repo, und niemand von Glyph hat es sich angesehen. Du installierst es auf eigenes Risiko."),
    ("someone", "someone else", "jemand anderem"),
    ("none_community", "No repositories with the topic glyph-plugin yet.", "Noch keine Repos mit dem Topic glyph-plugin."),
    ("repo", "Open on GitHub", "Auf GitHub öffnen"),
    ("install", "Install in Glyph", "In Glyph installieren"),
    ("install_short", "Install", "Installieren"),
    ("opens", "Opens Glyph on this plugin, ready to install.", "Öffnet Glyph direkt auf diesem Plugin, bereit zum Installieren."),
    ("no_app", "No Glyph yet?", "Noch kein Glyph?"),
    ("unchecked", "unchecked", "ungeprüft"),
    ("back", "All plugins", "Alle Plugins"),
    ("not_found", "Plugin not found", "Plugin nicht gefunden"),
    ("not_found_desc", "No plugin called \u{201c}{id}\u{201d} is in the catalog right now.", "Ein Plugin namens \u{201e}{id}\u{201c} steht gerade nicht im Katalog."),
    ("kind_theme", "Theme", "Theme"),
    ("kind_extension", "Extension", "Erweiterung"),
    ("kind_code", "Runs code", "Führt Code aus"),
    ("needs_listing", "Plugins with code install only once they are listed in glyphmusic/plugins.", "Plugins mit Code lassen sich erst installieren, wenn sie in glyphmusic/plugins gelistet sind."),
    ("does", "What it does", "Was es tut"),
    ("does_desc", "Glyph shows this list again before you install.", "Glyph zeigt diese Liste vor dem Installieren noch einmal."),
    ("perm_code", "Runs its own code in Glyph's sandbox", "Führt eigenen Code in Glyphs Sandbox aus"),
    ("perm_playback", "Controls playback and the queue, and sees what plays", "Steuert die Wiedergabe und die Warteschlange und sieht, was läuft"),
    ("perm_library", "Reads and changes your playlists, likes, lyrics and listening stats", "Liest und ändert deine Playlists, Likes, Songtexte und Hörstatistiken"),
    ("perm_search", "Searches music", "Sucht Musik"),
    ("perm_settings", "Reads and changes Glyph's settings (never keys, passwords or the server address)", "Liest und ändert Glyphs Einstellungen (nie Schlüssel, Passwörter oder die Serveradresse)"),
    ("perm_audio", "Reads the audio spectrum and changes the local equalizer", "Liest das Audiospektrum und verändert den lokalen Equalizer"),
    ("perm_lyrics_provider", "Supplies lyrics for the current song on this device", "Stellt Songtexte für den aktuellen Song auf diesem Gerät bereit"),
    ("perm_network", "Contacts {host}", "Kontaktiert {host}"),
    ("does_settings", "Adds settings", "Fügt Einstellungen hinzu"),
    ("does_menus", "Adds to the song menu", "Ergänzt das Song-Menü"),
    ("does_strings", "Changes texts", "Ändert Texte"),
    ("does_hide", "Hides settings", "Blendet Einstellungen aus"),
    ("details", "Details", "Details"),
    ("details_desc", "From the plugin's glyph-plugin.json.", "Aus der glyph-plugin.json des Plugins."),
    ("version", "Version", "Version"),
    ("author", "Author", "Autor"),
    ("license", "License", "Lizenz"),
    ("min_app", "Needs Glyph", "Braucht Glyph"),
    ("source_row", "Source", "Quelle"),
    ("colours", "Colours", "Farben"),
    ("colours_desc", "The background of this page shows the theme's glow.", "Der Hintergrund dieser Seite zeigt den Glow des Themes."),
    ("accent", "Accent", "Akzent"),
    ("glow", "Glow", "Glow"),
    ("glow_desc", "Background glow while nothing plays, dark to bright.", "Hintergrund-Glow, solange nichts läuft, von dunkel nach hell."),
    ("base", "Background", "Hintergrund"),
    ("publish", "Make your own", "Eigenes Plugin"),
    ("publish_desc", "Put a glyph-plugin.json in a GitHub repository and give it the topic glyph-plugin. It then shows up under Community. To appear under Listed (needed for plugins with code), open a pull request on glyphmusic/plugins.", "Leg eine glyph-plugin.json in ein GitHub-Repo und gib ihm das Topic glyph-plugin. Dann erscheint es unter Community. Für Gelistet (nötig für Plugins mit Code) stellst du einen Pull Request an glyphmusic/plugins."),
    ("guide", "Guide", "Anleitung"),
    ("search", "Search plugins", "Plugins suchen"),
    ("sort_stars", "Most stars", "Meiste Sterne"),
    ("sort_updated", "Recently updated", "Zuletzt aktualisiert"),
    ("sort_name", "Name", "Name"),
    ("no_match", "No plugin matches \u{201c}{q}\u{201d}.", "Kein Plugin passt zu \u{201e}{q}\u{201c}."),
    ("none_match", "No matches here.", "Hier passt nichts."),
    ("stars", "GitHub stars", "GitHub-Sterne"),
    ("stars_desc", "Stars of the plugin's GitHub repository.", "Sterne des GitHub-Repos des Plugins."),
    ("updated", "Last updated", "Zuletzt aktualisiert"),
];

/// The app's Lucide icons the page draws (same files as `glyph/assets/icons/ui`).
const ICONS: &[(&str, &str)] = &[
    ("info", r#"<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>"#),
    ("github", r#"<path d="M15 22v-4a4.8 4.8 0 0 0-1-3.5c3 0 6-2 6-5.5.08-1.25-.27-2.48-1-3.5.28-1.15.28-2.35 0-3.5 0 0-1 0-3 1.5-2.64-.5-5.36-.5-8 0C6 2 5 2 5 2c-.3 1.15-.3 2.35 0 3.5A5.403 5.403 0 0 0 4 9c0 3.5 3 5.5 6 5.5-.39.49-.68 1.05-.85 1.65-.17.6-.22 1.23-.15 1.85v4"/><path d="M9 18c-4.51 2-5-2-7-2"/>"#),
    ("external-link", r#"<path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>"#),
    ("chevron-left", r#"<path d="m15 18-6-6 6-6"/>"#),
    ("chevron-right", r#"<path d="m9 18 6-6-6-6"/>"#),
    ("download", r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/>"#),
    ("book-open", r#"<path d="M12 7v14"/><path d="M3 18a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h5a4 4 0 0 1 4 4 4 4 0 0 1 4-4h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3z"/>"#),
    ("search", r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>"#),
    ("puzzle", r#"<path d="M15.39 4.39a1 1 0 0 0 1.68-.474 2.5 2.5 0 1 1 3.014 3.015 1 1 0 0 0-.474 1.68l1.683 1.682a2.414 2.414 0 0 1 0 3.414L19.61 15.39a1 1 0 0 1-1.68-.474 2.5 2.5 0 1 0-3.014 3.015 1 1 0 0 1 .474 1.68l-1.683 1.682a2.414 2.414 0 0 1-3.414 0L8.61 19.61a1 1 0 0 0-1.68.474 2.5 2.5 0 1 1-3.014-3.015 1 1 0 0 0 .474-1.68l-1.683-1.682a2.414 2.414 0 0 1 0-3.414L4.39 8.61a1 1 0 0 1 1.68.474 2.5 2.5 0 1 0 3.014-3.015 1 1 0 0 1-.474-1.68l1.683-1.682a2.414 2.414 0 0 1 3.414 0z"/>"#),
];

fn svg(body: &str) -> String {
    format!(r#"<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{body}</svg>"#)
}

/// JSON for an inline `<script>`: `<` escaped so no text can close the tag.
fn script_json(v: &serde_json::Value) -> String {
    v.to_string().replace('<', "\\u003c")
}

/// Escapes text for HTML content and attribute values.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn handle(req: &Request) -> Response {
    let l = crate::share::lang(req.header("accept-language"));
    let pick = |en: &'static str, de: &'static str| if l == Lang::De { de } else { en };
    let strings: serde_json::Map<String, serde_json::Value> = STRINGS.iter().map(|(k, en, de)| (k.to_string(), pick(en, de).into())).collect();
    let icons: serde_json::Map<String, serde_json::Value> = ICONS.iter().map(|(k, body)| (k.to_string(), svg(body).into())).collect();
    let mut html = HTML
        .replace("/*SHARE*/", crate::share::SHARE_CSS)
        .replace("/*GLOW*/", crate::share::GLOW_JS)
        .replace("/*STRINGS*/", &script_json(&serde_json::Value::Object(strings)))
        .replace("/*ICONS*/", &script_json(&serde_json::Value::Object(icons)))
        .replace("{{lang}}", l.code())
        .replace("{{icon_download}}", &svg(ICONS.iter().find(|i| i.0 == "download").map_or("", |i| i.1)));
    for key in ["page_title", "page_desc", "title", "sub", "download"] {
        let (_, en, de) = STRINGS.iter().find(|s| s.0 == key).expect("string");
        html = html.replace(&format!("{{{{{key}}}}}"), &esc(pick(en, de)));
    }
    crate::share::localized(crate::site::html_response_csp(200, html, "public, max-age=300", CSP, false), l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use crate::testutil::*;
    use crate::traits::Method;
    use futures::executor::block_on;

    #[test]
    fn plugin_pages() {
        let h = harness(Default::default());
        let s = Server::from_arc(h.svc.clone());
        let get = |p: &str, lang: &str| Request::new(Method::Get, p, vec![("accept-language".into(), lang.into())], vec![], Some("1.2.3.4".into()));
        for path in ["/plugins", "/plugins/", "/plugins/sakura"] {
            let r = block_on(s.handle(get(path, "en")));
            assert_eq!(r.status, 200, "{path}");
            let html = String::from_utf8_lossy(r.bytes().unwrap()).to_string();
            assert!(html.contains("glyph://plugin/") && html.contains("function glyphGlow") && html.contains("Install in Glyph"));
            assert!(!html.contains("/*SHARE*/") && !html.contains("/*STRINGS*/") && !html.contains("{{"));
            let csp = r.headers.iter().find(|(k, _)| k == "content-security-policy").map(|(_, v)| v.clone()).unwrap();
            assert!(csp.contains("https://raw.githubusercontent.com") && csp.contains("https://api.github.com"));
        }
        let de = String::from_utf8_lossy(block_on(s.handle(get("/plugins", "de-DE"))).bytes().unwrap()).to_string();
        assert!(de.contains("In Glyph installieren") && de.contains(r#"<html lang="de">"#));
        assert!(!owns("/plugins/Bad_Id") && !owns("/plugins/a/b") && owns("/plugins/lo-fi-2"));
        assert_eq!(block_on(s.handle(get("/plugins/../etc", "en"))).status, 404);
    }
}
