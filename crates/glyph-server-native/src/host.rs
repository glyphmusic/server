//! Native implementation of the admin host: persists to `server.toml`, serves
//! the log ring buffer and drives self-update.

use crate::logbuf::LogBuffer;
use crate::settings::{load_file, save};
use crate::update;
use async_trait::async_trait;
use glyph_server_core::admin::host::{AdminHost, HostError};
use glyph_server_core::admin::types::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct NativeHost {
    pub config_path: PathBuf,
    /// `companion.off` marker, also written by `glyph-server companion off`.
    pub companion_flag: PathBuf,
    pub logs: Arc<LogBuffer>,
    pub started: Instant,
    pub update_repo: String,
}

#[async_trait]
impl AdminHost for NativeHost {
    fn platform(&self) -> &'static str {
        "native"
    }

    fn uptime_s(&self) -> Option<u64> {
        Some(self.started.elapsed().as_secs())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { set_keys: true, set_limits: true, cache_clear: true, logs: true, update: true }
    }

    async fn persist_keys(&self, values: &BTreeMap<String, Option<String>>) -> Result<Vec<String>, HostError> {
        let mut cfg = load_file(&self.config_path, false).map_err(HostError::Failed)?;
        let mut warnings = Vec::new();
        for (name, v) in values {
            cfg.keys.set(name, v.clone());
            let env_name = name.to_ascii_uppercase();
            if std::env::var(&env_name).map(|e| !e.trim().is_empty()).unwrap_or(false) {
                warnings.push(format!(
                    "The environment variable {env_name} is set and takes priority after a restart; \
                     remove it so the saved value is used."
                ));
            }
        }
        warnings.extend(save(&self.config_path, &cfg).map_err(HostError::Failed)?);
        Ok(warnings)
    }

    async fn persist_limits(&self, l: &Limits) -> Result<(), HostError> {
        if l.audio_cache_days != 15 || l.audio_cache_max_gb != 10 || l.d1_max_mb != 500 {
            return Err(HostError::Unsupported("D1/R2 storage limits are Worker-only; this native server does not store audio".into()));
        }
        let mut cfg = load_file(&self.config_path, false).map_err(HostError::Failed)?;
        cfg.rate_limit_per_min = Some(l.rate_limit_per_min);
        cfg.require_account = Some(l.require_account);
        cfg.youtube_daily_budget_units = Some(l.youtube_daily_budget_units);
        cfg.import_max_pages = Some(l.import_max_pages);
        save(&self.config_path, &cfg).map_err(HostError::Failed)?;
        let switched = if l.companion_enabled {
            std::fs::remove_file(&self.companion_flag).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
        } else {
            std::fs::write(&self.companion_flag, b"off
")
        };
        switched.map_err(|e| HostError::Failed(format!("could not switch the companion: {e}")))
    }

    async fn logs(&self, max_lines: usize, after: Option<u64>) -> Result<LogsResponse, HostError> {
        Ok(self.logs.read(max_lines, after))
    }

    async fn update_check(&self) -> Result<UpdateInfo, HostError> {
        update::check_info(&self.update_repo).await.map_err(HostError::Failed)
    }

    async fn update_apply(&self) -> Result<UpdateApplyResponse, HostError> {
        update::apply(&self.update_repo).await.map_err(HostError::Failed)
    }
    async fn companion_status(&self) -> Result<CompanionStatus,HostError> {
        let file=load_file(&self.config_path,false).map_err(HostError::Failed)?;
        let settings=crate::settings::Settings::resolve(file,None,self.config_path.clone());
        Ok(crate::diagnostics::snapshot(&settings).await)
    }
    async fn companion_update(&self, rdlp: bool, apply: bool) -> Result<MaintenanceResponse,HostError> {
        crate::diagnostics::maintenance(&self.update_repo,rdlp,apply).await.map_err(HostError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persists_keys_and_limits_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let host = NativeHost {
            config_path: dir.path().join("server.toml"),
            companion_flag: dir.path().join("companion.off"),
            logs: LogBuffer::new(),
            started: Instant::now(),
            update_repo: "x/y".into(),
        };
        let mut m = BTreeMap::new();
        m.insert("youtube_api_key".to_string(), Some("AIzaSECRET".to_string()));
        host.persist_keys(&m).await.unwrap();
        host.persist_limits(&Limits { require_account: false, rate_limit_per_min: 7, youtube_daily_budget_units: 500, import_max_pages: 2, companion_enabled: false, mcp_relay: true, audio_cache_days: 15, audio_cache_max_gb: 10, d1_max_mb: 500 })
            .await
            .unwrap();
        assert!(host.companion_flag.exists());
        host.persist_limits(&Limits { require_account: false, rate_limit_per_min: 7, youtube_daily_budget_units: 500, import_max_pages: 2, companion_enabled: true, mcp_relay: true, audio_cache_days: 15, audio_cache_max_gb: 10, d1_max_mb: 500 })
            .await
            .unwrap();
        assert!(!host.companion_flag.exists());
        let cfg = load_file(&host.config_path, true).unwrap();
        assert_eq!(cfg.keys.youtube_api_key.as_deref(), Some("AIzaSECRET"));
        assert_eq!(cfg.rate_limit_per_min, Some(7));
        assert_eq!(cfg.require_account,Some(false));
        // removing
        m.insert("youtube_api_key".to_string(), None);
        host.persist_keys(&m).await.unwrap();
        assert!(load_file(&host.config_path, true).unwrap().keys.youtube_api_key.is_none());
        assert!(host.uptime_s().is_some());
    }
}
