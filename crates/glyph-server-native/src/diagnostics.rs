//! Fixed, bounded component diagnostics; no shell or caller-supplied commands.
use crate::{settings::Settings, update};
use glyph_server_core::admin::types::{CompanionStatus, MaintenanceResponse, ToolStatus};
use tokio::io::AsyncReadExt;

pub async fn tool_version(program: &str, ffmpeg: bool) -> Option<String> {
    let work = async {
        let mut child = tokio::process::Command::new(program)
            .arg(if ffmpeg { "-version" } else { "--version" })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()?
            .take(65_537)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() > 65_536 || !child.wait().await.ok()?.success() {
            return None;
        }
        let text = String::from_utf8(bytes).ok()?;
        text.lines()
            .next()
            .map(|s| s.chars().filter(|c| !c.is_control()).take(160).collect())
            .filter(|s: &String| !s.is_empty())
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), work)
        .await
        .ok()
        .flatten()
}
pub async fn snapshot(s: &Settings) -> CompanionStatus {
    snapshot_with(
        s.ytdlp_path.as_deref(),
        !crate::serve::companion_off_flag(s).exists(),
    )
    .await
}
pub async fn snapshot_with(ytdlp: Option<&str>, enabled: bool) -> CompanionStatus {
    let rdlp = std::env::var("GLYPH_RDLP_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let ytdlp = ytdlp.unwrap_or("yt-dlp");
    let ffmpeg = std::env::var("GLYPH_FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into());
    let (r, y, f) = tokio::join!(
        tool_version(rdlp.as_deref().unwrap_or("r-dlp"), false),
        tool_version(ytdlp, false),
        tool_version(&ffmpeg, true)
    );
    let token = std::env::var("YTDLP_SERVICE_TOKEN").is_ok_and(|s| s.len() >= 32);
    CompanionStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        target: update::TARGET.into(),
        enabled,
        ready: token && enabled && y.is_some() && f.is_some() && (rdlp.is_none() || r.is_some()),
        r_dlp: ToolStatus {
            available: r.is_some(),
            version: r,
            configured: rdlp.is_some(),
        },
        yt_dlp: ToolStatus {
            available: y.is_some(),
            version: y,
            configured: true,
        },
        ffmpeg: ToolStatus {
            available: f.is_some(),
            version: f,
            configured: true,
        },
    }
}
pub async fn maintenance(
    repo: &str,
    rdlp: bool,
    apply: bool,
) -> Result<MaintenanceResponse, String> {
    if rdlp {
        let program = std::env::var("GLYPH_RDLP_PATH")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "r-dlp".into());
        let version = tool_version(&program, false)
            .await
            .ok_or("r-dlp is not installed; set GLYPH_RDLP_PATH to its executable")?;
        let current = version
            .split_whitespace()
            .last()
            .ok_or("r-dlp version missing")?;
        let info = update::check_component("glyphmusic/r-dlp", current).await?;
        let applied = apply && info.update_available;
        if applied {
            if std::path::Path::new("/.dockerenv").exists() {
                return Err("Container-managed r-dlp: run `glyph update r-dlp --compose-dir /path/to/compose-project` on its Docker host".into());
            }
            crate::ytdlp_pool::reload().await;
            update::install_rdlp(
                &program,
                info.latest.as_deref().ok_or("release version missing")?,
            )
            .await?;
            // Drain only existing workers, preserving the pool's current scheduling
            // and warmup implementation owned by the r-dlp work.
            crate::ytdlp_pool::reload().await;
        }
        let message = if applied {
            "r-dlp updated and extractor workers reloaded; subsequent requests use the new binary"
                .into()
        } else {
            info.message.clone()
        };
        Ok(MaintenanceResponse {
            info,
            applied,
            restart_required: false,
            message,
        })
    } else {
        let info = update::check_info(repo).await?;
        let result = if apply && info.update_available {
            Some(update::apply(repo).await?)
        } else {
            None
        };
        Ok(MaintenanceResponse {
            applied: result.is_some(),
            restart_required: result.as_ref().is_some_and(|r| r.restart_required),
            message: result
                .map(|r| r.message)
                .unwrap_or_else(|| info.message.clone()),
            info,
        })
    }
}
pub fn print(status: &CompanionStatus) {
    println!("Companion {} ({})", status.version, status.target);
    println!("Enabled: {}  Ready: {}", status.enabled, status.ready);
    for (name, tool) in [
        ("r-dlp", &status.r_dlp),
        ("yt-dlp", &status.yt_dlp),
        ("FFmpeg", &status.ffmpeg),
    ] {
        println!(
            "{name}: {}{}",
            tool.version.as_deref().unwrap_or("not available"),
            if tool.configured {
                ""
            } else {
                " (not selected for the companion)"
            }
        );
    }
    println!("Tool versions describe installed executables; persistent workers are reloaded after a managed r-dlp update.");
}
