//! `glyph-server`: the standalone metadata server plus everything needed to
//! set it up and run it: `setup` (wizard), `doctor` (checks), `service`
//! (start at boot), `remote` (manage a running server with the admin
//! password) and self-update.

mod doctor;
mod host;
mod logbuf;
mod platform;
mod ytdlp_pool;
mod remote;
mod serve;
mod service;
mod settings;
mod setup;
mod update;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

const EXAMPLES: &str = "\
Getting started:
  glyph-server setup              answer a few questions, writes server.toml
  glyph-server                    run the server
  glyph-server doctor             check config, keys, ports and reachability
  glyph-server service install    start it automatically at boot

yt-dlp companion of the Cloudflare Worker (takes effect at once):
  glyph-server companion status
  glyph-server companion off      every companion request gets 403
  glyph-server companion on

Manage a running server (here or anywhere):
  glyph-server remote login https://glyph.example.com
  glyph-server remote status
  glyph-server remote limits --rate 120
  glyph-server remote companion off
  glyph-server remote keys set youtube_api_key
  glyph-server remote logs --follow
  glyph-server remote accounts default-limit 5
  glyph-server remote accounts limit somehandle 10";

#[derive(Parser)]
#[command(name = "glyph-server", version, about = "Metadata server of the Glyph music app", after_help = EXAMPLES)]
struct Cli {
    /// Address to listen on (default 0.0.0.0:8787)
    #[arg(long, global = true)]
    bind: Option<String>,
    /// Path to server.toml (default: ./server.toml)
    #[arg(long, env = "GLYPH_CONFIG", global = true)]
    config: Option<PathBuf>,
    /// Same as `check-update` (kept for scripts)
    #[arg(long, hide = true)]
    check_update: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (the default when no command is given)
    Serve,
    /// Set the server up step by step (or --non-interactive for scripts/Docker)
    Setup(setup::SetupArgs),
    /// Check config, API keys, ports and whether the server is reachable
    Doctor,
    /// Run the server in the background at boot (systemd, launchd, Windows service)
    Service {
        #[command(subcommand)]
        cmd: service::ServiceCmd,
    },
    /// Manage a running server with its admin password
    Remote(remote::RemoteArgs),
    /// Print an Argon2id hash of an admin password (for Docker / Cloudflare secrets)
    HashPassword,
    /// Require a shared server bearer (prompted without echo); --disable makes it public.
    Access { #[arg(long)] disable: bool },
    /// Turn the Worker's yt-dlp companion on or off (off = 403), or show its state
    Companion {
        #[command(subcommand)]
        cmd: CompanionCmd,
    },
    /// Check GitHub for a newer release
    CheckUpdate,
    /// Download, verify (SHA-256) and install the latest release
    SelfUpdate,
    /// Entry point used by the Windows service manager
    #[command(hide = true)]
    ServiceRun,
}

#[derive(Subcommand, Clone, Copy)]
enum CompanionCmd {
    /// Answer the Worker again
    On,
    /// Answer every companion request with 403 Forbidden (YouTube pauses)
    Off,
    /// Show whether the companion is on and set up
    Status,
}

/// `glyph-server companion ...`: a marker file the running server checks on
/// every request, so no restart is needed.
fn companion_cmd(cmd: CompanionCmd, s: &settings::Settings) {
    let flag = serve::companion_off_flag(s);
    match cmd {
        CompanionCmd::Off => {
            if let Some(dir) = flag.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).unwrap_or_else(|e| fail(format!("cannot create {}: {e}", dir.display())));
            }
            std::fs::write(&flag, b"off
").unwrap_or_else(|e| fail(format!("cannot write {}: {e}", flag.display())));
            println!("companion: off (requests get 403 Forbidden, YouTube pauses)");
        }
        CompanionCmd::On => {
            match std::fs::remove_file(&flag) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => fail(format!("cannot remove {}: {e}", flag.display())),
            }
            println!("companion: on");
        }
        CompanionCmd::Status => {
            let token = std::env::var("YTDLP_SERVICE_TOKEN").ok().is_some_and(|t| t.len() >= 32);
            println!("companion: {}", if flag.exists() { "off (403)" } else { "on" });
            println!("token:     {}", if token { "set" } else { "missing (YTDLP_SERVICE_TOKEN, companion routes answer 404)" });
            println!("yt-dlp:    {}", s.ytdlp_path.as_deref().unwrap_or("from PATH"));
            println!("switch:    {}", flag.display());
        }
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap_or_else(|e| fail(e))
}

fn init_logs(logs: &Arc<logbuf::LogBuffer>, quiet: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| if quiet { "warn".into() } else { "info".into() });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(logbuf::TeeMaker(logs.clone()))
        .with_ansi(false)
        .try_init();
}

fn load_settings(cli: &Cli) -> settings::Settings {
    let path = cli.config.clone().unwrap_or_else(settings::default_config_path);
    let file = settings::load_file(&path, cli.config.is_some()).unwrap_or_else(|e| fail(format!("cannot read config: {e}")));
    settings::Settings::resolve(file, cli.bind.clone(), path)
}

fn main() {
    let cli = Cli::parse();
    let logs = logbuf::LogBuffer::new();
    let serving = matches!(cli.command, None | Some(Cmd::Serve) | Some(Cmd::ServiceRun)) && !cli.check_update;
    init_logs(&logs, !serving);
    let config_path = cli.config.clone().unwrap_or_else(settings::default_config_path);

    if cli.check_update {
        let s = load_settings(&cli);
        if let Err(e) = runtime().block_on(update::check(&s.update_repo)) {
            fail(format!("update check failed: {e}"));
        }
        return;
    }

    match cli.command {
        Some(Cmd::Access { disable }) => {
            let mut file = settings::load_file(&config_path, false).unwrap_or_else(|e| fail(e));
            file.server_bearer_hash = if disable { None } else {
                let token = dialoguer::Password::new().with_prompt("Server bearer (32-256 characters)").with_confirmation("Confirm bearer", "Bearers differ").interact().unwrap_or_else(|e| fail(e));
                if !glyph_server_core::access::valid_token(&token) { fail("use 32-256 ASCII bearer characters"); }
                Some(glyph_server_core::access::token_hash(&token))
            };
            for warning in settings::save(&config_path, &file).unwrap_or_else(|e| fail(e)) { eprintln!("{warning}"); }
            println!("Server access updated. Restart the server to apply (environment overrides still win).");
        }
        None | Some(Cmd::Serve) => {
            let s = load_settings(&cli);
            if !s.config_path.exists() {
                eprintln!(
                    "note: no {} found, running with defaults (key-free sources only).\n      Run `glyph-server setup` to add API keys, HTTPS and remote management.",
                    s.config_path.display()
                );
            }
            let r = runtime().block_on(serve::run(s, logs, async {
                let _ = tokio::signal::ctrl_c().await;
            }));
            if let Err(e) = r {
                fail(e);
            }
        }
        Some(Cmd::ServiceRun) => {
            let s = load_settings(&cli);
            #[cfg(windows)]
            {
                let run = Box::new(move |stop: tokio::sync::oneshot::Receiver<()>| {
                    let r = runtime().block_on(serve::run(s, logs, async {
                        let _ = stop.await;
                    }));
                    if let Err(e) = r {
                        tracing::error!("{e}");
                    }
                });
                if let Err(e) = service::win::dispatch(run) {
                    fail(e);
                }
            }
            #[cfg(not(windows))]
            {
                // systemd / launchd simply run `serve`.
                let r = runtime().block_on(serve::run(s, logs, async {
                    let _ = tokio::signal::ctrl_c().await;
                }));
                if let Err(e) = r {
                    fail(e);
                }
            }
        }
        Some(Cmd::Setup(args)) => {
            let http = platform::ReqwestClient::new();
            if let Err(e) = runtime().block_on(setup::run(config_path, args, &http)) {
                fail(e);
            }
        }
        Some(Cmd::Doctor) => {
            let s = load_settings(&cli);
            let ok = runtime().block_on(doctor::run(&s));
            std::process::exit(if ok { 0 } else { 1 });
        }
        Some(Cmd::Service { cmd }) => {
            if let Err(e) = service::handle(cmd, &config_path) {
                fail(e);
            }
        }
        Some(Cmd::Remote(args)) => {
            if let Err(e) = runtime().block_on(remote::run(args)) {
                fail(e);
            }
        }
        Some(Cmd::HashPassword) => match setup::hash_password_cli(std::env::var("GLYPH_ADMIN_PASSWORD").ok()) {
            Ok(h) => println!("{h}"),
            Err(e) => fail(e),
        },
        Some(Cmd::Companion { ref cmd }) => companion_cmd(*cmd, &load_settings(&cli)),
        Some(Cmd::CheckUpdate) => {
            let s = load_settings(&cli);
            if let Err(e) = runtime().block_on(update::check(&s.update_repo)) {
                fail(format!("update check failed: {e}"));
            }
        }
        Some(Cmd::SelfUpdate) => {
            let s = load_settings(&cli);
            if let Err(e) = runtime().block_on(update::self_update(&s.update_repo)) {
                fail(format!("self-update failed: {e}"));
            }
        }
    }
}
