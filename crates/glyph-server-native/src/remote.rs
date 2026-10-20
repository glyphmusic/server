//! `glyph remote …`: manage a running server (on this machine or
//! anywhere) through its admin API with the admin password.
//!
//! `remote login` exchanges the password for a token pair; only the refresh
//! token is kept, in the OS keychain (Windows Credential Manager, macOS
//! Keychain, Secret Service on Linux). Where no keychain exists it falls back
//! to a file readable only by the current user. The password itself is never
//! stored. Each command gets a fresh 15-minute access token.

use clap::{Args, Subcommand};
use glyph_server_core::admin::types::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "glyph-server-remote";

#[derive(Debug, Args)]
pub struct RemoteArgs {
    /// Server to talk to (default: the one from the last `remote login`)
    #[arg(long, global = true)]
    pub server: Option<String>,
    #[command(subcommand)]
    pub cmd: RemoteCmd,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCmd {
    /// Log in with the admin password (asked for, or --password-stdin)
    Login {
        /// Server URL, e.g. https://glyph.example.com or http://127.0.0.1:8787
        url: String,
        /// Read the password from standard input (scripts)
        #[arg(long)]
        password_stdin: bool,
    },
    /// Forget the stored login for this server
    Logout,
    /// Version, uptime, sources and limits
    Status,
    /// Show, set or test API keys
    Keys {
        #[command(subcommand)]
        cmd: Option<KeysCmd>,
    },
    /// Cache statistics or clear the cache
    Cache {
        #[command(subcommand)]
        cmd: Option<CacheCmd>,
    },
    /// Show or change rate limits
    Limits {
        /// Require a signed-in account for API and media access (true/false)
        #[arg(long)]
        require_account: Option<bool>,
        /// Requests per minute per client
        #[arg(long)]
        rate: Option<u32>,
        /// YouTube API units per day
        #[arg(long)]
        youtube_budget: Option<u64>,
        /// Max pages per playlist import
        #[arg(long)]
        import_pages: Option<u32>,
        /// Days companion audio stays in R2 (Worker)
        #[arg(long)]
        audio_days: Option<u32>,
        /// Total R2 storage ceiling in GB (Worker)
        #[arg(long = "r2-max-gb", visible_alias = "audio-max-gb")]
        audio_max_gb: Option<u32>,
        /// Independent D1 database ceiling in MB (Worker)
        #[arg(long)]
        d1_max_mb: Option<u32>,
    },
    /// Switch the yt-dlp companion (YouTube) on or off; no argument shows it
    Companion {
        #[command(subcommand)]
        cmd: Option<SwitchCmd>,
    },
    /// Let AI assistants (MCP) control signed-in apps through this server; no argument shows it
    Mcp {
        #[command(subcommand)]
        cmd: Option<SwitchCmd>,
    },
    /// Accounts: device limits are set here, never by the app
    Accounts {
        #[command(subcommand)]
        cmd: AccountsCmd,
    },
    /// Recent server log lines
    Logs {
        /// Number of lines
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
        /// Keep printing new lines
        #[arg(short, long)]
        follow: bool,
    },
    /// Check for or install a server update
    Update {
        /// Install it (the server restarts itself if it can)
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeysCmd {
    /// List keys (masked)
    List,
    /// Set a key, e.g. `keys set youtube_api_key` (the value is asked for hidden)
    Set {
        name: String,
        /// Value (avoid: it ends up in your shell history)
        value: Option<String>,
    },
    /// Remove a key
    Unset { name: String },
    /// Test the stored keys of a source, e.g. `keys test youtube`
    Test { provider: String },
}

#[derive(Debug, Subcommand)]
pub enum AccountsCmd {
    /// Show or change the device limit for accounts without their own
    DefaultLimit {
        /// New default (1-100)
        limit: Option<u8>,
    },
    /// Show an account: limit and devices
    Show { handle: String },
    /// Give one account its own device limit, or `default` to drop it
    Limit {
        handle: String,
        /// Number (1-100) or `default`
        limit: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SwitchCmd {
    On,
    Off,
    Status,
}

#[derive(Debug, Subcommand)]
pub enum CacheCmd {
    Stats,
    Clear,
}

// ------------------------------------------------------------------ storage

#[derive(Debug, Default, Serialize, Deserialize)]
struct Profile {
    /// Server of the last login.
    default: Option<String>,
    /// Refresh tokens that could not go into a keychain (url -> token).
    #[serde(default)]
    fallback_tokens: BTreeMap<String, String>,
}

fn profile_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("glyph-server").join("remote.toml")
}

fn load_profile() -> Profile {
    std::fs::read_to_string(profile_path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

fn save_profile(p: &Profile) -> Result<(), String> {
    let path = profile_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string(p).map_err(|e| e.to_string())?;
    crate::settings::write_private(&path, &text).map(|_| ())
}

fn store_token(url: &str, token: &str) -> Result<(), String> {
    match keyring::Entry::new(KEYRING_SERVICE, url).and_then(|e| e.set_password(token)) {
        Ok(()) => {
            let mut p = load_profile();
            p.fallback_tokens.remove(url);
            p.default = Some(url.to_string());
            save_profile(&p)
        }
        Err(e) => {
            eprintln!("note: no system keychain available ({e}); keeping the login in {}", profile_path().display());
            let mut p = load_profile();
            p.fallback_tokens.insert(url.to_string(), token.to_string());
            p.default = Some(url.to_string());
            save_profile(&p)
        }
    }
}

fn read_token(url: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, url)
        .and_then(|e| e.get_password())
        .ok()
        .or_else(|| load_profile().fallback_tokens.get(url).cloned())
}

fn forget_token(url: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, url) {
        let _ = e.delete_credential();
    }
    let mut p = load_profile();
    p.fallback_tokens.remove(url);
    if p.default.as_deref() == Some(url) {
        p.default = None;
    }
    let _ = save_profile(&p);
}

/// `https://x/`, `x:8787` -> canonical base URL without trailing slash.
pub fn normalize_url(u: &str) -> Result<String, String> {
    let u = u.trim().trim_end_matches('/');
    let with_scheme = if u.contains("://") { u.to_string() } else if is_local(u) { format!("http://{u}") } else { format!("https://{u}") };
    let parsed = url::Url::parse(&with_scheme).map_err(|_| format!("`{u}` is not a valid server address"))?;
    if !matches!(parsed.scheme(),"http"|"https") || !parsed.username().is_empty() || parsed.password().is_some() || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("use an HTTP(S) server URL without embedded credentials, query or fragment".into());
    }
    if parsed.scheme() == "http" && !is_local(parsed.host_str().unwrap_or("")) {
        return Err("the admin API only works over HTTPS (plain http is allowed for localhost only)".into());
    }
    Ok(with_scheme)
}

fn is_local(host: &str) -> bool {
    let h = host.split(':').next().unwrap_or(host);
    matches!(h, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

// ------------------------------------------------------------------ http

pub(crate) struct Client {
    http: reqwest::Client,
    base: String,
    access: String,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("glyph-server-remote/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client")
}

async fn error_of(r: reqwest::Response) -> String {
    let status = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<glyph_protocol::ApiError>(&body).map(|e| e.message).unwrap_or(body);
    match status {
        401 => format!("not logged in or the login expired ({msg}). Run `glyph remote login <url>`."),
        404 => format!("endpoint unavailable ({msg}); configure the admin API or update an older server/companion"),
        429 => format!("too many attempts: {msg}"),
        _ => format!("HTTP {status}: {msg}"),
    }
}

impl Client {
    pub(crate) async fn connect(server: Option<String>) -> Result<Self, String> {
        let base = match server {
            Some(s) => normalize_url(&s)?,
            None => load_profile().default.ok_or("no server yet. Run `glyph remote login <url>` first.")?,
        };
        let refresh = read_token(&base).ok_or_else(|| format!("not logged in to {base}. Run `glyph remote login {base}`."))?;
        let http = client();
        let r = http
            .post(format!("{base}/admin/v1/refresh"))
            .json(&RefreshRequest { refresh_token: refresh })
            .send()
            .await
            .map_err(|e| format!("cannot reach {base}: {}", e.without_url()))?;
        if !r.status().is_success() {
            return Err(error_of(r).await);
        }
        let pair: TokenPair = r.json().await.map_err(|e| e.to_string())?;
        // Rotated refresh token: keep the new one.
        store_token(&base, &pair.refresh_token)?;
        Ok(Self { http, base, access: pair.access_token })
    }

    async fn send<T: for<'de> Deserialize<'de>>(&self, req: reqwest::RequestBuilder) -> Result<T, String> {
        let r = req.bearer_auth(&self.access).send().await.map_err(|e| format!("cannot reach {}: {}", self.base, e.without_url()))?;
        if !r.status().is_success() {
            return Err(error_of(r).await);
        }
        r.json().await.map_err(|e| e.to_string())
    }

    pub(crate) async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, String> {
        self.send(self.http.get(format!("{}/admin/v1/{path}", self.base))).await
    }

    pub(crate) async fn post<B: Serialize, T: for<'de> Deserialize<'de>>(&self, path: &str, body: &B) -> Result<T, String> {
        let request=self.http.post(format!("{}/admin/v1/{path}", self.base)).json(body);
        let request=if path.contains("update") || path.contains("r-dlp") {request.timeout(std::time::Duration::from_secs(180))}else{request};
        self.send(request).await
    }

    async fn put<B: Serialize, T: for<'de> Deserialize<'de>>(&self, path: &str, body: &B) -> Result<T, String> {
        self.send(self.http.put(format!("{}/admin/v1/{path}", self.base)).json(body)).await
    }
}

fn ask_secret(prompt: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        dialoguer::Password::new().with_prompt(prompt).interact().map_err(|e| e.to_string())
    } else {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).map_err(|e| e.to_string())?;
        Ok(s.trim_end_matches(['\r', '\n']).to_string())
    }
}

fn uptime(s: u64) -> String {
    match s {
        s if s >= 86_400 => format!("{}d {}h", s / 86_400, s / 3600 % 24),
        s if s >= 3600 => format!("{}h {}m", s / 3600, s / 60 % 60),
        s => format!("{}m {}s", s / 60, s % 60),
    }
}

// ------------------------------------------------------------------ commands

pub async fn run(a: RemoteArgs) -> Result<(), String> {
    match a.cmd {
        RemoteCmd::Login { url, password_stdin } => {
            let base = normalize_url(&url)?;
            let password = if password_stdin {
                let mut s = String::new();
                std::io::stdin().read_line(&mut s).map_err(|e| e.to_string())?;
                s.trim_end_matches(['\r', '\n']).to_string()
            } else {
                ask_secret(&format!("Admin password for {base}"))?
            };
            let r = client()
                .post(format!("{base}/admin/v1/login"))
                .json(&LoginRequest { password })
                .send()
                .await
                .map_err(|e| format!("cannot reach {base}: {}", e.without_url()))?;
            if !r.status().is_success() {
                let status = r.status().as_u16();
                let msg = error_of(r).await;
                return Err(if status == 401 { "wrong password".into() } else { msg });
            }
            let pair: TokenPair = r.json().await.map_err(|e| e.to_string())?;
            store_token(&base, &pair.refresh_token)?;
            println!("Logged in to {base}. The login stays valid for 30 days of inactivity.");
            println!("Try: glyph remote status");
            Ok(())
        }
        RemoteCmd::Logout => {
            let base = match a.server {
                Some(s) => normalize_url(&s)?,
                None => load_profile().default.ok_or("not logged in anywhere")?,
            };
            if let Ok(c) = Client::connect(Some(base.clone())).await {
                let _: Result<serde_json::Value, _> = c.post("logout", &serde_json::json!({})).await;
            }
            forget_token(&base);
            println!("Logged out of {base}.");
            Ok(())
        }
        RemoteCmd::Status => {
            let c = Client::connect(a.server).await?;
            let s: StatusResponse = c.get("status").await?;
            println!("Server     {}", c.base);
            println!("Version    {} (protocol v{}, {})", s.server_version, s.protocol_version, s.platform);
            if let Some(u) = s.uptime_s {
                println!("Uptime     {}", uptime(u));
            }
            println!(
                "Limits     {} req/min per client · YouTube {} units/day · {} import pages",
                s.limits.rate_limit_per_min, s.limits.youtube_daily_budget_units, s.limits.import_max_pages
            );
            println!("\nSources");
            for p in s.providers {
                let state = match (p.enabled, p.skipped) {
                    (true, true) => "paused (error or quota, retries in a few minutes)",
                    (true, false) => "on",
                    (false, _) if p.needs_key => "off (no key: glyph remote keys set …)",
                    (false, _) => "off",
                };
                println!("  {:<18} {state}", p.name);
            }
            Ok(())
        }
        RemoteCmd::Keys { cmd } => {
            let c = Client::connect(a.server).await?;
            match cmd.unwrap_or(KeysCmd::List) {
                KeysCmd::List => {
                    let k: KeysResponse = c.get("keys").await?;
                    for k in k.keys {
                        println!("  {:<26} {}", k.name, if k.set { k.masked.unwrap_or_else(|| "set".into()) } else { "-".into() });
                    }
                    println!("\nSet one with: glyph remote keys set <NAME>");
                }
                KeysCmd::Set { name, value } => {
                    let name = name.to_lowercase();
                    let value = match value {
                        Some(v) => v,
                        None => ask_secret(&format!("Value for {name}"))?,
                    };
                    let r: SetKeysResponse = c.put("keys", &SetKeysRequest { values: BTreeMap::from([(name.clone(), value)]) }).await?;
                    for w in &r.warnings {
                        println!("warning: {w}");
                    }
                    println!("Saved {name}. It is active right away.");
                }
                KeysCmd::Unset { name } => {
                    let name = name.to_lowercase();
                    let _: SetKeysResponse = c.put("keys", &SetKeysRequest { values: BTreeMap::from([(name.clone(), String::new())]) }).await?;
                    println!("Removed {name}.");
                }
                KeysCmd::Test { provider } => {
                    let id = crate::setup::parse_provider(&provider).ok_or_else(|| format!("unknown source `{provider}` (e.g. youtube, jamendo, spotify, soundcloud, lastfm)"))?;
                    let r: TestResult = c.post("keys/test", &TestKeysRequest { provider: id, values: None }).await?;
                    println!("{}: {} {}", id.display_name(), if r.ok { "OK" } else { "FAILED" }, r.message);
                }
            }
            Ok(())
        }
        RemoteCmd::Cache { cmd } => {
            let c = Client::connect(a.server).await?;
            match cmd.unwrap_or(CacheCmd::Stats) {
                CacheCmd::Stats => {
                    let r: CacheStatsResponse = c.get("cache/stats").await?;
                    match r.stats {
                        Some(s) => println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default()),
                        None => println!("this server does not report cache statistics"),
                    }
                }
                CacheCmd::Clear => {
                    let r: CacheClearResponse = c.post("cache/clear", &serde_json::json!({})).await?;
                    println!("{}", if r.cleared { "Cache cleared." } else { "This server cannot clear its cache." });
                }
            }
            Ok(())
        }
        RemoteCmd::Limits { require_account, rate, youtube_budget, import_pages, audio_days, audio_max_gb, d1_max_mb } => {
            let c = Client::connect(a.server).await?;
            let mut l: Limits = c.get("limits").await?;
            if require_account.is_some() || rate.is_some() || youtube_budget.is_some() || import_pages.is_some() || audio_days.is_some() || audio_max_gb.is_some() || d1_max_mb.is_some() {
                if let Some(v) = require_account { l.require_account = v; }
                if let Some(v) = d1_max_mb { l.d1_max_mb = v; }
                if let Some(v) = audio_days {
                    l.audio_cache_days = v;
                }
                if let Some(v) = audio_max_gb {
                    l.audio_cache_max_gb = v;
                }
                if let Some(v) = rate {
                    l.rate_limit_per_min = v;
                }
                if let Some(v) = youtube_budget {
                    l.youtube_daily_budget_units = v;
                }
                if let Some(v) = import_pages {
                    l.import_max_pages = v;
                }
                l = c.put("limits", &l).await?;
                println!("Saved.");
            }
            println!("Account required               {}", l.require_account);
            println!("Requests per minute per client  {}", l.rate_limit_per_min);
            println!("YouTube units per day           {}", l.youtube_daily_budget_units);
            println!("Pages per playlist import       {}", l.import_max_pages);
            println!("yt-dlp companion                {}", if l.companion_enabled { "on" } else { "off" });
            println!("AI assistants (MCP relay)       {}", if l.mcp_relay { "on" } else { "off" });
            println!("Companion audio in R2 (days)    {}", l.audio_cache_days);
            println!("R2 total ceiling (GB)          {}", l.audio_cache_max_gb);
            println!("R2 working target (GB)         {:.3}", f64::from(l.audio_cache_max_gb) - 0.5);
            println!("D1 database ceiling (MB)       {}", l.d1_max_mb);
            println!("D1 working target (MB)         {}", l.d1_max_mb - 50.min(l.d1_max_mb/10));
            Ok(())
        }
        RemoteCmd::Mcp { cmd } => {
            let c = Client::connect(a.server).await?;
            let mut l: Limits = c.get("limits").await?;
            let want = match cmd {
                Some(SwitchCmd::On) => Some(true),
                Some(SwitchCmd::Off) => Some(false),
                Some(SwitchCmd::Status) | None => None,
            };
            if let Some(on) = want {
                l.mcp_relay = on;
                l = c.put("limits", &l).await?;
            }
            println!("mcp {}", if l.mcp_relay { "on (assistants sign in at /oauth/authorize, endpoint /mcp)" } else { "off (/mcp answers 404)" });
            Ok(())
        }
        RemoteCmd::Companion { cmd } => {
            let c = Client::connect(a.server).await?;
            let mut l: Limits = c.get("limits").await?;
            let want = match cmd {
                Some(SwitchCmd::On) => Some(true),
                Some(SwitchCmd::Off) => Some(false),
                Some(SwitchCmd::Status) | None => None,
            };
            if let Some(on) = want {
                l.companion_enabled = on;
                l = c.put("limits", &l).await?;
            }
            println!(
                "companion {}",
                if l.companion_enabled { "on" } else { "off (YouTube answers 403 Forbidden)" }
            );
            Ok(())
        }
        RemoteCmd::Accounts { cmd } => {
            let c = Client::connect(a.server).await?;
            match cmd {
                AccountsCmd::DefaultLimit { limit } => {
                    let d: AccountDefaults = match limit {
                        Some(n) => c.put("account-defaults", &AccountDefaults { device_limit: n }).await?,
                        None => c.get("account-defaults").await?,
                    };
                    println!("Devices per account (default)  {}", d.device_limit);
                }
                AccountsCmd::Show { handle } => print_account(&c.get(&format!("accounts/{}", handle_path(&handle))).await?),
                AccountsCmd::Limit { handle, limit } => {
                    let device_limit = parse_limit(&limit)?;
                    let acc: AdminAccount = c.put(&format!("accounts/{}", handle_path(&handle)), &AccountLimitRequest { device_limit }).await?;
                    print_account(&acc);
                }
            }
            Ok(())
        }
        RemoteCmd::Logs { lines, follow } => {
            let mut c = Client::connect(a.server.clone()).await?;
            let mut r: LogsResponse = c.get(&format!("logs?n={}", lines.clamp(1, 1000))).await?;
            for l in &r.lines {
                println!("{l}");
            }
            let mut renewed = std::time::Instant::now();
            while follow {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                // Access tokens last 15 minutes.
                if renewed.elapsed() > std::time::Duration::from_secs(600) {
                    c = Client::connect(a.server.clone()).await?;
                    renewed = std::time::Instant::now();
                }
                r = c.get(&format!("logs?n=1000&after={}", r.next)).await?;
                for l in &r.lines {
                    println!("{l}");
                }
            }
            Ok(())
        }
        RemoteCmd::Update { apply } => {
            let c = Client::connect(a.server).await?;
            if apply {
                let r: UpdateApplyResponse = c.post("update/apply", &serde_json::json!({})).await?;
                println!("{}", r.message);
                if r.restart_required {
                    println!("Restart the server to finish (a service restarts it automatically).");
                }
            } else {
                let r: UpdateInfo = c.post("update/check", &serde_json::json!({})).await?;
                println!("{}", r.message);
                if r.update_available {
                    println!("Install it with: glyph remote update --apply");
                }
            }
            Ok(())
        }
    }
}

/// `@Name` -> `name`, safe inside a URL path.
fn handle_path(h: &str) -> String {
    h.trim().trim_start_matches('@').to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.').collect()
}

fn parse_limit(v: &str) -> Result<Option<u8>, String> {
    if v.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    v.parse::<u8>().ok().filter(|n| (1..=100).contains(n)).map(Some).ok_or_else(|| format!("`{v}`: use a number from 1 to 100 or `default`"))
}

fn print_account(a: &AdminAccount) {
    let own = match a.device_limit_set {
        Some(_) => "own limit",
        None => "server default",
    };
    println!("@{}  ({})", a.handle, a.id);
    println!("Device limit  {} ({own})", a.device_limit);
    println!("Devices       {}", a.devices.len());
    for d in &a.devices {
        println!("  {:<28} {}", d.name, d.platform);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn account_gate_cli_requires_an_explicit_boolean() {
        use clap::Parser;
        #[derive(Parser)] struct Cli { #[command(flatten)] remote: super::RemoteArgs }
        for (value,expected) in [("true",true),("false",false)] {
            let cli = Cli::try_parse_from(["test","limits","--require-account",value]).unwrap();
            let super::RemoteCmd::Limits { require_account, .. } = cli.remote.cmd else { panic!("wrong command") };
            assert_eq!(require_account,Some(expected));
        }
        assert!(Cli::try_parse_from(["test","limits","--require-account"]).is_err());
    }
    #[test]
    fn storage_limits_have_independent_cli_flags_and_legacy_alias() {
        use clap::Parser;
        #[derive(Parser)] struct TestCli { #[command(flatten)] remote: super::RemoteArgs }
        for flag in ["--r2-max-gb", "--audio-max-gb"] {
            let args = TestCli::try_parse_from(["test", "limits", "--d1-max-mb", "250", flag, "20"]).unwrap();
            match args.remote.cmd {
                super::RemoteCmd::Limits { d1_max_mb, audio_max_gb, .. } => {
                    assert_eq!(d1_max_mb, Some(250));
                    assert_eq!(audio_max_gb, Some(20));
                }
                _ => panic!("wrong command"),
            }
        }
    }

    use super::*;

    #[test]
    fn account_arguments_are_checked() {
        assert_eq!(handle_path(" @Kasto "), "kasto");
        assert_eq!(handle_path("a/../b"), "a..b");
        assert_eq!(parse_limit("Default").unwrap(), None);
        assert_eq!(parse_limit("10").unwrap(), Some(10));
        assert!(parse_limit("0").is_err());
        assert!(parse_limit("101").is_err());
    }

    #[test]
    fn urls_are_normalised_and_https_enforced() {
        assert_eq!(normalize_url("glyph.example.com/").unwrap(), "https://glyph.example.com");
        assert_eq!(normalize_url("127.0.0.1:8787").unwrap(), "http://127.0.0.1:8787");
        assert_eq!(normalize_url("http://localhost:8787").unwrap(), "http://localhost:8787");
        assert!(normalize_url("http://glyph.example.com").is_err());
    }

    #[test]
    fn uptime_is_readable() {
        assert_eq!(uptime(59), "0m 59s");
        assert_eq!(uptime(3700), "1h 1m");
        assert_eq!(uptime(90_000), "1d 1h");
    }
}

/// Resolve a target without prompting or exposing a stored refresh token.
pub fn selected_server(explicit: Option<String>) -> Result<Option<String>,String> {
    explicit.or_else(||load_profile().default).map(|s|normalize_url(&s)).transpose()
}
