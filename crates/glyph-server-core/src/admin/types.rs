//! Wire types of the admin API (`/admin/v1/*`). Deliberately separate from
//! `glyph-protocol`: the Glyph app never talks to the admin API, only the
//! `glyph-server remote` CLI does.

use glyph_protocol::ProviderId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use crate::traits::CacheStats;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub access_expires_in_s: u64,
    pub refresh_token: String,
    pub refresh_expires_in_s: u64,
}

/// What the running host can change at runtime.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub set_keys: bool,
    pub set_limits: bool,
    pub cache_clear: bool,
    pub logs: bool,
    pub update: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub id: ProviderId,
    pub name: String,
    /// Needs a key at all (Audius, Deezer, Internet Archive, LRCLIB do not).
    pub needs_key: bool,
    pub enabled: bool,
    /// Temporarily skipped after an error or exhausted quota (10 minutes).
    pub skipped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub server_version: String,
    pub protocol_version: u32,
    /// `native` or `worker`.
    pub platform: String,
    pub uptime_s: Option<u64>,
    pub providers: Vec<ProviderStatus>,
    pub limits: Limits,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyInfo {
    pub name: String,
    pub provider: ProviderId,
    pub set: bool,
    /// Last four characters only (`****abcd`), never the key.
    pub masked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeysResponse {
    pub keys: Vec<KeyInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetKeysRequest {
    /// Key name -> new value; an empty string removes the key.
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetKeysResponse {
    pub updated: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestKeysRequest {
    pub provider: ProviderId,
    /// Test these candidate values instead of the stored ones (nothing is saved).
    #[serde(default)]
    pub values: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub provider: ProviderId,
    pub ok: bool,
    /// Plain-language explanation, safe to show to the user.
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheStatsResponse {
    pub supported: bool,
    pub stats: Option<CacheStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheClearResponse {
    pub cleared: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub rate_limit_per_min: u32,
    pub youtube_daily_budget_units: u64,
    pub import_max_pages: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogsResponse {
    pub lines: Vec<String>,
    /// Pass as `after` to get only newer lines.
    pub next: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateApplyResponse {
    pub message: String,
    pub restart_required: bool,
}
