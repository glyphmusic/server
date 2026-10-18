//! `glyph-server service install|uninstall|status`: systemd (Linux), launchd
//! (macOS), Windows service. Unit/plist generation is pure and unit tested;
//! the platform branches use runtime `cfg!` so the code is type-checked on
//! every OS.

use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const NAME: &str = "glyph-server";
pub const LAUNCHD_LABEL: &str = "com.glyphmusic.glyph-server";
#[cfg_attr(not(windows), allow(dead_code))]
pub const WINDOWS_SERVICE: &str = "GlyphServer";

#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// Install and start the service so the server runs at boot
    Install(ServiceArgs),
    /// Stop and remove the service
    Uninstall(ServiceArgs),
    /// Show whether the service is installed and running
    Status(ServiceArgs),
}

#[derive(Debug, Args, Clone, Default)]
pub struct ServiceArgs {
    /// Linux/macOS: install for the current user only (no root needed)
    #[arg(long)]
    pub user: bool,
    /// macOS: install as a system daemon (needs sudo); default is a per-user agent
    #[arg(long)]
    pub system: bool,
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub exe: PathBuf,
    pub config: PathBuf,
    /// Account the service should run as (systemd system mode).
    pub run_as: Option<String>,
    pub user_mode: bool,
    pub log_file: Option<PathBuf>,
}

pub fn systemd_unit(s: &Spec) -> String {
    let user_line = match (&s.run_as, s.user_mode) {
        (Some(u), false) => format!("User={u}\n"),
        _ => String::new(),
    };
    let wanted_by = if s.user_mode { "default.target" } else { "multi-user.target" };
    format!(
        "[Unit]\n\
         Description=Glyph metadata server\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         {user_line}\
         ExecStart=\"{exe}\" --config \"{cfg}\" serve\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         NoNewPrivileges=true\n\
         PrivateTmp=true\n\
         AmbientCapabilities=CAP_NET_BIND_SERVICE\n\n\
         [Install]\n\
         WantedBy={wanted_by}\n",
        exe = s.exe.display(),
        cfg = s.config.display(),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn launchd_plist(s: &Spec) -> String {
    let log = s.log_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "/tmp/glyph-server.log".into());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n\
         \x20   <string>{exe}</string>\n    <string>--config</string>\n    <string>{cfg}</string>\n    <string>serve</string>\n  </array>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>StandardOutPath</key><string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key><string>{log}</string>\n\
         </dict>\n</plist>\n",
        exe = xml_escape(&s.exe.display().to_string()),
        cfg = xml_escape(&s.config.display().to_string()),
        log = xml_escape(&log),
    )
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("could not run `{program}`: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(text.trim().to_string())
    }
}

fn permission_hint(e: &std::io::Error, what: &str) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        let how = if cfg!(windows) {
            "Open a terminal with \"Run as administrator\" and run the same command again."
        } else if cfg!(target_os = "macos") {
            "Run the command again with sudo, or install for your user only (no sudo needed): glyph-server service install"
        } else {
            "Run the command again with sudo, or install for your user only: glyph-server service install --user"
        };
        format!("No permission to {what}. {how}")
    } else {
        format!("Could not {what}: {e}")
    }
}

fn spec(config: &Path, a: &ServiceArgs) -> Result<Spec, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let config = std::fs::canonicalize(config).map_err(|_| {
        format!("{} does not exist yet. Run `glyph-server setup` first, or pass --config <path>.", config.display())
    })?;
    // `canonicalize` yields \\?\ paths on Windows, which services do not like.
    let strip = |p: PathBuf| PathBuf::from(p.display().to_string().trim_start_matches(r"\\?\").to_string());
    let user_mode = if cfg!(target_os = "macos") { !a.system } else { a.user };
    let run_as = std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty() && u != "root");
    let log_file = dirs::home_dir().map(|h| h.join("Library").join("Logs").join("glyph-server.log"));
    Ok(Spec { exe: strip(exe), config: strip(config), run_as, user_mode, log_file })
}

fn unit_path(user_mode: bool) -> Result<PathBuf, String> {
    if user_mode {
        let h = dirs::home_dir().ok_or("cannot find your home directory")?;
        Ok(h.join(".config").join("systemd").join("user").join(format!("{NAME}.service")))
    } else {
        Ok(PathBuf::from(format!("/etc/systemd/system/{NAME}.service")))
    }
}

fn plist_path(user_mode: bool) -> Result<PathBuf, String> {
    if user_mode {
        let h = dirs::home_dir().ok_or("cannot find your home directory")?;
        Ok(h.join("Library").join("LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist")))
    } else {
        Ok(PathBuf::from(format!("/Library/LaunchDaemons/{LAUNCHD_LABEL}.plist")))
    }
}

fn uid() -> String {
    run("id", &["-u"]).map(|s| s.trim().to_string()).unwrap_or_else(|_| "501".into())
}

pub fn handle(cmd: ServiceCmd, config: &Path) -> Result<(), String> {
    match cmd {
        ServiceCmd::Install(a) => install(config, &a),
        ServiceCmd::Uninstall(a) => uninstall(&a),
        ServiceCmd::Status(a) => status(&a),
    }
}

fn systemctl(user_mode: bool, args: &[&str]) -> Result<String, String> {
    let mut v: Vec<&str> = Vec::new();
    if user_mode {
        v.push("--user");
    }
    v.extend_from_slice(args);
    run("systemctl", &v)
}

fn install(config: &Path, a: &ServiceArgs) -> Result<(), String> {
    let s = spec(config, a)?;
    if cfg!(target_os = "linux") {
        let path = unit_path(s.user_mode)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| permission_hint(&e, "create the systemd directory"))?;
        }
        std::fs::write(&path, systemd_unit(&s)).map_err(|e| permission_hint(&e, &format!("write {}", path.display())))?;
        systemctl(s.user_mode, &["daemon-reload"])?;
        systemctl(s.user_mode, &["enable", "--now", NAME])?;
        println!("Installed {} and started it.", path.display());
        println!("Check it with: glyph-server service status{}", if s.user_mode { " --user" } else { "" });
        if s.user_mode {
            println!("To keep it running when you are logged out: sudo loginctl enable-linger $USER");
        }
        Ok(())
    } else if cfg!(target_os = "macos") {
        let path = plist_path(s.user_mode)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| permission_hint(&e, "create the LaunchAgents directory"))?;
        }
        std::fs::write(&path, launchd_plist(&s)).map_err(|e| permission_hint(&e, &format!("write {}", path.display())))?;
        let domain = if s.user_mode { format!("gui/{}", uid()) } else { "system".to_string() };
        let _ = run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
        run("launchctl", &["bootstrap", &domain, &path.display().to_string()])?;
        println!("Installed {} and started it.", path.display());
        Ok(())
    } else if cfg!(windows) {
        #[cfg(windows)]
        {
            return win::install(&s);
        }
        #[allow(unreachable_code)]
        Ok(())
    } else {
        Err("service installation is not supported on this operating system".into())
    }
}

fn uninstall(a: &ServiceArgs) -> Result<(), String> {
    let user_mode = if cfg!(target_os = "macos") { !a.system } else { a.user };
    if cfg!(target_os = "linux") {
        let path = unit_path(user_mode)?;
        let _ = systemctl(user_mode, &["disable", "--now", NAME]);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("no service found at {}. Nothing to remove.", path.display()))
            }
            Err(e) => return Err(permission_hint(&e, &format!("remove {}", path.display()))),
        }
        let _ = systemctl(user_mode, &["daemon-reload"]);
        println!("Service removed. Your config file and data were not touched.");
        Ok(())
    } else if cfg!(target_os = "macos") {
        let path = plist_path(user_mode)?;
        let domain = if user_mode { format!("gui/{}", uid()) } else { "system".to_string() };
        let _ = run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("no service found at {}. Nothing to remove.", path.display()))
            }
            Err(e) => return Err(permission_hint(&e, &format!("remove {}", path.display()))),
        }
        println!("Service removed. Your config file and data were not touched.");
        Ok(())
    } else if cfg!(windows) {
        #[cfg(windows)]
        {
            return win::uninstall();
        }
        #[allow(unreachable_code)]
        Ok(())
    } else {
        Err("service installation is not supported on this operating system".into())
    }
}

fn status(a: &ServiceArgs) -> Result<(), String> {
    let user_mode = if cfg!(target_os = "macos") { !a.system } else { a.user };
    if cfg!(target_os = "linux") {
        let path = unit_path(user_mode)?;
        if !path.exists() {
            println!("Not installed (no {}). Install it with: glyph-server service install{}", path.display(), if user_mode { " --user" } else { "" });
            return Ok(());
        }
        let active = systemctl(user_mode, &["is-active", NAME]).unwrap_or_else(|e| e).trim().to_string();
        let enabled = systemctl(user_mode, &["is-enabled", NAME]).unwrap_or_else(|e| e).trim().to_string();
        println!("Installed: yes ({})\nRunning:   {active}\nAt boot:   {enabled}", path.display());
        Ok(())
    } else if cfg!(target_os = "macos") {
        let path = plist_path(user_mode)?;
        if !path.exists() {
            println!("Not installed (no {}). Install it with: glyph-server service install", path.display());
            return Ok(());
        }
        let domain = if user_mode { format!("gui/{}", uid()) } else { "system".to_string() };
        match run("launchctl", &["print", &format!("{domain}/{LAUNCHD_LABEL}")]) {
            Ok(t) => {
                let state = t.lines().find(|l| l.trim_start().starts_with("state =")).unwrap_or("state = unknown").trim().to_string();
                println!("Installed: yes ({})\nRunning:   {state}", path.display());
            }
            Err(_) => println!("Installed: yes ({}), but not loaded. Reinstall with: glyph-server service install", path.display()),
        }
        Ok(())
    } else if cfg!(windows) {
        #[cfg(windows)]
        {
            return win::status();
        }
        #[allow(unreachable_code)]
        Ok(())
    } else {
        Err("service management is not supported on this operating system".into())
    }
}

#[cfg(windows)]
pub mod win {
    use super::*;
    use std::ffi::OsString;
    use std::time::Duration;
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
        ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    fn denied(e: &windows_service::Error) -> String {
        let t = e.to_string();
        let src = std::error::Error::source(e).map(|s| s.to_string()).unwrap_or_default();
        if src.contains("denied") || src.contains("Zugriff") || t.contains("denied") || src.contains("os error 5") {
            "No permission to manage Windows services. Open a terminal with \"Run as administrator\" and run the same command again.".into()
        } else {
            format!("{t}: {src}")
        }
    }

    pub fn install(s: &Spec) -> Result<(), String> {
        let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).map_err(|e| denied(&e))?;
        let info = ServiceInfo {
            name: OsString::from(WINDOWS_SERVICE),
            display_name: OsString::from("Glyph Server"),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: s.exe.clone(),
            launch_arguments: vec![OsString::from("--config"), s.config.clone().into_os_string(), OsString::from("service-run")],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };
        let svc = mgr
            .create_service(&info, ServiceAccess::START | ServiceAccess::CHANGE_CONFIG)
            .map_err(|e| {
                let t = denied(&e);
                if t.contains("already exists") || t.contains("1073") {
                    "The service already exists. Remove it first: glyph-server service uninstall".to_string()
                } else {
                    t
                }
            })?;
        let _ = svc.set_description("Glyph metadata server (https://github.com/glyphmusic/server)");
        svc.start::<OsString>(&[]).map_err(|e| format!("installed, but could not start: {e}"))?;
        println!("Windows service \"{WINDOWS_SERVICE}\" installed and started (starts automatically at boot).");
        Ok(())
    }

    pub fn uninstall() -> Result<(), String> {
        let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| denied(&e))?;
        let svc = mgr
            .open_service(WINDOWS_SERVICE, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)
            .map_err(|_| "The service is not installed. Nothing to remove.".to_string())?;
        if let Ok(st) = svc.query_status() {
            if st.current_state != ServiceState::Stopped {
                let _ = svc.stop();
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        svc.delete().map_err(|e| denied(&e))?;
        println!("Service removed. Your config file and data were not touched.");
        Ok(())
    }

    pub fn status() -> Result<(), String> {
        let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| denied(&e))?;
        match mgr.open_service(WINDOWS_SERVICE, ServiceAccess::QUERY_STATUS) {
            Err(_) => println!("Not installed. Install it with (as administrator): glyph-server service install"),
            Ok(svc) => {
                let st = svc.query_status().map_err(|e| e.to_string())?;
                println!("Installed: yes\nRunning:   {:?}", st.current_state);
            }
        }
        Ok(())
    }

    // ------- running as a service (`glyph-server --config X service-run`)

    static RUN: std::sync::Mutex<Option<Box<dyn FnOnce(tokio::sync::oneshot::Receiver<()>) + Send>>> = std::sync::Mutex::new(None);

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let tx = std::sync::Mutex::new(Some(tx));
        let handler = move |ctl| match ctl {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let Ok(status) = service_control_handler::register(WINDOWS_SERVICE, handler) else { return };
        let set = |state: ServiceState, accept: ServiceControlAccept| {
            let _ = status.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: accept,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::from_secs(10),
                process_id: None,
            });
        };
        set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN);
        if let Some(run) = RUN.lock().unwrap().take() {
            run(rx);
        }
        set(ServiceState::Stopped, ServiceControlAccept::empty());
    }

    /// Hand control to the Windows service dispatcher; `run` serves until the receiver fires.
    pub fn dispatch(run: Box<dyn FnOnce(tokio::sync::oneshot::Receiver<()>) + Send>) -> Result<(), String> {
        *RUN.lock().unwrap() = Some(run);
        service_dispatcher::start(WINDOWS_SERVICE, ffi_service_main)
            .map_err(|e| format!("this command is only meant to be started by Windows as a service ({e})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(user_mode: bool) -> Spec {
        Spec {
            exe: PathBuf::from("/usr/local/bin/glyph-server"),
            config: PathBuf::from("/etc/glyph/server.toml"),
            run_as: Some("alice".into()),
            user_mode,
            log_file: Some(PathBuf::from("/Users/a/Library/Logs/glyph-server.log")),
        }
    }

    #[test]
    fn systemd_system_unit() {
        let u = systemd_unit(&spec(false));
        assert!(u.contains("ExecStart=\"/usr/local/bin/glyph-server\" --config \"/etc/glyph/server.toml\" serve"));
        assert!(u.contains("User=alice"));
        assert!(u.contains("WantedBy=multi-user.target"));
        assert!(u.contains("Restart=on-failure"));
        assert!(!u.contains("password") && !u.contains("api_key"));
    }

    #[test]
    fn systemd_user_unit_has_no_user_line() {
        let u = systemd_unit(&spec(true));
        assert!(!u.contains("User="));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_is_escaped_and_keeps_alive() {
        let mut s = spec(true);
        s.config = PathBuf::from("/Users/a/A&B/server.toml");
        let p = launchd_plist(&s);
        assert!(p.contains("<string>/Users/a/A&amp;B/server.toml</string>"));
        assert!(p.contains(LAUNCHD_LABEL));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains("<string>serve</string>"));
    }

    #[test]
    fn permission_errors_get_actionable_hints() {
        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let m = permission_hint(&e, "write /etc/x");
        assert!(m.contains("No permission to write /etc/x"));
        assert!(m.contains("sudo") || m.contains("administrator"));
        let other = std::io::Error::other("boom");
        assert!(permission_hint(&other, "x").starts_with("Could not x"));
    }

    #[test]
    fn install_without_config_explains_what_to_do() {
        let err = super::spec(Path::new("definitely-not-here.toml"), &ServiceArgs::default()).unwrap_err();
        assert!(err.contains("glyph-server setup"));
    }
}
