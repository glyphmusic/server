//! `glyph-server setup`: interactive wizard (or `--non-interactive` for
//! scripts/Docker) that writes `server.toml`.

use crate::settings::{self, FileConfig, FileKeys, FileTls};
use clap::{Args, ValueEnum};
use dialoguer::{Confirm, Input, Password, Select};
use glyph_protocol::ProviderId;
use glyph_server_core::admin::{auth, keytest};
use glyph_server_core::{HttpClient, Secrets};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Only this machine can connect.
    Local,
    /// Behind a Cloudflare Tunnel (no port forwarding needed).
    Tunnel,
    /// Behind your own reverse proxy (Caddy, nginx, ...).
    Proxy,
    /// Built-in HTTPS with automatic Let's Encrypt certificates.
    Tls,
}

#[derive(Debug, Args, Clone, Default)]
pub struct SetupArgs {
    /// Do not ask questions; take everything from flags and environment variables.
    #[arg(long)]
    pub non_interactive: bool,
    /// Overwrite an existing config file.
    #[arg(long)]
    pub force: bool,
    /// Do not contact the providers to test keys.
    #[arg(long)]
    pub skip_tests: bool,
    /// Address to listen on (default depends on the mode).
    #[arg(long)]
    pub listen: Option<String>,
    /// Public URL clients use, e.g. https://glyph.example.com
    #[arg(long)]
    pub public_url: Option<String>,
    /// How the server is reached from outside (non-interactive; default local).
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,
    /// Domain for built-in HTTPS (mode tls).
    #[arg(long)]
    pub tls_domain: Option<String>,
    /// Contact e-mail for Let's Encrypt (mode tls).
    #[arg(long)]
    pub tls_email: Option<String>,
    /// File containing the admin password (non-interactive). Alternative: env GLYPH_ADMIN_PASSWORD
    /// or a ready-made hash in env GLYPH_ADMIN_PASSWORD_HASH.
    #[arg(long)]
    pub admin_password_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Answers {
    pub listen: Option<String>,
    pub public_url: Option<String>,
    pub mode: Mode,
    pub tls_domain: Option<String>,
    pub tls_email: Option<String>,
    pub admin_hash: Option<String>,
    pub secrets: Secrets,
}

/// Where each key comes from, in one sentence plus a link.
pub struct ProviderHelp {
    pub id: ProviderId,
    pub what: &'static str,
    pub url: &'static str,
    /// (server.toml key name, label)
    pub fields: &'static [(&'static str, &'static str)],
}

pub const PROVIDER_HELP: [ProviderHelp; 5] = [
    ProviderHelp {
        id: ProviderId::YouTube,
        what: "Optional, only for importing YouTube playlists. Searching YouTube needs no key, it uses yt-dlp.",
        url: "https://console.cloud.google.com/apis/library/youtube.googleapis.com",
        fields: &[("youtube_api_key", "YouTube API key (playlist import only)")],
    },
    ProviderHelp {
        id: ProviderId::Jamendo,
        what: "Create a free Jamendo developer app and copy its client id.",
        url: "https://devportal.jamendo.com/",
        fields: &[("jamendo_client_id", "Jamendo client id")],
    },
    ProviderHelp {
        id: ProviderId::SoundCloud,
        what: "Register a SoundCloud app to get a client id and client secret (SoundCloud approves apps manually).",
        url: "https://soundcloud.com/you/apps",
        fields: &[("soundcloud_client_id", "SoundCloud client id"), ("soundcloud_client_secret", "SoundCloud client secret")],
    },
    ProviderHelp {
        id: ProviderId::Spotify,
        what: "Create a Spotify app (no Premium needed) and copy its client id and secret; used for metadata only.",
        url: "https://developer.spotify.com/dashboard",
        fields: &[("spotify_client_id", "Spotify client id"), ("spotify_client_secret", "Spotify client secret")],
    },
    ProviderHelp {
        id: ProviderId::LastFm,
        what: "Create a free Last.fm API account and copy the \"API key\" (not the shared secret).",
        url: "https://www.last.fm/api/account/create",
        fields: &[("lastfm_api_key", "Last.fm API key")],
    },
];

pub fn help_for(id: ProviderId) -> Option<&'static ProviderHelp> {
    PROVIDER_HELP.iter().find(|h| h.id == id)
}

/// Provider names accepted on the command line (`remote keys set youtube`).
pub fn parse_provider(name: &str) -> Option<ProviderId> {
    match name.trim().to_ascii_lowercase().as_str() {
        "youtube" | "yt" => Some(ProviderId::YouTube),
        "jamendo" => Some(ProviderId::Jamendo),
        "soundcloud" | "sc" => Some(ProviderId::SoundCloud),
        "spotify" => Some(ProviderId::Spotify),
        "lastfm" | "last.fm" => Some(ProviderId::LastFm),
        _ => None,
    }
}

pub fn gen_salt() -> Result<[u8; 16], String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| format!("no secure random numbers available: {e}"))?;
    Ok(salt)
}

pub fn hash_new_password(pw: &str) -> Result<String, String> {
    if let Some(p) = auth::password_problem(pw) {
        return Err(p.to_string());
    }
    auth::hash_password(pw, &gen_salt()?)
}

pub fn validate_public_url(u: &str) -> Result<String, String> {
    let url = url::Url::parse(u.trim()).map_err(|_| "that is not a valid URL (example: https://glyph.example.com)".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("the URL must start with http:// or https://".into());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

pub fn validate_listen(a: &str) -> Result<String, String> {
    a.trim()
        .parse::<std::net::SocketAddr>()
        .map(|_| a.trim().to_string())
        .map_err(|_| "use the form address:port, for example 0.0.0.0:8787 or 127.0.0.1:8787".into())
}

pub fn build_config(a: &Answers) -> FileConfig {
    let mut cfg = FileConfig::default();
    cfg.public_url = a.public_url.clone();
    cfg.keys = FileKeys::from_secrets(&a.secrets);
    cfg.admin.password_hash = a.admin_hash.clone();
    match a.mode {
        Mode::Local => {
            cfg.bind = Some(a.listen.clone().unwrap_or_else(|| "127.0.0.1:8787".into()));
        }
        // A proxy/tunnel connects from this machine: listen on loopback only,
        // otherwise anybody could fake the forwarding headers we trust.
        Mode::Tunnel | Mode::Proxy => {
            cfg.bind = Some(a.listen.clone().unwrap_or_else(|| "127.0.0.1:8787".into()));
            cfg.trust_proxy = Some(true);
        }
        Mode::Tls => {
            cfg.bind = Some(a.listen.clone().unwrap_or_else(|| "0.0.0.0:443".into()));
            cfg.tls = FileTls { domain: a.tls_domain.clone(), email: a.tls_email.clone(), ..Default::default() };
            if cfg.public_url.is_none() {
                if let Some(d) = &a.tls_domain {
                    cfg.public_url = Some(format!("https://{d}"));
                }
            }
        }
    }
    cfg
}

/// Collect answers from flags/env (no prompts).
pub fn answers_from_env(args: &SetupArgs, env: &dyn Fn(&str) -> Option<String>) -> Result<Answers, String> {
    let mode = args.mode.unwrap_or(Mode::Local);
    let public_url = args.public_url.clone().or_else(|| env("GLYPH_PUBLIC_URL")).map(|u| validate_public_url(&u)).transpose()?;
    let listen = args.listen.clone().or_else(|| env("GLYPH_BIND")).map(|l| validate_listen(&l)).transpose()?;
    let (tls_domain, tls_email) = (
        args.tls_domain.clone().or_else(|| env("GLYPH_TLS_DOMAIN")),
        args.tls_email.clone().or_else(|| env("GLYPH_TLS_EMAIL")),
    );
    if mode == Mode::Tls && tls_domain.is_none() {
        return Err("mode tls needs --tls-domain (or GLYPH_TLS_DOMAIN)".into());
    }
    let admin_hash = if let Some(h) = env("GLYPH_ADMIN_PASSWORD_HASH") {
        if !auth::is_valid_hash(&h) {
            return Err("GLYPH_ADMIN_PASSWORD_HASH is not an Argon2id hash (create one with `glyph-server hash-password`)".into());
        }
        Some(h)
    } else if let Some(f) = &args.admin_password_file {
        let pw = std::fs::read_to_string(f).map_err(|e| format!("cannot read {}: {e}", f.display()))?;
        Some(hash_new_password(pw.trim_end_matches(['\r', '\n']))?)
    } else if let Some(pw) = env("GLYPH_ADMIN_PASSWORD") {
        Some(hash_new_password(&pw)?)
    } else {
        None
    };
    let g = |n: &str| env(n);
    let secrets = Secrets {
        youtube_api_key: g("YOUTUBE_API_KEY"),
        jamendo_client_id: g("JAMENDO_CLIENT_ID"),
        soundcloud_client_id: g("SOUNDCLOUD_CLIENT_ID"),
        soundcloud_client_secret: g("SOUNDCLOUD_CLIENT_SECRET"),
        spotify_client_id: g("SPOTIFY_CLIENT_ID"),
        spotify_client_secret: g("SPOTIFY_CLIENT_SECRET"),
        lastfm_api_key: g("LASTFM_API_KEY"),
    }
    .normalized();
    Ok(Answers { listen, public_url, mode, tls_domain, tls_email, admin_hash, secrets })
}

fn prompt_err(e: dialoguer::Error) -> String {
    format!("input cancelled or not possible here ({e}). Without a terminal, use `glyph-server setup --non-interactive`.")
}

/// Ask for the keys of every provider the user wants, testing each live.
/// `secrets` is updated in place. Shared with `cloudflare setup`.
pub async fn ask_provider_keys(http: &dyn HttpClient, secrets: &mut Secrets, run_tests: bool) -> Result<(), String> {
    println!("\nProviders. Audius, Deezer, Internet Archive and LRCLIB need no key and are always on.");
    println!("The others are optional: press Enter to skip any you do not have; you can add them later\n(`glyph-server remote keys set <provider>`).");
    for help in PROVIDER_HELP.iter() {
        let name = help.id.display_name();
        println!("\n{name}: {}\n  Get it here: {}", help.what, help.url);
        let have = help.fields.iter().all(|(k, _)| secrets.get(k).is_some());
        let prompt = if have { format!("{name} is already set. Replace it?") } else { format!("Set up {name} now?") };
        if !Confirm::new().with_prompt(prompt).default(false).interact().map_err(prompt_err)? {
            continue;
        }
        loop {
            let mut candidate = secrets.clone();
            for (key, label) in help.fields {
                let v = Password::new().with_prompt(format!("{label} (input is hidden)")).allow_empty_password(true).interact().map_err(prompt_err)?;
                if !v.trim().is_empty() {
                    candidate.set(key, Some(v));
                }
            }
            if help.fields.iter().any(|(k, _)| candidate.get(k).is_none()) {
                println!("Skipped {name}: not all values were entered.");
                break;
            }
            if !run_tests {
                *secrets = candidate;
                break;
            }
            print!("Testing {name} ... ");
            let o = keytest::test_provider(http, &candidate, help.id).await;
            println!("{}", if o.ok { "OK" } else { "FAILED" });
            println!("  {}", o.message);
            if o.ok {
                *secrets = candidate;
                break;
            }
            let choice = Select::new()
                .with_prompt("What now?")
                .items(&["Enter the key again", "Save it anyway", "Skip this provider"])
                .default(0)
                .interact()
                .map_err(prompt_err)?;
            match choice {
                0 => continue,
                1 => {
                    *secrets = candidate;
                    break;
                }
                _ => break,
            }
        }
    }
    Ok(())
}

pub fn ask_admin_password() -> Result<Option<String>, String> {
    println!("\nRemote administration lets you manage this server from any computer with `glyph-server remote ...`.");
    if !Confirm::new().with_prompt("Turn it on? (recommended)").default(true).interact().map_err(prompt_err)? {
        return Ok(None);
    }
    loop {
        let pw = Password::new()
            .with_prompt("Choose an admin password (at least 10 characters)")
            .with_confirmation("Repeat the password", "The two passwords do not match, try again")
            .interact()
            .map_err(prompt_err)?;
        match hash_new_password(&pw) {
            Ok(h) => return Ok(Some(h)),
            Err(p) => println!("Please choose another password: {p}."),
        }
    }
}

pub async fn run(config_path: PathBuf, args: SetupArgs, http: &dyn HttpClient) -> Result<(), String> {
    if config_path.exists() && !args.force {
        if args.non_interactive || !std::io::stdin().is_terminal() {
            return Err(format!("{} already exists; use --force to overwrite it", config_path.display()));
        }
        let ok = Confirm::new()
            .with_prompt(format!("{} already exists. Overwrite it?", config_path.display()))
            .default(false)
            .interact()
            .map_err(prompt_err)?;
        if !ok {
            return Err("nothing changed".into());
        }
    }

    let answers = if args.non_interactive {
        let a = answers_from_env(&args, &|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))?;
        if !args.skip_tests {
            for h in PROVIDER_HELP.iter() {
                if h.fields.iter().all(|(k, _)| a.secrets.get(k).is_some()) {
                    let o = keytest::test_provider(http, &a.secrets, h.id).await;
                    println!("{}: {} {}", h.id.display_name(), if o.ok { "OK" } else { "FAILED" }, o.message);
                }
            }
        }
        a
    } else {
        if !std::io::stdin().is_terminal() {
            return Err("this is not an interactive terminal; use `glyph-server setup --non-interactive`".into());
        }
        interactive(http, &args).await?
    };

    let cfg = build_config(&answers);
    let warnings = settings::save(&config_path, &cfg)?;
    for w in warnings {
        println!("warning: {w}");
    }
    let abs = std::fs::canonicalize(&config_path).unwrap_or(config_path.clone());
    // Windows canonical paths carry a verbatim prefix, which confuses people and shells.
    let abs = PathBuf::from(abs.display().to_string().trim_start_matches(r"\\?\").to_string());
    println!("\nSaved {} (readable only by you).", abs.display());
    println!("\nNext steps:");
    println!("  1. Start the server:        glyph-server --config {}", abs.display());
    println!("  2. Keep it running forever: glyph-server service install --config {}", abs.display());
    println!("  3. Check everything:        glyph-server doctor --config {}", abs.display());
    if cfg.admin.password_hash.is_some() {
        println!("  4. Manage it remotely:      glyph-server remote login <your server URL>");
    }
    Ok(())
}

async fn interactive(http: &dyn HttpClient, args: &SetupArgs) -> Result<Answers, String> {
    println!("Welcome to glyph-server setup. Press Enter to accept the suggestion in [brackets].\n");
    let modes = [
        "Cloudflare Tunnel (recommended: no router settings, free HTTPS)",
        "Built-in HTTPS with my own domain (Let's Encrypt, needs ports 80/443 open)",
        "Behind my own reverse proxy (Caddy, nginx, ...)",
        "Only on this computer / my home network for now",
    ];
    let pick = Select::new().with_prompt("How will the Glyph app reach this server?").items(&modes).default(0).interact().map_err(prompt_err)?;
    let mode = [Mode::Tunnel, Mode::Tls, Mode::Proxy, Mode::Local][pick];

    let (mut tls_domain, mut tls_email) = (args.tls_domain.clone(), args.tls_email.clone());
    if mode == Mode::Tls {
        tls_domain = Some(
            Input::<String>::new()
                .with_prompt("Your domain (its DNS A/AAAA record must point to this machine)")
                .validate_with(|s: &String| if s.contains('.') && !s.contains('/') && !s.contains(' ') { Ok(()) } else { Err("enter a domain like glyph.example.com") })
                .interact_text()
                .map_err(prompt_err)?,
        );
        let e = Input::<String>::new().with_prompt("E-mail for Let's Encrypt expiry notices (optional)").allow_empty(true).interact_text().map_err(prompt_err)?;
        tls_email = Some(e).filter(|e| !e.trim().is_empty());
    }

    let default_listen = match mode {
        Mode::Tls => "0.0.0.0:443",
        Mode::Local => "0.0.0.0:8787",
        _ => "127.0.0.1:8787",
    };
    let listen = Input::<String>::new()
        .with_prompt("Address and port to listen on")
        .default(args.listen.clone().unwrap_or_else(|| default_listen.to_string()))
        .validate_with(|s: &String| validate_listen(s).map(|_| ()))
        .interact_text()
        .map_err(prompt_err)?;

    let public_url = if mode == Mode::Tls {
        None
    } else {
        let hint = match mode {
            Mode::Tunnel => "Public URL of your tunnel, e.g. https://glyph.example.com (Enter to add it later)",
            Mode::Proxy => "Public URL, e.g. https://glyph.example.com (Enter to add it later)",
            _ => "Public URL (Enter to skip)",
        };
        let u = Input::<String>::new()
            .with_prompt(hint)
            .allow_empty(true)
            .validate_with(|s: &String| if s.trim().is_empty() { Ok(()) } else { validate_public_url(s).map(|_| ()) })
            .interact_text()
            .map_err(prompt_err)?;
        Some(u).filter(|u| !u.trim().is_empty()).map(|u| validate_public_url(&u)).transpose()?
    };

    let admin_hash = ask_admin_password()?;
    let mut secrets = Secrets::default();
    ask_provider_keys(http, &mut secrets, !args.skip_tests).await?;
    Ok(Answers { listen: Some(listen), public_url, mode, tls_domain, tls_email, admin_hash, secrets })
}

/// `glyph-server hash-password`: print an Argon2id hash (for Docker, Cloudflare secrets).
pub fn hash_password_cli(from_env: Option<String>) -> Result<String, String> {
    let pw = match from_env {
        Some(p) => p,
        None if std::io::stdin().is_terminal() => Password::new()
            .with_prompt("Admin password")
            .with_confirmation("Repeat the password", "The two passwords do not match")
            .interact()
            .map_err(prompt_err)?,
        None => {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).map_err(|e| e.to_string())?;
            s.trim_end_matches(['\r', '\n']).to_string()
        }
    };
    hash_new_password(&pw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn non_interactive_answers_from_env() {
        let env = env_of(&[
            ("GLYPH_ADMIN_PASSWORD", "a very long password"),
            ("YOUTUBE_API_KEY", " AIzaKEY "),
            ("GLYPH_PUBLIC_URL", "https://glyph.example.com/"),
        ]);
        let a = answers_from_env(&SetupArgs { mode: Some(Mode::Tunnel), ..Default::default() }, &env).unwrap();
        assert!(auth::verify_password("a very long password", a.admin_hash.as_deref().unwrap()));
        assert_eq!(a.secrets.youtube_api_key.as_deref(), Some("AIzaKEY"));
        assert_eq!(a.public_url.as_deref(), Some("https://glyph.example.com"));
        let cfg = build_config(&a);
        assert_eq!(cfg.bind.as_deref(), Some("127.0.0.1:8787"));
        assert_eq!(cfg.trust_proxy, Some(true));
        assert!(cfg.admin.password_hash.is_some());
        // the plain password must not end up in the rendered file
        let text = settings::render(&cfg).unwrap();
        assert!(!text.contains("a very long password"));
        assert!(text.contains("$argon2id$"));
    }

    #[test]
    fn rejects_bad_input() {
        let none = env_of(&[]);
        assert!(answers_from_env(&SetupArgs { mode: Some(Mode::Tls), ..Default::default() }, &none).is_err());
        assert!(answers_from_env(&SetupArgs { listen: Some("nonsense".into()), ..Default::default() }, &none).is_err());
        assert!(answers_from_env(&SetupArgs { public_url: Some("ftp://x".into()), ..Default::default() }, &none).is_err());
        assert!(answers_from_env(&SetupArgs::default(), &env_of(&[("GLYPH_ADMIN_PASSWORD", "short")])).is_err());
        assert!(answers_from_env(&SetupArgs::default(), &env_of(&[("GLYPH_ADMIN_PASSWORD_HASH", "plain")])).is_err());
        // no password -> admin stays off
        assert!(answers_from_env(&SetupArgs::default(), &none).unwrap().admin_hash.is_none());
    }

    #[test]
    fn tls_mode_config() {
        let a = Answers {
            listen: None,
            public_url: None,
            mode: Mode::Tls,
            tls_domain: Some("glyph.example.com".into()),
            tls_email: Some("me@example.com".into()),
            admin_hash: None,
            secrets: Secrets::default(),
        };
        let cfg = build_config(&a);
        assert_eq!(cfg.bind.as_deref(), Some("0.0.0.0:443"));
        assert_eq!(cfg.tls.domain.as_deref(), Some("glyph.example.com"));
        assert_eq!(cfg.public_url.as_deref(), Some("https://glyph.example.com"));
        assert_eq!(cfg.trust_proxy, None);
    }

    #[test]
    fn local_mode_and_validation() {
        let a = Answers {
            listen: Some("0.0.0.0:9000".into()),
            public_url: None,
            mode: Mode::Local,
            tls_domain: None,
            tls_email: None,
            admin_hash: None,
            secrets: Secrets::default(),
        };
        assert_eq!(build_config(&a).bind.as_deref(), Some("0.0.0.0:9000"));
        assert!(validate_listen("0.0.0.0:8787").is_ok());
        assert!(validate_listen("8787").is_err());
        assert!(validate_public_url("glyph.example.com").is_err());
        assert!(validate_public_url("http://192.168.1.5:8787").is_ok());
    }

    #[test]
    fn provider_names() {
        assert_eq!(parse_provider("YouTube"), Some(ProviderId::YouTube));
        assert_eq!(parse_provider("last.fm"), Some(ProviderId::LastFm));
        assert_eq!(parse_provider("deezer"), None);
        for h in PROVIDER_HELP.iter() {
            assert!(h.url.starts_with("https://"));
            for (k, _) in h.fields {
                assert!(Secrets::default().get(k).is_none());
                assert!(glyph_server_core::config::KEY_NAMES.iter().any(|(n, p)| n == k && *p == h.id));
            }
        }
    }
}
