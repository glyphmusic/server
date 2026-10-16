//! Settings from `server.toml` and environment variables (env wins).
//! Secrets use the same names as the Worker secrets.

use glyph_server_core::Secrets;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub bind: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub cache_entries: Option<usize>,
    pub trust_proxy: Option<bool>,
    pub rate_limit_per_min: Option<u32>,
    pub youtube_daily_budget_units: Option<u64>,
    pub update_repo: Option<String>,
    pub keys: FileKeys,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct FileKeys {
    pub youtube_api_key: Option<String>,
    pub jamendo_client_id: Option<String>,
    pub soundcloud_client_id: Option<String>,
    pub soundcloud_client_secret: Option<String>,
    pub spotify_client_id: Option<String>,
    pub spotify_client_secret: Option<String>,
    pub lastfm_api_key: Option<String>,
}

impl std::fmt::Debug for FileKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKeys{..}")
    }
}

#[derive(Debug)]
pub struct Settings {
    pub bind: String,
    pub cache_dir: Option<PathBuf>,
    pub cache_entries: usize,
    pub trust_proxy: bool,
    pub rate_limit_per_min: u32,
    pub youtube_daily_budget_units: u64,
    pub update_repo: String,
    pub secrets: Secrets,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub fn load_file(path: Option<&Path>) -> Result<FileConfig, String> {
    let (p, explicit) = match path {
        Some(p) => (p.to_path_buf(), true),
        None => (PathBuf::from("server.toml"), false),
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

impl Settings {
    pub fn resolve(file: FileConfig, cli_bind: Option<String>) -> Self {
        let k = file.keys;
        let pick = |name: &str, f: Option<String>| env(name).or(f);
        let secrets = Secrets {
            youtube_api_key: pick("YOUTUBE_API_KEY", k.youtube_api_key),
            jamendo_client_id: pick("JAMENDO_CLIENT_ID", k.jamendo_client_id),
            soundcloud_client_id: pick("SOUNDCLOUD_CLIENT_ID", k.soundcloud_client_id),
            soundcloud_client_secret: pick("SOUNDCLOUD_CLIENT_SECRET", k.soundcloud_client_secret),
            spotify_client_id: pick("SPOTIFY_CLIENT_ID", k.spotify_client_id),
            spotify_client_secret: pick("SPOTIFY_CLIENT_SECRET", k.spotify_client_secret),
            lastfm_api_key: pick("LASTFM_API_KEY", k.lastfm_api_key),
        };
        Self {
            bind: cli_bind.or_else(|| env("GLYPH_BIND")).or(file.bind).unwrap_or_else(|| "0.0.0.0:8787".into()),
            cache_dir: env("GLYPH_CACHE_DIR").map(PathBuf::from).or(file.cache_dir),
            cache_entries: env("GLYPH_CACHE_ENTRIES").and_then(|v| v.parse().ok()).or(file.cache_entries).unwrap_or(10_000),
            trust_proxy: env("GLYPH_TRUST_PROXY").map(|v| v == "1" || v == "true").or(file.trust_proxy).unwrap_or(false),
            rate_limit_per_min: env("GLYPH_RATE_LIMIT_PER_MIN").and_then(|v| v.parse().ok()).or(file.rate_limit_per_min).unwrap_or(60),
            youtube_daily_budget_units: env("YOUTUBE_DAILY_BUDGET_UNITS").and_then(|v| v.parse().ok()).or(file.youtube_daily_budget_units).unwrap_or(9000),
            update_repo: env("GLYPH_SERVER_REPO").or(file.update_repo).unwrap_or_else(|| "glyphmusic/glyph-server".into()),
            secrets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_and_never_debug_prints_keys() {
        let f: FileConfig = toml::from_str(
            r#"
            bind = "127.0.0.1:9000"
            cache_dir = "cache"
            [keys]
            youtube_api_key = "SECRETVALUE"
            "#,
        )
        .unwrap();
        assert!(!format!("{f:?}").contains("SECRETVALUE"));
        let s = Settings::resolve(f, None);
        assert_eq!(s.bind, "127.0.0.1:9000");
        assert_eq!(s.secrets.youtube_api_key.as_deref(), Some("SECRETVALUE"));
        assert!(!format!("{s:?}").contains("SECRETVALUE"));
        let s2 = Settings::resolve(FileConfig::default(), Some("0.0.0.0:1".into()));
        assert_eq!(s2.bind, "0.0.0.0:1");
    }
}
