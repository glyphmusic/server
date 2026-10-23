//! `glyph config`: read and change `server.toml` without editing it by hand.
//! Every option is listed with its default and the environment variable that
//! overrides it; API keys are never printed, only whether they are set.

use crate::settings::{self, FileConfig};
use clap::Subcommand;
use std::path::Path;

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// List every option with its value, where it comes from and its default
    List,
    /// Print one option (`keys.*` only shows whether it is set)
    Get { key: String },
    /// Change an option; `keys.*` without a value is asked for without echo
    Set { key: String, value: Option<String> },
    /// Remove an option so its default applies again
    Unset { key: String },
    /// Print the path of the config file
    Path,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Text,
    Int,
    Bool,
    Secret,
}

struct Opt {
    key: &'static str,
    kind: Kind,
    env: Option<&'static str>,
    default: &'static str,
    help: &'static str,
}

const fn opt(key: &'static str, kind: Kind, env: Option<&'static str>, default: &'static str, help: &'static str) -> Opt {
    Opt { key, kind, env, default, help }
}

const OPTS: &[Opt] = &[
    opt("bind", Kind::Text, Some("GLYPH_BIND"), "0.0.0.0:8787 (443 with tls)", "address and port to listen on"),
    opt("public_url", Kind::Text, Some("GLYPH_PUBLIC_URL"), "-", "URL clients use, e.g. https://glyph.example.com"),
    opt("cache_dir", Kind::Text, Some("GLYPH_CACHE_DIR"), "in memory", "directory for the response cache"),
    opt("cache_entries", Kind::Int, Some("GLYPH_CACHE_ENTRIES"), "10000", "cached responses kept"),
    opt("trust_proxy", Kind::Bool, Some("GLYPH_TRUST_PROXY"), "false", "read client IPs from X-Forwarded-For (behind a reverse proxy)"),
    opt("rate_limit_per_min", Kind::Int, Some("GLYPH_RATE_LIMIT_PER_MIN"), "60", "anonymous requests per minute and IP"),
    opt("require_account", Kind::Bool, Some("GLYPH_REQUIRE_ACCOUNT"), "false", "only signed-in apps may use the API"),
    opt("youtube_daily_budget_units", Kind::Int, Some("YOUTUBE_DAILY_BUDGET_UNITS"), "9000", "YouTube Data API units per day"),
    opt("ytdlp_path", Kind::Text, Some("YTDLP_PATH"), "from PATH", "yt-dlp executable for YouTube"),
    opt("proxy_audio", Kind::Bool, Some("GLYPH_PROXY_AUDIO"), "true", "stream audio through this server"),
    opt("playlist_max_tracks", Kind::Int, None, "5000", "maximum songs per playlist (1-50000)"),
    opt("import_max_pages", Kind::Int, None, "5", "pages fetched per playlist import"),
    opt("youtube_per_day", Kind::Int, None, "0 (no limit)", "YouTube songs per account or address and day"),
    opt("update_repo", Kind::Text, Some("GLYPH_SERVER_REPO"), "glyphmusic/server", "GitHub repository used for updates"),
    opt("tls.domain", Kind::Text, Some("GLYPH_TLS_DOMAIN"), "-", "domain for automatic Let's Encrypt HTTPS"),
    opt("tls.email", Kind::Text, Some("GLYPH_TLS_EMAIL"), "-", "contact address for Let's Encrypt"),
    opt("tls.cache_dir", Kind::Text, None, "tls-cache next to server.toml", "where certificates are kept"),
    opt("tls.staging", Kind::Bool, None, "false", "use the Let's Encrypt staging environment"),
    opt("keys.youtube_api_key", Kind::Secret, Some("YOUTUBE_API_KEY"), "-", "YouTube Data API key (playlist import)"),
    opt("keys.jamendo_client_id", Kind::Secret, Some("JAMENDO_CLIENT_ID"), "-", "Jamendo client ID"),
    opt("keys.soundcloud_client_id", Kind::Secret, Some("SOUNDCLOUD_CLIENT_ID"), "-", "SoundCloud client ID"),
    opt("keys.soundcloud_client_secret", Kind::Secret, Some("SOUNDCLOUD_CLIENT_SECRET"), "-", "SoundCloud client secret"),
    opt("keys.spotify_client_id", Kind::Secret, Some("SPOTIFY_CLIENT_ID"), "-", "Spotify client ID (playlist import)"),
    opt("keys.spotify_client_secret", Kind::Secret, Some("SPOTIFY_CLIENT_SECRET"), "-", "Spotify client secret"),
    opt("keys.lastfm_api_key", Kind::Secret, Some("LASTFM_API_KEY"), "-", "Last.fm API key (radio, similar artists)"),
];

fn find(key: &str) -> Result<&'static Opt, String> {
    let key = key.trim().to_ascii_lowercase().replace('-', "_");
    // `glyph config set lastfm_api_key ...` works without the `keys.` prefix.
    OPTS.iter()
        .find(|o| o.key == key || o.key.strip_prefix("keys.") == Some(key.as_str()))
        .ok_or_else(|| format!("unknown option `{key}`; `glyph config list` shows all options"))
}

fn parse(opt: &Opt, raw: &str) -> Result<toml::Value, String> {
    let raw = raw.trim();
    match opt.kind {
        Kind::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Ok(true.into()),
            "false" | "off" | "no" | "0" => Ok(false.into()),
            _ => Err(format!("{}: use true or false", opt.key)),
        },
        Kind::Int => raw.parse::<u32>().map(|n| toml::Value::Integer(n.into())).map_err(|_| format!("{}: use a whole number", opt.key)),
        Kind::Text | Kind::Secret => {
            if raw.is_empty() {
                return Err(format!("{}: empty value; use `glyph config unset {}`", opt.key, opt.key));
            }
            match opt.key {
                "bind" if raw.parse::<std::net::SocketAddr>().is_err() => Err("bind: use address:port, e.g. 0.0.0.0:8787".into()),
                "public_url" if !(raw.starts_with("https://") || raw.starts_with("http://")) => Err("public_url: start with https://".into()),
                "tls.email" if !raw.contains('@') => Err("tls.email: not an e-mail address".into()),
                "update_repo" if raw.split('/').count() != 2 => Err("update_repo: use owner/name".into()),
                _ => Ok(raw.trim_end_matches('/').to_string().into()),
            }
        }
    }
}

fn load(path: &Path) -> Result<toml::Table, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => s.parse::<toml::Table>().map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn lookup<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    match key.split_once('.') {
        Some((section, rest)) => table.get(section)?.as_table()?.get(rest),
        None => table.get(key),
    }
}

fn store(table: &mut toml::Table, key: &str, value: Option<toml::Value>) {
    match key.split_once('.') {
        Some((section, rest)) => {
            let sub = table.entry(section).or_insert_with(|| toml::Value::Table(toml::Table::new()));
            let Some(sub) = sub.as_table_mut() else { return };
            match value {
                Some(v) => {
                    sub.insert(rest.into(), v);
                }
                None => {
                    sub.remove(rest);
                }
            }
            if sub.is_empty() {
                table.remove(section);
            }
        }
        None => match value {
            Some(v) => {
                table.insert(key.into(), v);
            }
            None => {
                table.remove(key);
            }
        },
    }
}

/// Validates the edited table against the real config type, then writes it
/// owner-only like `setup` does.
fn save(path: &Path, table: &toml::Table) -> Result<(), String> {
    let text = toml::to_string(table).map_err(|e| e.to_string())?;
    let cfg: FileConfig = toml::from_str(&text).map_err(|e| format!("invalid config: {e}"))?;
    for warning in settings::save(path, &cfg)? {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn shown(opt: &Opt, v: &toml::Value) -> String {
    match (opt.kind, v) {
        (Kind::Secret, _) => "set".into(),
        (_, toml::Value::String(s)) => s.clone(),
        _ => v.to_string(),
    }
}

fn env_value(opt: &Opt) -> Option<(&'static str, String)> {
    let name = opt.env?;
    std::env::var(name).ok().filter(|v| !v.trim().is_empty()).map(|v| (name, v))
}

/// Applies `set` / `unset` to the file and returns what to print.
fn change(path: &Path, key: &str, value: Option<&str>) -> Result<String, String> {
    let opt = find(key)?;
    let mut table = load(path)?;
    let value = value.map(|v| parse(opt, v)).transpose()?;
    let line = match &value {
        Some(v) => format!("{} = {}", opt.key, shown(opt, v)),
        None => format!("{} unset (default: {})", opt.key, opt.default),
    };
    store(&mut table, opt.key, value);
    save(path, &table)?;
    let mut out = format!("{line}\nsaved to {}. Restart the server to apply.", path.display());
    if let Some((name, _)) = env_value(opt) {
        out.push_str(&format!("\nnote: the environment variable {name} is set and overrides this."));
    }
    Ok(out)
}

fn list(path: &Path) -> Result<String, String> {
    let table = load(path)?;
    let width = OPTS.iter().map(|o| o.key.len()).max().unwrap_or(0);
    let mut out = format!("{}\n\n", path.display());
    for o in OPTS {
        let (value, source) = match (env_value(o), lookup(&table, o.key)) {
            (Some((name, v)), _) => (if o.kind == Kind::Secret { "set".into() } else { v }, format!("env {name}")),
            (None, Some(v)) => (shown(o, v), "server.toml".into()),
            (None, None) => (if o.kind == Kind::Secret { "not set".into() } else { o.default.into() }, "default".into()),
        };
        out.push_str(&format!("{:width$}  {value}  [{source}]\n{:width$}  {}\n", o.key, "", o.help));
    }
    out.push_str("\nChange with `glyph config set <option> <value>`, reset with `glyph config unset <option>`.");
    Ok(out)
}

fn get(path: &Path, key: &str) -> Result<String, String> {
    let opt = find(key)?;
    if let Some((name, v)) = env_value(opt) {
        return Ok(if opt.kind == Kind::Secret { format!("set (env {name})") } else { v });
    }
    Ok(match lookup(&load(path)?, opt.key) {
        Some(v) => shown(opt, v),
        None if opt.kind == Kind::Secret => "not set".into(),
        None => opt.default.into(),
    })
}

pub fn run(cmd: ConfigCmd, path: &Path) -> Result<(), String> {
    let out = match cmd {
        ConfigCmd::Path => path.display().to_string(),
        ConfigCmd::List => list(path)?,
        ConfigCmd::Get { key } => get(path, &key)?,
        ConfigCmd::Unset { key } => change(path, &key, None)?,
        ConfigCmd::Set { key, value } => {
            let opt = find(&key)?;
            let value = match value {
                Some(v) => v,
                None if opt.kind == Kind::Secret => dialoguer::Password::new()
                    .with_prompt(opt.key)
                    .interact()
                    .map_err(|e| e.to_string())?,
                None => return Err(format!("usage: glyph config set {} <value>", opt.key)),
            };
            change(path, &key, Some(&value))?
        }
    };
    println!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_unset_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        change(&path, "rate-limit-per-min", Some("120")).unwrap();
        change(&path, "tls.domain", Some("glyph.example.com")).unwrap();
        change(&path, "lastfm_api_key", Some("SECRETKEY")).unwrap();
        change(&path, "require_account", Some("on")).unwrap();
        let cfg = settings::load_file(&path, true).unwrap();
        assert_eq!(cfg.rate_limit_per_min, Some(120));
        assert_eq!(cfg.tls.domain.as_deref(), Some("glyph.example.com"));
        assert_eq!(cfg.keys.lastfm_api_key.as_deref(), Some("SECRETKEY"));
        assert_eq!(cfg.require_account, Some(true));
        assert_eq!(get(&path, "tls.domain").unwrap(), "glyph.example.com");
        assert!(!list(&path).unwrap().contains("SECRETKEY"));
        assert!(!get(&path, "keys.lastfm_api_key").unwrap().contains("SECRET"));
        change(&path, "tls.domain", None).unwrap();
        let cfg = settings::load_file(&path, true).unwrap();
        assert!(cfg.tls.domain.is_none());
        assert_eq!(cfg.rate_limit_per_min, Some(120));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        assert!(change(&path, "nope", Some("1")).is_err());
        assert!(change(&path, "trust_proxy", Some("maybe")).is_err());
        assert!(change(&path, "cache_entries", Some("-3")).is_err());
        assert!(change(&path, "bind", Some("localhost")).is_err());
        assert!(change(&path, "public_url", Some("glyph.example.com")).is_err());
        assert!(!path.exists(), "nothing is written for invalid input");
    }

    #[test]
    fn every_option_is_a_real_config_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        for o in OPTS {
            let v = match o.kind {
                Kind::Bool => "true",
                Kind::Int => "7",
                _ if o.key == "bind" => "127.0.0.1:1",
                _ if o.key == "public_url" => "https://x.example",
                _ if o.key == "tls.email" => "a@b.c",
                _ if o.key == "update_repo" => "a/b",
                _ => "value",
            };
            change(&path, o.key, Some(v)).unwrap_or_else(|e| panic!("{}: {e}", o.key));
            let text = std::fs::read_to_string(&path).unwrap();
            let leaf = o.key.rsplit('.').next().unwrap();
            assert!(text.contains(leaf), "{} was dropped by FileConfig", o.key);
        }
    }
}
