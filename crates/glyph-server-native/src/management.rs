use crate::{diagnostics, remote, settings::Settings, update};
use clap::ValueEnum;
use glyph_server_core::admin::types::{
    CompanionStatus, MaintenanceResponse, StatusResponse, UpdateApplyResponse,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Component {
    Cli,
    Server,
    #[value(alias = "companoin")]
    Companion,
    #[value(name = "r-dlp", alias = "rdlp")]
    Rdlp,
    All,
}
async fn health(base: &str) -> Result<glyph_protocol::HealthResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!("server health HTTP {}", response.status().as_u16()));
    }
    response
        .json()
        .await
        .map_err(|_| "server returned invalid health data".into())
}
pub async fn inspect(
    s: &Settings,
    component: Component,
    explicit: Option<String>,
    local: bool,
    check: bool,
) -> Result<(), String> {
    if matches!(component, Component::Cli | Component::All) {
        println!(
            "Glyph CLI {} ({})",
            env!("CARGO_PKG_VERSION"),
            update::TARGET
        );
        if check {
            println!("{}", update::check_info(&s.update_repo).await?.message);
        }
        if component == Component::Cli {
            return Ok(());
        }
    }
    let base = if local {
        None
    } else {
        remote::selected_server(explicit)?
    };
    if matches!(component, Component::Server | Component::All) {
        if let Some(base) = &base {
            let status = health(base).await?;
            println!(
                "Server {} at {base} (protocol {}, accounts required: {})",
                status.server_version, status.protocol_version, status.account_required
            );
            if check {
                println!(
                    "{}",
                    update::check_component(&s.update_repo, &status.server_version)
                        .await?
                        .message
                );
            }
        } else if component == Component::Server {
            return Err(
                "no server selected; run `glyph remote login <url>` or pass --server <url>".into(),
            );
        }
        if component == Component::Server {
            return Ok(());
        }
    }
    let status = if let Some(base) = base {
        let client = remote::Client::connect(Some(base)).await?;
        let status: CompanionStatus = client.get("companion/status").await?;
        if check {
            let result: MaintenanceResponse = client
                .post("companion/update/check", &serde_json::json!({}))
                .await?;
            println!("Companion update: {}", result.message);
            if status.r_dlp.available {
                let result: MaintenanceResponse = client
                    .post("companion/r-dlp/check", &serde_json::json!({}))
                    .await?;
                println!("r-dlp update: {}", result.message);
            }
        }
        status
    } else {
        println!("Local companion/tools (use --server <url> for a remote companion)");
        let status = diagnostics::snapshot(s).await;
        if check && status.r_dlp.available {
            println!(
                "{}",
                diagnostics::maintenance(&s.update_repo, true, false)
                    .await?
                    .message
            );
        }
        status
    };
    if component == Component::Rdlp {
        println!(
            "r-dlp: {}",
            status.r_dlp.version.as_deref().unwrap_or("not installed")
        );
        if !status.r_dlp.available {
            return Err("r-dlp unavailable; install it or set GLYPH_RDLP_PATH".into());
        }
    } else {
        diagnostics::print(&status);
        if check && !status.ready {
            return Err("companion not ready: check its token, enabled state, yt-dlp, r-dlp and FFmpeg above".into());
        }
    }
    Ok(())
}
pub async fn upgrade(
    s: &Settings,
    component: Component,
    server: Option<String>,
    local: bool,
    check: bool,
) -> Result<(), String> {
    match component {
        Component::Cli => if check {update::check(&s.update_repo).await.map(|_|())} else {update::self_update(&s.update_repo).await},
        Component::Server => {
            let client=remote::Client::connect(server).await?;
            let status:StatusResponse=client.get("status").await?;
            if status.platform=="worker" {
                if check {println!("{}",update::check_component(&s.update_repo,&status.server_version).await?.message);return Ok(());}
                return Err("Cloudflare Worker: use `glyph update server --worker-config /path/to/wrangler.toml` (requires Cloudflare login)".into());
            }
            if check {
                let result:glyph_server_core::admin::types::UpdateInfo=client.post("update/check",&serde_json::json!({})).await?;
                println!("{}",result.message);
            } else {
                let result:UpdateApplyResponse=client.post("update/apply",&serde_json::json!({})).await?;
                println!("{}",result.message);
                if result.restart_required {println!("Restart required. Verify the running version with `glyph version server` afterwards.");}
            }
            Ok(())
        },
        Component::Companion|Component::Rdlp => {
            let selected=if local{None}else{remote::selected_server(server)?};
            let rdlp=component==Component::Rdlp;
            let result=if let Some(base)=selected {
                remote::Client::connect(Some(base)).await?.post::<_,MaintenanceResponse>(
                    &format!("companion/{}/{}",if rdlp{"r-dlp"}else{"update"},if check{"check"}else{"apply"}),&serde_json::json!({})).await?
            } else {diagnostics::maintenance(&s.update_repo,rdlp,!check).await?};
            println!("{}",result.message);
            if result.restart_required {println!("Restart the companion, then verify with `glyph check companion`.");}
            Ok(())
        },
        Component::All => Err("Choose one update target: cli, server, companion or r-dlp. Use `glyph check all` to inspect all components.".into()),
    }
}
