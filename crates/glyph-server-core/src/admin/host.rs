//! What the admin API needs from the platform it runs on. The native binary
//! persists to `server.toml`; the Worker cannot change secrets at runtime and
//! keeps the defaults ("unsupported", with a pointer to the right tool).

use super::types::*;
use crate::traits::MaybeSendSync;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum HostError {
    Unsupported(String),
    Failed(String),
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait AdminHost: MaybeSendSync {
    /// `native` or `worker`.
    fn platform(&self) -> &'static str;
    fn uptime_s(&self) -> Option<u64> {
        None
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// Persist key changes (`None` removes). Returns warnings for the user.
    async fn persist_keys(&self, _values: &BTreeMap<String, Option<String>>) -> Result<Vec<String>, HostError> {
        Err(HostError::Unsupported(
            "this server cannot change keys at runtime".into(),
        ))
    }
    async fn persist_limits(&self, _limits: &Limits) -> Result<(), HostError> {
        Err(HostError::Unsupported(
            "this server cannot change limits at runtime".into(),
        ))
    }
    async fn logs(&self, _max_lines: usize, _after: Option<u64>) -> Result<LogsResponse, HostError> {
        Err(HostError::Unsupported("logs are not available on this server".into()))
    }
    async fn update_check(&self) -> Result<UpdateInfo, HostError> {
        Err(HostError::Unsupported("updates are not managed by this server".into()))
    }
    async fn update_apply(&self) -> Result<UpdateApplyResponse, HostError> {
        Err(HostError::Unsupported("updates are not managed by this server".into()))
    }
    async fn companion_status(&self) -> Result<CompanionStatus, HostError> {
        Err(HostError::Unsupported("no companion management configured".into()))
    }
    async fn companion_update(&self, _rdlp: bool, _apply: bool) -> Result<MaintenanceResponse, HostError> {
        Err(HostError::Unsupported("no companion management configured".into()))
    }
}

/// Used when no host is configured (tests, embedding).
pub struct NoHost;

impl AdminHost for NoHost {
    fn platform(&self) -> &'static str {
        "unknown"
    }
}
