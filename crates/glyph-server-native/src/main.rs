//! `glyph-server`: the standalone metadata server plus everything needed to
//! set it up and run it: `setup` (wizard), `doctor` (checks), `service`
//! (start at boot), `remote` (manage a running server with the admin
//! password) and self-update.

mod config_cmd;
mod doctor;
mod diagnostics;
mod exit;
mod management;
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

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

/// Full CLI reference; every `--help` ends with it so people and AI agents
/// find the details (flags, examples, common tasks).
macro_rules! docs_url { () => { "https://docs.glyph.my/self-hosting/cli" } }
const DOCS: &str = concat!("Docs: ", docs_url!(), "\nMarkdown (for AI agents): ", docs_url!(), ".md");

const EXAMPLES: &str = concat!("\
Getting started:
  glyph status                    CLI, selected server, users and limits at a glance
  glyph doctor                    find problems on this machine (config, keys, ports)
  glyph version all               CLI/server/companion versions
  glyph check companion           probe companion, r-dlp, yt-dlp and FFmpeg
  glyph update                    update the CLI

Own server (server.toml on this machine):
  glyph setup                     answer a few questions, writes server.toml
  glyph                           run the server
  glyph config list               every option, its value and where it comes from
  glyph config set rate_limit_per_min 120
  glyph service install           start it automatically at boot

Cloudflare (npm package):
  glyph setup cloudflare          own Cloudflare deployment
  glyph deploy                    deploy again
  glyph cloudflare list|set|unset|secret|domain

A running server, here or anywhere (log in once with the admin password):
  glyph remote login https://glyph.example.com
  glyph config set rate_limit_per_min 120 --server https://glyph.example.com
  glyph companion off --server https://glyph.example.com   server stops using YouTube
  glyph remote stats
  glyph remote ban account somehandle      account, its addresses and hardware
  glyph remote ban ip 203.0.113.0/24
  glyph remote unban somehandle
  glyph remote accounts revoke somehandle  sign out its devices
  glyph remote logs --follow

The companion machine itself:
  glyph companion off             answer every Worker request with 403

For scripts and AI agents: --json (machine-readable), --dry-run (change
nothing), --yes (no questions); exit codes are listed in the docs.

Docs: ", docs_url!(), "
Markdown (for AI agents): ", docs_url!(), ".md
All docs for AI agents: https://docs.glyph.my/llms.txt");

/// Puts [`DOCS`] under the help of every subcommand that has no text of its own.
fn with_docs(mut cmd: clap::Command) -> clap::Command {
    if cmd.get_after_help().is_none() {
        cmd = cmd.after_help(DOCS);
    }
    let names: Vec<String> = cmd.get_subcommands().map(|c| c.get_name().to_string()).collect();
    for name in names {
        cmd = cmd.mut_subcommand(name, with_docs);
    }
    cmd
}

fn parse_cli() -> Cli {
    let matches = with_docs(Cli::command()).get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

#[derive(Parser)]
#[command(name = "glyph", bin_name = "glyph", version, about = "Metadata server of the Glyph music app", after_help = EXAMPLES)]
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
    /// Print machine-readable JSON (errors too, with their exit code)
    #[arg(long, global = true)]
    json: bool,
    /// Do not ask before changes that affect every user of a server
    #[arg(long, short = 'y', global = true)]
    yes: bool,
    /// Show what a change would do, change nothing
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// The CLI, the selected server, its users and limits at a glance
    Status {
        /// Server to show (default: the one from `glyph remote use` / the last login)
        #[arg(long)] server: Option<String>,
    },
    /// Show installed or running component versions (default: CLI)
    Version {
        #[arg(value_enum,default_value="cli")] target:management::Component,
        #[arg(long)] server:Option<String>,
        #[arg(long)] local:bool,
    },
    /// Check server/companion connectivity, tools and release versions
    Check {
        #[arg(value_enum,default_value="all")] target:management::Component,
        #[arg(long)] server:Option<String>,
        #[arg(long)] local:bool,
    },
    /// Update CLI, native server, companion or r-dlp; --check only checks
    Update {
        #[arg(value_enum,default_value="cli")] target:management::Component,
        #[arg(long)] check:bool,
        #[arg(long)] server:Option<String>,
        #[arg(long)] local:bool,
    },
    /// Run the server (the default when no command is given)
    Serve,
    /// Set the server up step by step (or --non-interactive for scripts/Docker)
    Setup(setup::SetupArgs),
    /// Check config, API keys, ports and whether the server is reachable
    Doctor,
    /// Show or change options: server.toml here, or a running server's live settings with --server
    Config {
        #[command(subcommand)]
        cmd: config_cmd::ConfigCmd,
        /// A running server (logged in with `glyph remote login`): its limits and keys, applied at once
        #[arg(long, global = true)]
        server: Option<String>,
    },
    /// Run the server in the background at boot (systemd, launchd, Windows service)
    Service {
        #[command(subcommand)]
        cmd: service::ServiceCmd,
    },
    /// Manage a running server with its admin password
    Remote(remote::RemoteArgs),
    /// Print an Argon2id hash of an admin password (for Docker / Cloudflare secrets)
    #[command(hide = true)]
    HashPassword,
    /// Require a shared server bearer (prompted without echo); --disable makes it public. Applies within seconds.
    Access { #[arg(long)] disable: bool },
    /// YouTube companion: on this machine (default) or, with --server, whether a server uses it
    Companion {
        #[command(subcommand)]
        cmd: CompanionCmd,
        /// A running server (logged in): switch its use of the companion for every user
        #[arg(long, global = true)]
        server: Option<String>,
    },
    /// Shell completions: glyph completions bash|zsh|fish|powershell|elvish
    Completions { shell: clap_complete::Shell },
    /// Manual page (roff) on standard output: glyph man > glyph.1
    Man,
    /// [npm package] Deploy the Cloudflare Worker again
    Deploy {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// [npm package] Cloudflare options: list, set, unset, secret, domain
    Cloudflare {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },
    /// Check GitHub for a newer release (same as `glyph update server --check`)
    #[command(hide = true)]
    CheckUpdate,
    /// Download, verify (SHA-256) and install the latest release
    #[command(hide = true)]
    SelfUpdate,
    /// Entry point used by the Windows service manager
    #[command(hide = true)]
    ServiceRun,
}

#[derive(Subcommand, Clone, Copy)]
enum CompanionCmd {
    /// Answer the Worker again (with --server: the server uses its companion again)
    On,
    /// Answer every companion request with 403 (with --server: the server stops using it; YouTube pauses for everyone)
    Off,
    /// Show whether the companion is on and set up
    Status,
}

impl From<CompanionCmd> for remote::SwitchCmd {
    fn from(c: CompanionCmd) -> Self {
        match c {
            CompanionCmd::On => Self::On,
            CompanionCmd::Off => Self::Off,
            CompanionCmd::Status => Self::Status,
        }
    }
}

/// `glyph companion ...`: a marker file the running server checks on
/// every request, so no restart is needed.
fn companion_cmd(cmd: CompanionCmd, s: &settings::Settings, o: remote::Opts) {
    let flag = serve::companion_off_flag(s);
    if o.dry_run && !matches!(cmd, CompanionCmd::Status) {
        println!("dry run: would switch the companion on this machine {}. Nothing changed.", if matches!(cmd, CompanionCmd::On) { "on" } else { "off" });
        return;
    }
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
        CompanionCmd::Status if o.json => {
            let token = std::env::var("YTDLP_SERVICE_TOKEN").ok().is_some_and(|t| t.len() >= 32);
            println!("{}", serde_json::json!({ "enabled": !flag.exists(), "token": token, "ytdlp_path": s.ytdlp_path, "switch": flag }));
        }
        CompanionCmd::Status => {
            let token = std::env::var("YTDLP_SERVICE_TOKEN").ok().is_some_and(|t| t.len() >= 32);
            println!("companion: {}", if flag.exists() { "off (403)" } else { "on" });
            println!("token:     {}", if token { "set" } else { "missing (YTDLP_SERVICE_TOKEN, companion routes answer 404)" });
            println!("yt-dlp:    {}", s.ytdlp_path.as_deref().unwrap_or("from PATH"));
            println!("switch:    {}", flag.display());
            println!("\nThis is the companion machine. Whether a server uses it: glyph companion status --server <url>");
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
    let cli = parse_cli();
    let opts = remote::Opts { json: cli.json, yes: cli.yes, dry_run: cli.dry_run };
    let done = |r: Result<(), exit::CliError>| if let Err(e) = r { exit::fail(e, opts.json) };
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
        Some(Cmd::Version {target,ref server,local}) | Some(Cmd::Check {target,ref server,local}) => {
            let check=matches!(cli.command,Some(Cmd::Check{..}));
            if let Err(error)=runtime().block_on(management::inspect(&load_settings(&cli),target,server.clone(),local,check)){fail(error);}
        },
        Some(Cmd::Update {target,check,ref server,local}) => {
            if let Err(error)=runtime().block_on(management::upgrade(&load_settings(&cli),target,server.clone(),local,check)){fail(error);}
        },
        Some(Cmd::Access { disable }) => {
            let mut file = settings::load_file(&config_path, false).unwrap_or_else(|e| fail(e));
            file.server_bearer_hash = if disable { None } else {
                let token = dialoguer::Password::new().with_prompt("Server bearer (32-256 characters)").with_confirmation("Confirm bearer", "Bearers differ").interact().unwrap_or_else(|e| fail(e));
                if !glyph_server_core::access::valid_token(&token) { fail("use 32-256 ASCII bearer characters"); }
                Some(glyph_server_core::access::token_hash(&token))
            };
            for warning in settings::save(&config_path, &file).unwrap_or_else(|e| fail(e)) { eprintln!("{warning}"); }
            println!("Server access updated. A running server applies it within a few seconds (GLYPH_SERVER_BEARER in its environment still wins).");
        }
        None | Some(Cmd::Serve) => {
            let s = load_settings(&cli);
            if !s.config_path.exists() {
                eprintln!(
                    "note: no {} found, running with defaults (key-free sources only).\n      Run `glyph setup` to add API keys, HTTPS and remote management.",
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
        Some(Cmd::Config { cmd, server: Some(server) }) => done(runtime().block_on(remote::config(Some(server), cmd, opts))),
        Some(Cmd::Config { cmd, server: None }) => {
            if let Err(e) = config_cmd::run(cmd, &config_path) {
                fail(e);
            }
        }
        Some(Cmd::Status { server }) => done(runtime().block_on(remote::overview(server, opts))),
        Some(Cmd::Completions { shell }) => {
            let mut cmd = with_docs(Cli::command());
            clap_complete::generate(shell, &mut cmd, "glyph", &mut std::io::stdout());
        }
        Some(Cmd::Man) => {
            if let Err(e) = clap_mangen::Man::new(with_docs(Cli::command())).render(&mut std::io::stdout()) {
                fail(e);
            }
        }
        Some(Cmd::Deploy { .. }) | Some(Cmd::Cloudflare { .. }) => exit::fail(
            exit::CliError::new(exit::USAGE, "this command comes with the npm package of the CLI")
                .hint("npm install --global glyph-server, then run it again"),
            opts.json,
        ),
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
        Some(Cmd::Remote(args)) => done(runtime().block_on(remote::run(args, opts))),
        Some(Cmd::HashPassword) => match setup::hash_password_cli(std::env::var("GLYPH_ADMIN_PASSWORD").ok()) {
            Ok(h) => println!("{h}"),
            Err(e) => fail(e),
        },
        Some(Cmd::Companion { cmd, server: Some(ref server) }) => done(runtime().block_on(remote::companion(Some(server.clone()), Some(cmd.into()), opts))),
        Some(Cmd::Companion { cmd, server: None }) => companion_cmd(cmd, &load_settings(&cli), opts),
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

#[cfg(test)]
mod cli_tests {
    use super::*;
    #[test]
    fn component_commands_and_legacy_aliases_parse_without_running_them() {
        assert!(matches!(Cli::try_parse_from(["glyph","version"]).unwrap().command,Some(Cmd::Version{target:management::Component::Cli,..})));
        assert!(matches!(Cli::try_parse_from(["glyph","check","companoin"]).unwrap().command,Some(Cmd::Check{target:management::Component::Companion,..})));
        assert!(matches!(Cli::try_parse_from(["glyph","check","r-dlp","--local"]).unwrap().command,Some(Cmd::Check{target:management::Component::Rdlp,local:true,..})));
        assert!(matches!(Cli::try_parse_from(["glyph","update","companion","--check","--server","https://own.example"]).unwrap().command,Some(Cmd::Update{check:true,..})));
        assert!(matches!(Cli::try_parse_from(["glyph-server","self-update"]).unwrap().command,Some(Cmd::SelfUpdate)));
        assert!(matches!(Cli::try_parse_from(["glyph","--config","x.toml","config","set","bind","0.0.0.0:1"]).unwrap().command,Some(Cmd::Config{cmd:config_cmd::ConfigCmd::Set{..},..})));
        let help=Cli::try_parse_from(["glyph","help"]).err().unwrap();
        assert_eq!(help.kind(),clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("Usage: glyph"));
    }

    #[test]
    fn global_flags_and_new_commands_parse() {
        let cli = Cli::try_parse_from(["glyph", "remote", "stats", "--json", "--yes"]).unwrap();
        assert!(cli.json && cli.yes && !cli.dry_run);
        assert!(matches!(Cli::try_parse_from(["glyph", "--dry-run", "companion", "off", "--server", "https://x.example"]).unwrap().command, Some(Cmd::Companion { server: Some(_), .. })));
        assert!(matches!(Cli::try_parse_from(["glyph", "config", "set", "rate_limit_per_min", "30", "--server", "x.example"]).unwrap().command, Some(Cmd::Config { server: Some(_), .. })));
        assert!(matches!(Cli::try_parse_from(["glyph", "completions", "powershell"]).unwrap().command, Some(Cmd::Completions { .. })));
        assert!(matches!(Cli::try_parse_from(["glyph", "cloudflare", "set", "X", "1"]).unwrap().command, Some(Cmd::Cloudflare { .. })));
        assert!(matches!(Cli::try_parse_from(["glyph", "status"]).unwrap().command, Some(Cmd::Status { server: None })));
        let help = with_docs(Cli::command()).render_help().to_string();
        assert!(!help.contains("self-update") && !help.contains("hash-password"), "legacy commands stay hidden");
        assert!(help.contains("[npm package]"));
    }

    #[test]
    fn every_help_links_the_cli_docs() {
        let help = |args: &[&str]| with_docs(Cli::command()).try_get_matches_from(args).err().unwrap().to_string();
        for args in [&["glyph", "help"][..], &["glyph", "--help"], &["glyph", "remote", "--help"], &["glyph", "remote", "limits", "--help"], &["glyph", "help", "companion"], &["glyph", "config", "set", "-h"]] {
            let text = help(args);
            assert!(text.contains("https://docs.glyph.my/self-hosting/cli.md"), "{args:?}: {text}");
        }
    }
}
