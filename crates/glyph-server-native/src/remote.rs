//! `glyph remote …`: manage a running server (on this machine or
//! anywhere) through its admin API with the admin password.
//!
//! `remote login` exchanges the password for a token pair; only the refresh
//! token is kept, in the OS keychain (Windows Credential Manager, macOS
//! Keychain, Secret Service on Linux). Where no keychain exists it falls back
//! to a file readable only by the current user. The password itself is never
//! stored. Each command gets a fresh 15-minute access token.
//!
//! Every read prints text, or JSON with `--json`. Changes that affect every
//! user of a server ask first on a terminal (`--yes` skips the question,
//! `--dry-run` only shows what would change).

use crate::exit::{self, CliError};
use clap::{Args, Subcommand};
use glyph_server_core::admin::types::*;
use glyph_server_core::bans::{Ban, BanKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "glyph-server-remote";

#[derive(Debug, Args)]
pub struct RemoteArgs {
    /// Server to talk to (default: the one from `remote use` / the last `remote login`)
    #[arg(long, global = true)]
    pub server: Option<String>,
    #[command(subcommand)]
    pub cmd: RemoteCmd,
}

/// Global flags of the CLI that change how a command runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    /// Machine-readable output.
    pub json: bool,
    /// Do not ask before changes.
    pub yes: bool,
    /// Show what would change, change nothing.
    pub dry_run: bool,
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
    /// Servers you are logged in to (* = default)
    List,
    /// Make a logged-in server the default for every `remote` command
    Use { url: String },
    /// Version, uptime, sources and limits
    Status,
    /// Accounts, active users, bans and cache of the server
    Stats,
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
        /// YouTube songs per account (or address, for guests) and day; 0 = no limit
        #[arg(long)]
        youtube_per_day: Option<u32>,
        /// Max pages per playlist import
        #[arg(long)]
        import_pages: Option<u32>,
        /// Maximum songs per playlist (1-50000)
        #[arg(long)]
        playlist_max_tracks: Option<usize>,
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
    /// Same as `glyph companion on|off|status --server <url>` (kept for scripts)
    #[command(hide = true)]
    Companion {
        #[command(subcommand)]
        cmd: Option<SwitchCmd>,
    },
    /// Let AI assistants (MCP) control signed-in apps through this server; no argument shows it
    Mcp {
        #[command(subcommand)]
        cmd: Option<SwitchCmd>,
    },
    /// Accounts: device limits and sign-outs are set here, never by the app
    Accounts {
        #[command(subcommand)]
        cmd: AccountsCmd,
    },
    /// Ban an account, a device, an IP address or range, hardware or an install
    Ban {
        #[command(subcommand)]
        cmd: BanCmd,
    },
    /// Lift bans: a ban id, @handle (its account and everything banned with it), IP/CIDR or hash
    Unban { target: String },
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
    /// Show an account: limit, devices, their ids and hardware hashes
    Show { handle: String },
    /// Give one account its own device limit, or `default` to drop it
    Limit {
        handle: String,
        /// Number (1-100) or `default`
        limit: String,
    },
    /// Sign out one device of an account (its id from `accounts show`), or all of them
    Revoke {
        handle: String,
        /// Device id; leave out to sign out every device
        device: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct BanOpts {
    /// Note for yourself (never shown to the banned user)
    #[arg(long)]
    reason: Option<String>,
    /// Lift the ban after this many days (default: until `glyph remote unban`)
    #[arg(long)]
    days: Option<u32>,
}

#[derive(Debug, Subcommand)]
pub enum BanCmd {
    /// Every ban in force
    List,
    /// An account, plus the last IP and the hardware of each of its devices
    Account {
        handle: String,
        /// Do not ban the addresses its devices used
        #[arg(long)]
        no_ip: bool,
        /// Do not ban the hardware of its devices
        #[arg(long)]
        no_hardware: bool,
        #[command(flatten)]
        opts: BanOpts,
    },
    /// One device of an account: its hardware and last IP (id from `accounts show`)
    Device {
        handle: String,
        device: String,
        #[command(flatten)]
        opts: BanOpts,
    },
    /// An IP address or CIDR range, e.g. 203.0.113.7 or 203.0.113.0/24
    Ip {
        address: String,
        #[command(flatten)]
        opts: BanOpts,
    },
    /// A hardware hash (from `accounts show`); survives reinstalling the app
    Hardware {
        hash: String,
        #[command(flatten)]
        opts: BanOpts,
    },
    /// An app installation id
    Install {
        id: String,
        #[command(flatten)]
        opts: BanOpts,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
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
    /// Server every `remote` command uses unless `--server` is given.
    default: Option<String>,
    /// Every server with a stored login.
    #[serde(default)]
    servers: Vec<String>,
    /// Refresh tokens that could not go into a keychain (url -> token).
    #[serde(default)]
    fallback_tokens: BTreeMap<String, String>,
}

fn profile_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("glyph-server").join("remote.toml")
}

fn load_profile() -> Profile {
    let mut p: Profile = std::fs::read_to_string(profile_path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
    // Profiles from before `servers` existed still know their default.
    if let Some(d) = p.default.clone() {
        if !p.servers.contains(&d) {
            p.servers.push(d);
        }
    }
    p
}

fn save_profile(p: &Profile) -> Result<(), String> {
    let path = profile_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string(p).map_err(|e| e.to_string())?;
    crate::settings::write_private(&path, &text).map(|_| ())
}

/// Stores the refresh token. `make_default`: a new login becomes the default;
/// a token rotated while using another server does not move it.
fn store_token(url: &str, token: &str, make_default: bool) -> Result<(), String> {
    let mut p = load_profile();
    match keyring::Entry::new(KEYRING_SERVICE, url).and_then(|e| e.set_password(token)) {
        Ok(()) => {
            p.fallback_tokens.remove(url);
        }
        Err(e) => {
            if !p.fallback_tokens.contains_key(url) {
                eprintln!("note: no system keychain available ({e}); keeping the login in {}", profile_path().display());
            }
            p.fallback_tokens.insert(url.to_string(), token.to_string());
        }
    }
    if !p.servers.iter().any(|s| s == url) {
        p.servers.push(url.to_string());
    }
    if make_default || p.default.is_none() {
        p.default = Some(url.to_string());
    }
    save_profile(&p)
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
    p.servers.retain(|s| s != url);
    if p.default.as_deref() == Some(url) {
        p.default = p.servers.first().cloned();
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

fn unreachable(base: &str, e: reqwest::Error) -> CliError {
    CliError::new(exit::UNREACHABLE, format!("cannot reach {base}: {}", e.without_url()))
        .hint(format!("Check the address and that the server runs: glyph check server --server {base}"))
}

async fn error_of(r: reqwest::Response) -> CliError {
    let status = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<glyph_protocol::ApiError>(&body).map(|e| e.message).unwrap_or(body);
    match status {
        401 => CliError::new(exit::DENIED, format!("not logged in or the login expired ({msg})")).hint("Log in again: glyph remote login <url>"),
        403 => CliError::new(exit::DENIED, format!("forbidden: {msg}")),
        404 => CliError::new(exit::NOT_FOUND, format!("not found: {msg}"))
            .hint("An older server may not know this command yet: glyph remote update"),
        429 => CliError::new(exit::GENERAL, format!("too many attempts: {msg}")),
        501 => CliError::new(exit::GENERAL, format!("this server cannot do that: {msg}")),
        _ => CliError::new(exit::GENERAL, format!("HTTP {status}: {msg}")),
    }
}

impl Client {
    pub(crate) async fn connect(server: Option<String>) -> Result<Self, CliError> {
        let base = match server {
            Some(s) => normalize_url(&s)?,
            None => load_profile().default.ok_or_else(|| {
                CliError::new(exit::NOT_LOGGED_IN, "no server yet").hint("Run `glyph remote login <url>` first.")
            })?,
        };
        let refresh = read_token(&base).ok_or_else(|| {
            CliError::new(exit::NOT_LOGGED_IN, format!("not logged in to {base}")).hint(format!("Run `glyph remote login {base}`."))
        })?;
        let http = client();
        let r = http
            .post(format!("{base}/admin/v1/refresh"))
            .json(&RefreshRequest { refresh_token: refresh })
            .send()
            .await
            .map_err(|e| unreachable(&base, e))?;
        if !r.status().is_success() {
            return Err(error_of(r).await);
        }
        let pair: TokenPair = r.json().await.map_err(|e| e.to_string())?;
        // Rotated refresh token: keep the new one.
        store_token(&base, &pair.refresh_token, false)?;
        Ok(Self { http, base, access: pair.access_token })
    }

    async fn send<T: for<'de> Deserialize<'de>>(&self, req: reqwest::RequestBuilder) -> Result<T, CliError> {
        let r = req.bearer_auth(&self.access).send().await.map_err(|e| unreachable(&self.base, e))?;
        if !r.status().is_success() {
            return Err(error_of(r).await);
        }
        r.json().await.map_err(|e| CliError::from(e.to_string()))
    }

    pub(crate) async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, CliError> {
        self.send(self.http.get(format!("{}/admin/v1/{path}", self.base))).await
    }

    pub(crate) async fn post<B: Serialize, T: for<'de> Deserialize<'de>>(&self, path: &str, body: &B) -> Result<T, CliError> {
        let request=self.http.post(format!("{}/admin/v1/{path}", self.base)).json(body);
        let request=if path.contains("update") || path.contains("r-dlp") {request.timeout(std::time::Duration::from_secs(180))}else{request};
        self.send(request).await
    }

    async fn put<B: Serialize, T: for<'de> Deserialize<'de>>(&self, path: &str, body: &B) -> Result<T, CliError> {
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

/// Prints `value` as JSON with `--json`, otherwise runs `text`.
fn emit<T: Serialize>(o: Opts, value: &T, text: impl FnOnce()) {
    if o.json {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
    } else {
        text();
    }
}

/// How many people a change reaches, for the confirmation question.
async fn reach(c: &Client) -> String {
    match c.get::<StatsResponse>("stats").await.ok().and_then(|s| s.accounts) {
        Some(a) => format!(" It affects every user of {} ({} accounts active in the last 7 days).", c.base, a.active_7d),
        None => format!(" It affects every user of {}.", c.base),
    }
}

/// Asks before a change. `Ok(false)` = dry run, nothing to do. Without a
/// terminal (scripts, agents) the change goes ahead; pass `--dry-run` to look first.
fn confirm(o: Opts, what: &str, reach: &str) -> Result<bool, CliError> {
    if o.dry_run {
        if !o.json {
            println!("dry run: would {what}. Nothing changed.");
        }
        return Ok(false);
    }
    if o.yes || !std::io::stdin().is_terminal() || o.json {
        return Ok(true);
    }
    let ok = dialoguer::Confirm::new()
        .with_prompt(format!("{}{reach} Continue?", capitalize(what)))
        .default(false)
        .interact()
        .map_err(|e| e.to_string())?;
    if ok { Ok(true) } else { Err(CliError::new(exit::CANCELLED, "cancelled, nothing changed")) }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn switch(cmd: Option<SwitchCmd>) -> Option<bool> {
    match cmd {
        Some(SwitchCmd::On) => Some(true),
        Some(SwitchCmd::Off) => Some(false),
        Some(SwitchCmd::Status) | None => None,
    }
}

// ------------------------------------------------------------------ commands

pub async fn run(a: RemoteArgs, o: Opts) -> Result<(), CliError> {
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
                .map_err(|e| unreachable(&base, e))?;
            if !r.status().is_success() {
                let status = r.status().as_u16();
                let e = error_of(r).await;
                return Err(if status == 401 { CliError::new(exit::DENIED, "wrong password") } else { e });
            }
            let pair: TokenPair = r.json().await.map_err(|e| e.to_string())?;
            store_token(&base, &pair.refresh_token, true)?;
            emit(o, &serde_json::json!({ "server": base, "default": true }), || {
                println!("Logged in to {base}. The login stays valid for 30 days of inactivity.");
                println!("Try: glyph remote status");
            });
            Ok(())
        }
        RemoteCmd::Logout => {
            let base = match a.server {
                Some(s) => normalize_url(&s)?,
                None => load_profile().default.ok_or_else(|| CliError::new(exit::NOT_LOGGED_IN, "not logged in anywhere"))?,
            };
            if let Ok(c) = Client::connect(Some(base.clone())).await {
                let _: Result<serde_json::Value, _> = c.post("logout", &serde_json::json!({})).await;
            }
            forget_token(&base);
            emit(o, &serde_json::json!({ "logged_out": base }), || println!("Logged out of {base}."));
            Ok(())
        }
        RemoteCmd::List => {
            let p = load_profile();
            emit(o, &serde_json::json!({ "default": p.default, "servers": p.servers }), || {
                if p.servers.is_empty() {
                    println!("Not logged in anywhere. Run `glyph remote login <url>`.");
                }
                for s in &p.servers {
                    println!("{} {s}", if p.default.as_deref() == Some(s.as_str()) { "*" } else { " " });
                }
            });
            Ok(())
        }
        RemoteCmd::Use { url } => {
            let base = normalize_url(&url)?;
            if read_token(&base).is_none() {
                return Err(CliError::new(exit::NOT_LOGGED_IN, format!("not logged in to {base}")).hint(format!("Run `glyph remote login {base}`.")));
            }
            let mut p = load_profile();
            p.default = Some(base.clone());
            save_profile(&p)?;
            emit(o, &serde_json::json!({ "default": base }), || println!("Default server: {base}"));
            Ok(())
        }
        RemoteCmd::Status => {
            let c = Client::connect(a.server).await?;
            let s: StatusResponse = c.get("status").await?;
            emit(o, &s, || print_status(&c.base, &s));
            Ok(())
        }
        RemoteCmd::Stats => {
            let c = Client::connect(a.server).await?;
            let s: StatsResponse = c.get("stats").await?;
            emit(o, &s, || print_stats(&s));
            Ok(())
        }
        RemoteCmd::Keys { cmd } => {
            let c = Client::connect(a.server).await?;
            match cmd.unwrap_or(KeysCmd::List) {
                KeysCmd::List => {
                    let k: KeysResponse = c.get("keys").await?;
                    emit(o, &k, || {
                        for k in &k.keys {
                            println!("  {:<26} {}", k.name, if k.set { k.masked.clone().unwrap_or_else(|| "set".into()) } else { "-".into() });
                        }
                        println!("\nSet one with: glyph remote keys set <NAME>");
                    });
                }
                KeysCmd::Set { name, value } => {
                    let name = name.to_lowercase();
                    if !confirm(o, &format!("set {name} on {}", c.base), "")? { return Ok(()); }
                    let value = match value {
                        Some(v) => v,
                        None => ask_secret(&format!("Value for {name}"))?,
                    };
                    let r: SetKeysResponse = c.put("keys", &SetKeysRequest { values: BTreeMap::from([(name.clone(), value)]) }).await?;
                    emit(o, &r, || {
                        for w in &r.warnings {
                            println!("warning: {w}");
                        }
                        println!("Saved {name}. It is active right away.");
                    });
                }
                KeysCmd::Unset { name } => {
                    let name = name.to_lowercase();
                    if !confirm(o, &format!("remove {name} from {} (its source stops working)", c.base), "")? { return Ok(()); }
                    let r: SetKeysResponse = c.put("keys", &SetKeysRequest { values: BTreeMap::from([(name.clone(), String::new())]) }).await?;
                    emit(o, &r, || println!("Removed {name}."));
                }
                KeysCmd::Test { provider } => {
                    let id = crate::setup::parse_provider(&provider)
                        .ok_or_else(|| CliError::new(exit::USAGE, format!("unknown source `{provider}` (e.g. youtube, jamendo, spotify, soundcloud, lastfm)")))?;
                    let r: TestResult = c.post("keys/test", &TestKeysRequest { provider: id, values: None }).await?;
                    emit(o, &r, || println!("{}: {} {}", id.display_name(), if r.ok { "OK" } else { "FAILED" }, r.message));
                }
            }
            Ok(())
        }
        RemoteCmd::Cache { cmd } => {
            let c = Client::connect(a.server).await?;
            match cmd.unwrap_or(CacheCmd::Stats) {
                CacheCmd::Stats => {
                    let r: CacheStatsResponse = c.get("cache/stats").await?;
                    emit(o, &r, || match &r.stats {
                        Some(s) => println!("{}", serde_json::to_string_pretty(s).unwrap_or_default()),
                        None => println!("this server does not report cache statistics"),
                    });
                }
                CacheCmd::Clear => {
                    if !confirm(o, &format!("clear the cache of {}", c.base), " Songs and searches load slower until it fills again.")? { return Ok(()); }
                    let r: CacheClearResponse = c.post("cache/clear", &serde_json::json!({})).await?;
                    emit(o, &r, || println!("{}", if r.cleared { "Cache cleared." } else { "This server cannot clear its cache." }));
                }
            }
            Ok(())
        }
        RemoteCmd::Limits { require_account, rate, youtube_budget, youtube_per_day, import_pages, playlist_max_tracks, audio_days, audio_max_gb, d1_max_mb } => {
            let c = Client::connect(a.server).await?;
            let old: Limits = c.get("limits").await?;
            let mut l = old.clone();
            if let Some(v) = require_account { l.require_account = v; }
            if let Some(v) = rate { l.rate_limit_per_min = v; }
            if let Some(v) = youtube_budget { l.youtube_daily_budget_units = v; }
            if let Some(v) = youtube_per_day { l.youtube_per_day = v; }
            if let Some(v) = import_pages { l.import_max_pages = v; }
            if let Some(v) = playlist_max_tracks { l.playlist_max_tracks = v; }
            if let Some(v) = audio_days { l.audio_cache_days = v; }
            if let Some(v) = audio_max_gb { l.audio_cache_max_gb = v; }
            if let Some(v) = d1_max_mb { l.d1_max_mb = v; }
            let l = save_limits(&c, o, &old, l).await?;
            emit(o, &l, || print_limits(&l));
            Ok(())
        }
        RemoteCmd::Mcp { cmd } => {
            let c = Client::connect(a.server).await?;
            let old: Limits = c.get("limits").await?;
            let l = match switch(cmd) {
                Some(on) => save_limits(&c, o, &old, Limits { mcp_relay: on, ..old.clone() }).await?,
                None => old,
            };
            emit(o, &serde_json::json!({ "mcp_relay": l.mcp_relay }), || {
                println!("mcp {}", if l.mcp_relay { "on (assistants sign in at /oauth/authorize, endpoint /mcp)" } else { "off (/mcp answers 404)" })
            });
            Ok(())
        }
        RemoteCmd::Companion { cmd } => companion(a.server, cmd, o).await,
        RemoteCmd::Accounts { cmd } => {
            let c = Client::connect(a.server).await?;
            match cmd {
                AccountsCmd::DefaultLimit { limit } => {
                    let d: AccountDefaults = match limit {
                        Some(n) => {
                            if !confirm(o, &format!("set the default device limit to {n}"), "")? { return Ok(()); }
                            c.put("account-defaults", &AccountDefaults { device_limit: n }).await?
                        }
                        None => c.get("account-defaults").await?,
                    };
                    emit(o, &d, || println!("Devices per account (default)  {}", d.device_limit));
                }
                AccountsCmd::Show { handle } => {
                    let acc: AdminAccount = c.get(&format!("accounts/{}", handle_path(&handle))).await?;
                    emit(o, &acc, || print_account(&acc));
                }
                AccountsCmd::Limit { handle, limit } => {
                    let device_limit = parse_limit(&limit)?;
                    if !confirm(o, &format!("set the device limit of @{} to {limit}", handle_path(&handle)), "")? { return Ok(()); }
                    let acc: AdminAccount = c.put(&format!("accounts/{}", handle_path(&handle)), &AccountLimitRequest { device_limit }).await?;
                    emit(o, &acc, || print_account(&acc));
                }
                AccountsCmd::Revoke { handle, device } => {
                    let what = match &device {
                        Some(d) => format!("sign out device {d} of @{}", handle_path(&handle)),
                        None => format!("sign out every device of @{}", handle_path(&handle)),
                    };
                    if !confirm(o, &what, " They can sign in again unless banned.")? { return Ok(()); }
                    let acc: AdminAccount = c.post(&format!("accounts/{}/revoke", handle_path(&handle)), &RevokeRequest { device }).await?;
                    emit(o, &acc, || print_account(&acc));
                }
            }
            Ok(())
        }
        RemoteCmd::Ban { cmd } => ban(a.server, cmd, o).await,
        RemoteCmd::Unban { target } => {
            let c = Client::connect(a.server).await?;
            let target = target.trim().to_string();
            let preview: BansResponse = c.post("bans/remove", &UnbanRequest { target: target.clone(), dry_run: true }).await?;
            if !o.json && !preview.changed.is_empty() {
                println!("Matching bans:");
                print_bans(&preview.changed);
            }
            if !confirm(o, &format!("lift {} ban(s)", preview.changed.len()), "")? {
                emit(o, &preview, || {});
                return Ok(());
            }
            let r: BansResponse = c.post("bans/remove", &UnbanRequest { target, dry_run: false }).await?;
            emit(o, &r, || println!("Lifted {} ban(s). They apply on every location within a minute.", r.changed.len()));
            Ok(())
        }
        RemoteCmd::Logs { lines, follow } => {
            let mut c = Client::connect(a.server.clone()).await?;
            let mut r: LogsResponse = c.get(&format!("logs?n={}", lines.clamp(1, 1000))).await?;
            let show = |r: &LogsResponse| {
                for l in &r.lines {
                    if o.json { println!("{}", serde_json::json!({ "line": l })); } else { println!("{l}"); }
                }
            };
            show(&r);
            let mut renewed = std::time::Instant::now();
            while follow {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                // Access tokens last 15 minutes.
                if renewed.elapsed() > std::time::Duration::from_secs(600) {
                    c = Client::connect(a.server.clone()).await?;
                    renewed = std::time::Instant::now();
                }
                r = c.get(&format!("logs?n=1000&after={}", r.next)).await?;
                show(&r);
            }
            Ok(())
        }
        RemoteCmd::Update { apply } => {
            let c = Client::connect(a.server).await?;
            if apply {
                if !confirm(o, &format!("install the latest server release on {} (it restarts)", c.base), "")? { return Ok(()); }
                let r: UpdateApplyResponse = c.post("update/apply", &serde_json::json!({})).await?;
                emit(o, &r, || {
                    println!("{}", r.message);
                    if r.restart_required {
                        println!("Restart the server to finish (a service restarts it automatically).");
                    }
                });
            } else {
                let r: UpdateInfo = c.post("update/check", &serde_json::json!({})).await?;
                emit(o, &r, || {
                    println!("{}", r.message);
                    if r.update_available {
                        println!("Install it with: glyph remote update --apply");
                    }
                });
            }
            Ok(())
        }
    }
}

/// Shows what changes, asks, then saves. Returns the limits in force.
async fn save_limits(c: &Client, o: Opts, old: &Limits, new: Limits) -> Result<Limits, CliError> {
    let changes = diff(old, &new);
    if changes.is_empty() {
        return Ok(new);
    }
    if !o.json {
        for (k, a, b) in &changes {
            println!("  {k}: {a} -> {b}");
        }
    }
    let what = format!("change {} on {}", changes.iter().map(|(k, _, _)| k.as_str()).collect::<Vec<_>>().join(", "), c.base);
    if !confirm(o, &what, &reach(c).await)? {
        return Ok(old.clone());
    }
    let saved: Limits = c.put("limits", &new).await?;
    if !o.json {
        println!("Saved. Active at once (other Cloudflare locations within 30 seconds).");
    }
    Ok(saved)
}

fn diff(old: &Limits, new: &Limits) -> Vec<(String, String, String)> {
    let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) = (serde_json::to_value(old), serde_json::to_value(new)) else { return Vec::new() };
    b.iter().filter(|(k, v)| a.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), a.get(k).map(|x| x.to_string()).unwrap_or_default(), v.to_string())).collect()
}

/// `glyph companion on|off|status --server <url>`: the server stops (or
/// starts) using its yt-dlp companion for everyone.
pub async fn companion(server: Option<String>, cmd: Option<SwitchCmd>, o: Opts) -> Result<(), CliError> {
    let c = Client::connect(server).await?;
    let old: Limits = c.get("limits").await?;
    let l = match switch(cmd) {
        Some(on) => save_limits(&c, o, &old, Limits { companion_enabled: on, ..old.clone() }).await?,
        None => old,
    };
    emit(o, &serde_json::json!({ "server": c.base, "companion_enabled": l.companion_enabled }), || {
        println!("companion {} ({})", if l.companion_enabled { "on" } else { "off: YouTube on this server answers 403" }, c.base)
    });
    Ok(())
}

async fn ban(server: Option<String>, cmd: BanCmd, o: Opts) -> Result<(), CliError> {
    let c = Client::connect(server).await?;
    let (mut req, what) = match cmd {
        BanCmd::List => {
            let r: BansResponse = c.get("bans").await?;
            emit(o, &r, || if r.bans.is_empty() { println!("No bans.") } else { print_bans(&r.bans) });
            return Ok(());
        }
        BanCmd::Account { handle, no_ip, no_hardware, opts } => {
            let h = handle_path(&handle);
            let what = format!("ban @{h}{}{}", if no_ip { "" } else { ", its addresses" }, if no_hardware { "" } else { " and its devices' hardware" });
            (BanRequest { ip: !no_ip, hardware: !no_hardware, ..request(BanTarget::Account, &h, opts) }, what)
        }
        BanCmd::Device { handle, device, opts } => {
            let h = handle_path(&handle);
            (BanRequest { device: Some(device.clone()), ..request(BanTarget::Device, &h, opts) }, format!("ban device {device} of @{h} (hardware and last address)"))
        }
        BanCmd::Ip { address, opts } => (request(BanTarget::Ip, &address, opts), format!("ban {address}")),
        BanCmd::Hardware { hash, opts } => (request(BanTarget::Hardware, &hash, opts), "ban this hardware".to_string()),
        BanCmd::Install { id, opts } => (request(BanTarget::Install, &id, opts), "ban this installation".to_string()),
    };
    req.dry_run = true;
    let preview: BansResponse = c.post("bans", &req).await?;
    if !o.json {
        if preview.changed.is_empty() {
            println!("Already banned.");
            return Ok(());
        }
        println!("New bans:");
        print_bans(&preview.changed);
    }
    if !confirm(o, &what, " Banned clients get 403 on every request.")? {
        emit(o, &preview, || {});
        return Ok(());
    }
    req.dry_run = false;
    let r: BansResponse = c.post("bans", &req).await?;
    emit(o, &r, || println!("Banned. Active at once here, on every location within a minute. Undo: glyph remote unban <id>"));
    Ok(())
}

fn request(target: BanTarget, value: &str, opts: BanOpts) -> BanRequest {
    BanRequest { target, value: value.trim().to_string(), device: None, reason: opts.reason, days: opts.days, ip: true, hardware: true, dry_run: false }
}

// ------------------------------------------------------------------ printing

fn print_status(base: &str, s: &StatusResponse) {
    println!("Server     {base}");
    println!("Version    {} (protocol v{}, {})", s.server_version, s.protocol_version, s.platform);
    if let Some(u) = s.uptime_s {
        println!("Uptime     {}", uptime(u));
    }
    println!(
        "Limits     {} req/min per client · YouTube {} units/day · {} import pages",
        s.limits.rate_limit_per_min, s.limits.youtube_daily_budget_units, s.limits.import_max_pages
    );
    println!("Companion  {}", if s.limits.companion_enabled { "on" } else { "off" });
    println!("\nSources");
    for p in &s.providers {
        let state = match (p.enabled, p.skipped) {
            (true, true) => "paused (error or quota, retries in a few minutes)",
            (true, false) => "on",
            (false, _) if p.needs_key => "off (no key: glyph remote keys set …)",
            (false, _) => "off",
        };
        println!("  {:<18} {state}", p.name);
    }
}

pub(crate) fn print_stats(s: &StatsResponse) {
    if let Some(u) = s.uptime_s {
        println!("Uptime              {}", uptime(u));
    }
    match &s.accounts {
        Some(a) => {
            println!("Accounts            {}", a.accounts);
            println!("Devices             {}", a.devices);
            println!("Active (1/7/30 d)   {} / {} / {}", a.active_1d, a.active_7d, a.active_30d);
        }
        None => println!("Accounts            not enabled on this server"),
    }
    println!("Bans                {}", s.bans);
    println!("Companion           {}", if s.companion_enabled { "on" } else { "off" });
    println!("YouTube per day     {}", if s.youtube_per_day == 0 { "no limit".to_string() } else { s.youtube_per_day.to_string() });
    if let Some(c) = &s.cache {
        println!("Cache               {}", serde_json::to_string(c).unwrap_or_default());
    }
}

fn print_limits(l: &Limits) {
    println!("Account required               {}", l.require_account);
    println!("Requests per minute per client  {}", l.rate_limit_per_min);
    println!("YouTube units per day           {}", l.youtube_daily_budget_units);
    println!("YouTube songs per user and day  {}", if l.youtube_per_day == 0 { "no limit".to_string() } else { l.youtube_per_day.to_string() });
    println!("Songs per playlist              {}", l.playlist_max_tracks);
    println!("Pages per playlist import       {}", l.import_max_pages);
    println!("yt-dlp companion                {}", if l.companion_enabled { "on" } else { "off" });
    println!("AI assistants (MCP relay)       {}", if l.mcp_relay { "on" } else { "off" });
    println!("Companion audio in R2 (days)    {}", l.audio_cache_days);
    println!("R2 total ceiling (GB)          {}", l.audio_cache_max_gb);
    println!("R2 working target (GB)         {:.3}", f64::from(l.audio_cache_max_gb) - 0.5);
    println!("D1 database ceiling (MB)       {}", l.d1_max_mb);
    println!("D1 working target (MB)         {}", l.d1_max_mb - 50.min(l.d1_max_mb/10));
}

fn print_bans(bans: &[Ban]) {
    for b in bans {
        let kind = match b.kind {
            BanKind::Account => "account",
            BanKind::Ip => "ip",
            BanKind::IpHash => "ip (hashed)",
            BanKind::Hardware => "hardware",
            BanKind::Install => "install",
        };
        let what = b.label.clone().unwrap_or_else(|| if b.kind == BanKind::Ip { b.value.clone() } else { format!("{}…", &b.value[..b.value.len().min(12)]) });
        let until = b.expires_at.map(|t| format!(" until {}", date(t))).unwrap_or_default();
        let reason = b.reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
        println!("  {:<11} {:<12} {what}{until}{reason}", b.id, kind);
    }
}

/// `2026-10-22` from Unix milliseconds.
fn date(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `@Name` -> `name`, safe inside a URL path.
fn handle_path(h: &str) -> String {
    h.trim().trim_start_matches('@').to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.').collect()
}

fn parse_limit(v: &str) -> Result<Option<u8>, CliError> {
    if v.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    v.parse::<u8>()
        .ok()
        .filter(|n| (1..=100).contains(n))
        .map(Some)
        .ok_or_else(|| CliError::new(exit::USAGE, format!("`{v}`: use a number from 1 to 100 or `default`")))
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
        println!("  {:<20} {:<24} {:<8} last seen {}", d.id, d.name, d.platform, date(d.last_seen));
        if let Some(h) = &d.hardware {
            println!("  {:<20} hardware {h}", "");
        }
    }
    if !a.devices.is_empty() {
        println!("\nSign one out: glyph remote accounts revoke {} <id>   Ban: glyph remote ban device {} <id>", a.handle, a.handle);
    }
}

// ------------------------------------------------------------------ config --server

/// `glyph config list|get|set|unset --server <url>`: the live settings of a
/// running server (its limits and keys) under the names `glyph config` uses.
pub async fn config(server: Option<String>, cmd: crate::config_cmd::ConfigCmd, o: Opts) -> Result<(), CliError> {
    use crate::config_cmd::ConfigCmd;
    let c = Client::connect(server).await?;
    let old: Limits = c.get("limits").await?;
    let serde_json::Value::Object(map) = serde_json::to_value(&old).map_err(|e| e.to_string())? else { return Err("unexpected limits".into()) };
    let unknown = |k: &str| CliError::new(exit::USAGE, format!("`{k}` is not a live setting")).hint(format!("Live settings: {} and keys.<name>", map.keys().cloned().collect::<Vec<_>>().join(", ")));
    match cmd {
        ConfigCmd::List => {
            let keys: KeysResponse = c.get("keys").await?;
            emit(o, &serde_json::json!({ "server": c.base, "limits": old, "keys": keys.keys }), || {
                for (k, v) in &map {
                    println!("{k:<28} {v}");
                }
                for k in &keys.keys {
                    println!("{:<28} {}", format!("keys.{}", k.name), if k.set { k.masked.clone().unwrap_or_else(|| "set".into()) } else { "-".into() });
                }
                println!("\nLive settings of {}: changes apply at once.", c.base);
            });
        }
        ConfigCmd::Get { key } => {
            let key = key.trim().to_ascii_lowercase().replace('-', "_");
            if let Some(name) = key.strip_prefix("keys.") {
                let keys: KeysResponse = c.get("keys").await?;
                let k = keys.keys.into_iter().find(|k| k.name == name).ok_or_else(|| unknown(&key))?;
                emit(o, &k, || println!("{}", if k.set { k.masked.clone().unwrap_or_else(|| "set".into()) } else { "-".into() }));
            } else {
                let v = map.get(&key).ok_or_else(|| unknown(&key))?;
                emit(o, &serde_json::json!({ (key.clone()): v }), || println!("{v}"));
            }
        }
        ConfigCmd::Set { key, value } => set_live(&c, o, &old, &map, &key, value, &unknown).await?,
        ConfigCmd::Unset { key } => {
            let key_n = key.trim().to_ascii_lowercase().replace('-', "_");
            if key_n.starts_with("keys.") {
                set_live(&c, o, &old, &map, &key, Some(String::new()), &unknown).await?;
            } else {
                let defaults = serde_json::to_value(Limits::default()).map_err(|e| e.to_string())?;
                let v = defaults.get(&key_n).ok_or_else(|| unknown(&key_n))?.to_string();
                set_live(&c, o, &old, &map, &key, Some(v), &unknown).await?;
            }
        }
        ConfigCmd::Path => return Err(CliError::new(exit::USAGE, "a running server has no local file; drop --server to see server.toml")),
    }
    Ok(())
}

async fn set_live(
    c: &Client,
    o: Opts,
    old: &Limits,
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<String>,
    unknown: &dyn Fn(&str) -> CliError,
) -> Result<(), CliError> {
    let key = key.trim().to_ascii_lowercase().replace('-', "_");
    if let Some(name) = key.strip_prefix("keys.") {
        let unset = value.as_deref() == Some("");
        if !confirm(o, &format!("{} {name} on {}", if unset { "remove" } else { "set" }, c.base), "")? { return Ok(()); }
        let value = match value {
            Some(v) => v,
            None => ask_secret(&format!("Value for {name}"))?,
        };
        let r: SetKeysResponse = c.put("keys", &SetKeysRequest { values: BTreeMap::from([(name.to_string(), value)]) }).await?;
        emit(o, &r, || println!("{} {name}. Active at once.", if unset { "Removed" } else { "Saved" }));
        return Ok(());
    }
    let current = map.get(&key).ok_or_else(|| unknown(&key))?;
    let raw = value.ok_or_else(|| CliError::new(exit::USAGE, format!("give a value: glyph config set {key} <value> --server <url>")))?;
    let parsed = match current {
        serde_json::Value::Bool(_) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => serde_json::Value::Bool(true),
            "false" | "off" | "no" | "0" => serde_json::Value::Bool(false),
            _ => return Err(CliError::new(exit::USAGE, format!("{key} takes true or false"))),
        },
        serde_json::Value::Number(_) => raw.trim().parse::<u64>().map(serde_json::Value::from).map_err(|_| CliError::new(exit::USAGE, format!("{key} takes a whole number")))?,
        _ => serde_json::Value::String(raw),
    };
    let mut next = map.clone();
    next.insert(key.clone(), parsed);
    let new: Limits = serde_json::from_value(serde_json::Value::Object(next)).map_err(|e| CliError::new(exit::USAGE, e.to_string()))?;
    let l = save_limits(c, o, old, new).await?;
    emit(o, &l, || {});
    Ok(())
}

// ------------------------------------------------------------------ status

/// `glyph status`: the CLI, the selected server and what runs on it.
pub async fn overview(server: Option<String>, o: Opts) -> Result<(), CliError> {
    let cli = env!("CARGO_PKG_VERSION");
    let base = selected_server(server.clone())?;
    let Some(base) = base else {
        emit(o, &serde_json::json!({ "cli": cli, "server": null }), || {
            println!("CLI        {cli}");
            println!("Server     none selected (glyph remote login <url>, or --server <url>)");
        });
        return Ok(());
    };
    let c = Client::connect(Some(base.clone())).await?;
    let status: StatusResponse = c.get("status").await?;
    let stats: Option<StatsResponse> = c.get("stats").await.ok();
    emit(o, &serde_json::json!({ "cli": cli, "server": base, "status": status, "stats": stats }), || {
        println!("CLI        {cli}");
        print_status(&base, &status);
        if let Some(s) = &stats {
            println!();
            print_stats(s);
        }
        println!("\nDeeper checks: glyph doctor (this machine), glyph check all --server {base}");
    });
    Ok(())
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
    fn playlist_limit_has_an_operator_cli_flag() {
        use clap::Parser;
        #[derive(Parser)] struct Cli { #[command(flatten)] remote: super::RemoteArgs }
        let cli = Cli::try_parse_from(["test", "limits", "--playlist-max-tracks", "8000", "--youtube-per-day", "200"]).unwrap();
        let super::RemoteCmd::Limits { playlist_max_tracks, youtube_per_day, .. } = cli.remote.cmd else { panic!("wrong command") };
        assert_eq!((playlist_max_tracks, youtube_per_day), (Some(8000), Some(200)));
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

    #[test]
    fn ban_commands_parse() {
        use clap::Parser;
        #[derive(Parser)] struct Cli { #[command(flatten)] remote: super::RemoteArgs }
        let cli = Cli::try_parse_from(["t", "ban", "account", "@Kasto", "--no-ip", "--days", "7", "--reason", "spam"]).unwrap();
        let super::RemoteCmd::Ban { cmd: super::BanCmd::Account { handle, no_ip, no_hardware, opts } } = cli.remote.cmd else { panic!("wrong command") };
        assert_eq!((handle.as_str(), no_ip, no_hardware, opts.days), ("@Kasto", true, false, Some(7)));
        assert!(Cli::try_parse_from(["t", "ban", "ip", "203.0.113.0/24"]).is_ok());
        assert!(Cli::try_parse_from(["t", "ban", "device", "kasto", "d_1"]).is_ok());
        assert!(Cli::try_parse_from(["t", "unban", "203.0.113.0/24"]).is_ok());
        assert!(Cli::try_parse_from(["t", "accounts", "revoke", "kasto"]).is_ok());
    }

    use super::*;

    #[test]
    fn account_arguments_are_checked() {
        assert_eq!(handle_path(" @Kasto "), "kasto");
        assert_eq!(handle_path("a/../b"), "a..b");
        assert_eq!(parse_limit("Default").unwrap(), None);
        assert_eq!(parse_limit("10").unwrap(), Some(10));
        assert_eq!(parse_limit("0").unwrap_err().code, exit::USAGE);
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
    fn uptime_and_dates_are_readable() {
        assert_eq!(uptime(59), "0m 59s");
        assert_eq!(uptime(3700), "1h 1m");
        assert_eq!(uptime(90_000), "1d 1h");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_792_627_200_000), "2026-10-22");
    }

    #[test]
    fn limit_changes_are_listed() {
        let old = Limits::default();
        let new = Limits { rate_limit_per_min: 30, youtube_per_day: 200, ..old.clone() };
        let d = diff(&old, &new);
        assert_eq!(d.len(), 2);
        assert!(d.iter().any(|(k, a, b)| k == "rate_limit_per_min" && a == "60" && b == "30"));
        assert!(confirm(Opts { dry_run: true, ..Default::default() }, "x", "").is_ok_and(|go| !go));
        assert!(confirm(Opts { yes: true, ..Default::default() }, "x", "").unwrap());
    }
}

/// Resolve a target without prompting or exposing a stored refresh token.
pub fn selected_server(explicit: Option<String>) -> Result<Option<String>,String> {
    explicit.or_else(||load_profile().default).map(|s|normalize_url(&s)).transpose()
}
