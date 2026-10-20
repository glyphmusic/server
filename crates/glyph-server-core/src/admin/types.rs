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
    /// Only signed-in account sessions may use the app API. Off by default.
    #[serde(default)]
    pub require_account: bool,
    pub rate_limit_per_min: u32,
    pub youtube_daily_budget_units: u64,
    pub import_max_pages: u32,
    /// yt-dlp companion (YouTube search and audio). Off = YouTube answers 403.
    #[serde(default = "enabled")]
    pub companion_enabled: bool,
    /// AI assistants (MCP) reach signed-in apps through this server.
    #[serde(default = "enabled")]
    pub mcp_relay: bool,
    /// Days companion audio stays in R2 (Worker only).
    #[serde(default = "audio_days")]
    pub audio_cache_days: u32,
    /// Total R2 GB ceiling including protected uploads; evicts cold caches only.
    #[serde(default = "audio_gb")]
    pub audio_cache_max_gb: u32,
    /// Independent D1 database ceiling in decimal MB (Worker only).
    #[serde(default = "d1_mb")]
    pub d1_max_mb: u32,
}

impl Default for Limits {
    fn default() -> Self {
        let config = crate::Config::new(Default::default());
        Self {
            require_account: config.require_account,
            rate_limit_per_min: config.rate_limit_per_min,
            youtube_daily_budget_units: config.youtube_daily_budget_units,
            import_max_pages: config.import_max_pages,
            companion_enabled: config.companion_enabled,
            mcp_relay: config.mcp_relay,
            audio_cache_days: config.audio_cache_days,
            audio_cache_max_gb: config.audio_cache_max_gb,
            d1_max_mb: config.d1_max_mb,
        }
    }
}

fn d1_mb() -> u32 { 500 }

fn enabled() -> bool {
    true
}

fn audio_days() -> u32 {
    15
}

fn audio_gb() -> u32 {
    10
}

/// `GET/PUT /admin/v1/account-defaults`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountDefaults {
    /// Devices per account unless an account has its own limit.
    pub device_limit: u8,
}

/// `PUT /admin/v1/accounts/{handle}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountLimitRequest {
    /// `None` = back to the server default.
    pub device_limit: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminDevice {
    pub name: String,
    pub platform: String,
    pub last_seen: u64,
}

/// `GET /admin/v1/accounts/{handle}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminAccount {
    pub id: String,
    pub handle: String,
    pub created_at: u64,
    /// Limit in force.
    pub device_limit: u8,
    /// Own limit; `None` = server default.
    pub device_limit_set: Option<u8>,
    pub devices: Vec<AdminDevice>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStatus {
    pub available: bool,
    pub version: Option<String>,
    pub configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionStatus {
    /// Version of the companion's running Glyph process, not the Docker image tag.
    pub version: String,
    pub target: String,
    pub enabled: bool,
    pub ready: bool,
    pub r_dlp: ToolStatus,
    pub yt_dlp: ToolStatus,
    pub ffmpeg: ToolStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceResponse {
    pub info: UpdateInfo,
    pub applied: bool,
    pub restart_required: bool,
    pub message: String,
}
