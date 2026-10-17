//! Settings from `server.toml` and environment variables (env wins).
//! Secrets use the same names as the Worker secrets.
//!
//! The file is written by `glyph-server setup` and by the admin API; it is
//! created readable by the current user only (0600 on Unix, owner-only ACL on
//! Windows).

use glyph_server_core::Secrets;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// Public URL clients use, e.g. `https://glyph.example.com` (doctor checks it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_entries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub youtube_daily_budget_units: Option<u64>,
    /// yt-dlp executable for YouTube search (default: found in `PATH`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ytdlp_path: Option<String>,
    /// Serve audio through `/v1/audio/...` instead of handing out provider URLs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_audio: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_max_pages: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_repo: Option<String>,
    pub admin: FileAdmin,
    pub tls: FileTls,
    pub keys: FileKeys,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileAdmin {
    /// Argon2id hash (never the password). No hash = admin API off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
}

impl std::fmt::Debug for FileAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileAdmin{..}")
    }
}

/// Built-in HTTPS with automatic Let's Encrypt certificates.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileTls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Where certificates are kept (default: `tls-cache` next to the config).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// Use the Let's Encrypt staging environment (for testing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staging: Option<bool>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileKeys {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub youtube_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jamendo_client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soundcloud_client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soundcloud_client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lastfm_api_key: Option<String>,
}

impl std::fmt::Debug for FileKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKeys{..}")
    }
}

impl FileKeys {
    pub fn from_secrets(s: &Secrets) -> Self {
        Self {
            youtube_api_key: s.youtube_api_key.clone(),
            jamendo_client_id: s.jamendo_client_id.clone(),
            soundcloud_client_id: s.soundcloud_client_id.clone(),
            soundcloud_client_secret: s.soundcloud_client_secret.clone(),
            spotify_client_id: s.spotify_client_id.clone(),
            spotify_client_secret: s.spotify_client_secret.clone(),
            lastfm_api_key: s.lastfm_api_key.clone(),
        }
    }

    /// Set a key by its `server.toml` name. False for unknown names.
    pub fn set(&mut self, name: &str, value: Option<String>) -> bool {
        let v = value.filter(|v| !v.is_empty());
        let slot = match name {
            "youtube_api_key" => &mut self.youtube_api_key,
            "jamendo_client_id" => &mut self.jamendo_client_id,
            "soundcloud_client_id" => &mut self.soundcloud_client_id,
            "soundcloud_client_secret" => &mut self.soundcloud_client_secret,
            "spotify_client_id" => &mut self.spotify_client_id,
            "spotify_client_secret" => &mut self.spotify_client_secret,
            "lastfm_api_key" => &mut self.lastfm_api_key,
            _ => return false,
        };
        *slot = v;
        true
    }
}

#[derive(Debug, Clone)]
pub struct TlsSettings {
    pub domain: String,
    pub email: Option<String>,
    pub cache_dir: PathBuf,
    pub staging: bool,
}

#[derive(Debug)]
pub struct Settings {
    pub bind: String,
    pub public_url: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub cache_entries: usize,
    pub trust_proxy: bool,
    pub rate_limit_per_min: u32,
    pub youtube_daily_budget_units: u64,
    /// Configured yt-dlp path, if any (the working one is resolved at startup).
    pub ytdlp_path: Option<String>,
    /// Whether `/v1/resolve` hands out `/v1/audio/...` URLs (default true).
    pub proxy_audio: bool,
    pub import_max_pages: u32,
    pub update_repo: String,
    pub admin_password_hash: Option<String>,
    pub tls: Option<TlsSettings>,
    pub secrets: Secrets,
    /// The file settings were read from / will be written to.
    pub config_path: PathBuf,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub const DEFAULT_CONFIG: &str = "server.toml";

/// Path used when `--config` is not given.
pub fn default_config_path() -> PathBuf {
    std::env::var_os("GLYPH_CONFIG").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG))
}

pub fn load_file(path: &Path, must_exist: bool) -> Result<FileConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if !must_exist && e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Serialize a config as TOML with a short header.
pub fn render(cfg: &FileConfig) -> Result<String, String> {
    let body = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    Ok(format!(
        "# glyph-server configuration. Written by `glyph-server setup` / the admin API.\n\
         # Contains secrets: keep this file private. Environment variables override it.\n\n{body}"
    ))
}

/// Write `contents` so that only the current user can read it. Returns
/// non-fatal warnings (e.g. when the Windows ACL could not be tightened).
pub fn write_private(path: &Path, contents: &str) -> Result<Vec<String>, String> {
    use std::io::Write;
    let mut warnings = Vec::new();
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().ok();
    }
    #[cfg(windows)]
    if let Err(w) = restrict_windows_acl(&tmp) {
        warnings.push(w);
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(warnings)
}

/// Owner-only ACL via `icacls` (removes inherited permissions).
#[cfg(windows)]
fn restrict_windows_acl(path: &Path) -> Result<(), String> {
    let user = std::env::var("USERNAME").map_err(|_| "could not determine the current user".to_string())?;
    let domain = std::env::var("USERDOMAIN").ok().filter(|d| !d.is_empty());
    let principal = match domain {
        Some(d) => format!("{d}\\{user}"),
        None => user,
    };
    let out = std::process::Command::new("icacls")
        .arg(path)
        // Owner plus SYSTEM (so the Windows service, which runs as SYSTEM, can read it).
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{principal}:(F)"))
        .args(["/grant:r", "*S-1-5-18:(F)"])
        .output()
        .map_err(|e| format!("could not run icacls to restrict file access: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("icacls could not restrict {} to your user; check its permissions manually", path.display()))
    }
}

/// True when other users can read the file (Unix only; Windows always false).
pub fn is_world_readable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map(|m| m.permissions().mode() & 0o077 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

pub fn save(path: &Path, cfg: &FileConfig) -> Result<Vec<String>, String> {
    write_private(path, &render(cfg)?)
}

impl Settings {
    pub fn resolve(file: FileConfig, cli_bind: Option<String>, config_path: PathBuf) -> Self {
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
        let tls_domain = env("GLYPH_TLS_DOMAIN").or(file.tls.domain);
        let base = config_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let tls = tls_domain.map(|domain| TlsSettings {
            domain,
            email: env("GLYPH_TLS_EMAIL").or(file.tls.email),
            cache_dir: file.tls.cache_dir.unwrap_or_else(|| base.join("tls-cache")),
            staging: file.tls.staging.unwrap_or(false),
        });
        let default_bind = if tls.is_some() { "0.0.0.0:443" } else { "0.0.0.0:8787" };
        Self {
            bind: cli_bind.or_else(|| env("GLYPH_BIND")).or(file.bind).unwrap_or_else(|| default_bind.into()),
            public_url: env("GLYPH_PUBLIC_URL").or(file.public_url),
            cache_dir: env("GLYPH_CACHE_DIR").map(PathBuf::from).or(file.cache_dir),
            cache_entries: env("GLYPH_CACHE_ENTRIES").and_then(|v| v.parse().ok()).or(file.cache_entries).unwrap_or(10_000),
            trust_proxy: env("GLYPH_TRUST_PROXY").map(|v| v == "1" || v == "true").or(file.trust_proxy).unwrap_or(false),
            rate_limit_per_min: env("GLYPH_RATE_LIMIT_PER_MIN").and_then(|v| v.parse().ok()).or(file.rate_limit_per_min).unwrap_or(60),
            youtube_daily_budget_units: env("YOUTUBE_DAILY_BUDGET_UNITS").and_then(|v| v.parse().ok()).or(file.youtube_daily_budget_units).unwrap_or(9000),
            ytdlp_path: env("YTDLP_PATH").or(file.ytdlp_path),
            proxy_audio: env("GLYPH_PROXY_AUDIO").map(|v| v != "0" && v != "false").or(file.proxy_audio).unwrap_or(true),
            import_max_pages: file.import_max_pages.unwrap_or(5),
            update_repo: env("GLYPH_SERVER_REPO").or(file.update_repo).unwrap_or_else(|| "glyphmusic/glyph-server".into()),
            admin_password_hash: env("ADMIN_PASSWORD_HASH").or(file.admin.password_hash),
            tls,
            secrets,
            config_path,
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
            [admin]
            password_hash = "HASHVALUE"
            "#,
        )
        .unwrap();
        assert!(!format!("{f:?}").contains("SECRETVALUE"));
        assert!(!format!("{f:?}").contains("HASHVALUE"));
        let s = Settings::resolve(f, None, "x/server.toml".into());
        assert_eq!(s.bind, "127.0.0.1:9000");
        assert_eq!(s.secrets.youtube_api_key.as_deref(), Some("SECRETVALUE"));
        assert!(!format!("{s:?}").contains("SECRETVALUE"));
        let s2 = Settings::resolve(FileConfig::default(), Some("0.0.0.0:1".into()), "server.toml".into());
        assert_eq!(s2.bind, "0.0.0.0:1");
    }

    #[test]
    fn tls_changes_default_bind_and_cache_dir() {
        let f: FileConfig = toml::from_str("[tls]\ndomain = \"glyph.example.com\"\nemail = \"a@b.c\"").unwrap();
        let s = Settings::resolve(f, None, PathBuf::from("/etc/glyph/server.toml"));
        let tls = s.tls.unwrap();
        assert_eq!(s.bind, "0.0.0.0:443");
        assert_eq!(tls.domain, "glyph.example.com");
        assert_eq!(tls.cache_dir, PathBuf::from("/etc/glyph/tls-cache"));
        assert!(!tls.staging);
    }

    #[test]
    fn config_roundtrip_through_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("server.toml");
        let mut cfg = FileConfig::default();
        cfg.bind = Some("127.0.0.1:8787".into());
        cfg.public_url = Some("https://glyph.example.com".into());
        cfg.admin.password_hash = Some("$argon2id$v=19$m=8,t=1,p=1$c2FsdHNhbHRzYWx0$aGFzaA".into());
        assert!(cfg.keys.set("youtube_api_key", Some("AIzaKEY".into())));
        assert!(cfg.keys.set("lastfm_api_key", Some("".into())));
        assert!(!cfg.keys.set("nope", Some("x".into())));
        save(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("youtube_api_key = \"AIzaKEY\""));
        assert!(!text.contains("lastfm_api_key"), "unset keys are not written:\n{text}");
        let back = load_file(&path, true).unwrap();
        assert_eq!(back.public_url.as_deref(), Some("https://glyph.example.com"));
        assert_eq!(back.keys.youtube_api_key.as_deref(), Some("AIzaKEY"));
        assert!(back.admin.password_hash.unwrap().starts_with("$argon2id$"));
        // overwrite keeps working and leaves no temp file behind
        save(&path, &back_default()).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());
        assert!(!is_world_readable(&path));
    }

    fn back_default() -> FileConfig {
        FileConfig { bind: Some("0.0.0.0:1".into()), ..Default::default() }
    }

    #[cfg(unix)]
    #[test]
    fn secrets_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        write_private(&path, "a = 1").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // an existing, too-open file is tightened on rewrite
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "a = 2").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn missing_file_is_default_unless_required() {
        let p = Path::new("definitely-missing-glyph.toml");
        assert!(load_file(p, false).is_ok());
        assert!(load_file(p, true).is_err());
    }
}
