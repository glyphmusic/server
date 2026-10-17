//! `glyph-server doctor`: checks everything a working server needs and says
//! in plain words what to do about each problem.

use crate::platform::ReqwestClient;
use crate::settings::{self, Settings};
use glyph_server_core::admin::{auth, keytest};
use glyph_server_core::Config;

struct Report {
    failed: bool,
}

impl Report {
    fn ok(&self, msg: impl AsRef<str>) {
        println!("  [ ok ] {}", msg.as_ref());
    }
    fn warn(&self, msg: impl AsRef<str>, fix: impl AsRef<str>) {
        println!("  [warn] {}\n         -> {}", msg.as_ref(), fix.as_ref());
    }
    fn fail(&mut self, msg: impl AsRef<str>, fix: impl AsRef<str>) {
        self.failed = true;
        println!("  [FAIL] {}\n         -> {}", msg.as_ref(), fix.as_ref());
    }
}

async fn health(url: &str) -> Result<glyph_protocol::HealthResponse, String> {
    let r = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("{}/v1/health", url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status().as_u16()));
    }
    r.json().await.map_err(|e| e.to_string())
}

/// Returns true when nothing failed.
pub async fn run(s: &Settings) -> bool {
    let mut r = Report { failed: false };
    println!("glyph-server {} doctor\n", env!("CARGO_PKG_VERSION"));

    println!("Config");
    if s.config_path.exists() {
        r.ok(format!("{} found", s.config_path.display()));
        if settings::is_world_readable(&s.config_path) {
            r.warn("the config file is readable by other users (it may contain keys)", "run `glyph-server setup --force` or restrict the file permissions");
        }
    } else {
        r.warn(format!("no {} (running with defaults)", s.config_path.display()), "run `glyph-server setup`");
    }

    println!("\nSources");
    let config = Config::new(s.secrets.clone());
    let enabled = config.enabled_providers();
    r.ok(format!("{} on: {}", enabled.len(), enabled.iter().map(|p| p.display_name()).collect::<Vec<_>>().join(", ")));
    let http = ReqwestClient::new();
    for h in crate::setup::PROVIDER_HELP.iter() {
        if h.fields.iter().all(|(k, _)| s.secrets.get(k).is_some()) {
            let o = keytest::test_provider(&http, &s.secrets, h.id).await;
            if o.ok {
                r.ok(format!("{} key works", h.id.display_name()));
            } else {
                r.fail(format!("{} key: {}", h.id.display_name(), o.message), format!("fix it with `glyph-server remote keys set` or edit {}", s.config_path.display()));
            }
        }
    }

    println!("\nAdmin API (remote management)");
    match &s.admin_password_hash {
        None => r.warn("off (no admin password)", "run `glyph-server setup` to set one, then use `glyph-server remote`"),
        Some(h) if auth::is_valid_hash(h) => r.ok("on"),
        Some(_) => r.fail("admin password hash is invalid, the admin API stays off", "run `glyph-server hash-password` and paste the result into server.toml"),
    }

    println!("\nNetwork");
    let port = s.bind.rsplit(':').next().unwrap_or("8787");
    match std::net::TcpListener::bind(&s.bind) {
        Ok(_) => r.warn(format!("nothing is listening on {} yet", s.bind), "start it with `glyph-server` or `glyph-server service install`"),
        Err(_) => match health(&format!("http://127.0.0.1:{port}")).await {
            Ok(h) => r.ok(format!("server {} is running on {}", h.server_version, s.bind)),
            Err(_) if s.tls.is_some() => r.ok(format!("{} is in use (HTTPS server, checked below)", s.bind)),
            Err(_) => r.fail(format!("{} is used by another program", s.bind), "stop that program or pick another port with --bind"),
        },
    }
    match &s.public_url {
        None => r.warn("no public URL set, so I cannot check reachability from outside", "set public_url in server.toml (e.g. https://glyph.example.com)"),
        Some(u) => match health(u).await {
            Ok(h) if h.protocol_version == glyph_protocol::PROTOCOL_VERSION => r.ok(format!("{u} answers (protocol v{})", h.protocol_version)),
            Ok(h) => r.fail(
                format!("{u} speaks protocol v{} but this build speaks v{}", h.protocol_version, glyph_protocol::PROTOCOL_VERSION),
                "update the server: `glyph-server self-update`",
            ),
            Err(e) => r.fail(format!("{u} is not reachable: {e}"), "check DNS, the tunnel / reverse proxy, or port forwarding"),
        },
    }

    println!("\nUpdates");
    match crate::update::check_info(&s.update_repo).await {
        Ok(i) if i.update_available => r.warn(i.message, "run `glyph-server self-update`"),
        Ok(i) => r.ok(i.message),
        Err(e) => r.warn(format!("could not check for updates: {e}"), "check the internet connection"),
    }

    println!("\n{}", if r.failed { "Some checks failed. Fix the [FAIL] lines above." } else { "All good." });
    !r.failed
}
