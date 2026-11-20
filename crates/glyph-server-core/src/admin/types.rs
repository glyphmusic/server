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
    #[serde(default = "playlist_tracks")]
    pub playlist_max_tracks: usize,
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
    /// Space one account may use (synced data and pictures), in MB.
    #[serde(default = "account_mb")]
    pub account_max_mb: u32,
    /// Maximum total accounts allowed on this server; 0 = no limit.
    #[serde(default)]
    pub max_accounts: u32,
    /// YouTube songs per account or address and day; 0 = no limit.
    #[serde(default)]
    pub youtube_per_day: u32,
    /// YouTube audio and downloads only for signed-in accounts.
    #[serde(default)]
    pub youtube_require_account: bool,
    /// Oldest Glyph app version still accepted (`None` = every version).
    #[serde(default)]
    pub min_app_version: Option<String>,
    /// App release offered on the Stable update channel (`None` = GitHub's
    /// normal releases decide).
    #[serde(default)]
    pub stable_app_version: Option<String>,
    /// App release offered on the Latest update channel (`None` = newest).
    #[serde(default)]
    pub latest_app_version: Option<String>,
}

impl Default for Limits {
    fn default() -> Self {
        let config = crate::Config::new(Default::default());
        Self {
            require_account: config.require_account,
            rate_limit_per_min: config.rate_limit_per_min,
            youtube_daily_budget_units: config.youtube_daily_budget_units,
            import_max_pages: config.import_max_pages,
            playlist_max_tracks: config.playlist_max_tracks,
            companion_enabled: config.companion_enabled,
            mcp_relay: config.mcp_relay,
            audio_cache_days: config.audio_cache_days,
            audio_cache_max_gb: config.audio_cache_max_gb,
            d1_max_mb: config.d1_max_mb,
            account_max_mb: config.account_max_mb,
            max_accounts: config.max_accounts,
            youtube_per_day: config.youtube_per_day,
            youtube_require_account: config.youtube_require_account,
            min_app_version: config.min_app_version,
            stable_app_version: config.stable_app_version,
            latest_app_version: config.latest_app_version,
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
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub platform: String,
    pub last_seen: u64,
    /// Hardware hash the device last sent (ban it with `ban hardware`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,
    /// A hashed address is on file (banning the account bans it too).
    #[serde(default)]
    pub ip_known: bool,
    /// App version the device last reported (e.g. `0.12.13`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
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
    /// Space used by this account (synced data and pictures), in bytes.
    #[serde(default)]
    pub used_bytes: u64,
    /// Whether this account currently has a live account ban.
    #[serde(default)]
    pub banned: bool,
    /// May use the admin dashboard in the app.
    #[serde(default)]
    pub admin: bool,
    /// sha256 of the profile picture (`/v1/profiles/{handle}/images/{sha}`), if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// sha256 of the profile banner (`/v1/profiles/{handle}/images/{sha}`), if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
}

/// One row of `GET /admin/v1/accounts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminAccountSummary {
    pub id: String,
    pub handle: String,
    pub created_at: u64,
    pub device_limit: u8,
    pub device_limit_set: Option<u8>,
    pub devices: u64,
    pub last_seen: Option<u64>,
    /// Space used by live synced data, in bytes.
    pub sync_bytes: u64,
    #[serde(default)]
    pub banned: bool,
    /// May use the admin dashboard in the app.
    #[serde(default)]
    pub admin: bool,
    /// sha256 of the profile picture (`/v1/profiles/{handle}/images/{sha}`), if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// sha256 of the profile banner (`/v1/profiles/{handle}/images/{sha}`), if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
}

/// `GET /admin/v1/accounts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminAccountsResponse {
    pub accounts: Vec<AdminAccountSummary>,
    pub total: u64,
    pub limit: u32,
    pub offset: u32,
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

fn playlist_tracks() -> usize { 5_000 }
fn account_mb() -> u32 { 50 }

/// What `POST /admin/v1/bans` bans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BanTarget {
    /// `value` = handle. Also bans the hashed IPs and hardware of its devices
    /// unless `ip` / `hardware` are false.
    Account,
    /// `value` = handle, `device` = device id: that device's hardware and IP.
    Device,
    /// `value` = IP address or CIDR range.
    Ip,
    /// `value` = hardware hash from `accounts show`.
    Hardware,
    /// `value` = install id.
    Install,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanRequest {
    pub target: BanTarget,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `None` = until removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    #[serde(default = "yes")]
    pub ip: bool,
    #[serde(default = "yes")]
    pub hardware: bool,
    /// Report what would be banned without saving.
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /admin/v1/bans/remove`: a ban id, a handle (its account and every
/// ban derived from it), an IP/CIDR or a hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnbanRequest {
    pub target: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BansResponse {
    pub bans: Vec<crate::bans::Ban>,
    /// Bans this request added or removed.
    #[serde(default)]
    pub changed: Vec<crate::bans::Ban>,
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /admin/v1/accounts/{handle}/revoke`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RevokeRequest {
    /// `None` = every device of the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// `POST /admin/v1/accounts/{handle}/profile/reset`: remove an account's
/// profile picture and/or banner and delete their blobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResetProfileImagesRequest {
    #[serde(default)]
    pub avatar: bool,
    #[serde(default)]
    pub banner: bool,
}

/// `GET /admin/v1/stats`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsResponse {
    pub uptime_s: Option<u64>,
    /// Space the server uses (`None` when the host cannot measure it).
    #[serde(default)]
    pub storage: Option<StorageUsage>,
    /// `None` without an account backend.
    pub accounts: Option<crate::accounts::store::AccountStats>,
    /// Maximum total accounts allowed on this server; 0 = no limit.
    #[serde(default)]
    pub max_accounts: u32,
    pub bans: usize,
    pub cache: Option<CacheStats>,
    pub companion_enabled: bool,
    pub youtube_per_day: u32,
}

/// `POST /admin/v1/accounts/<handle>/admin`: give or take the admin role.
/// Only the server owner (admin password) may call it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminRoleRequest {
    pub admin: bool,
}

/// `GET /admin/v1/me`: who is calling and what they may do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminMe {
    /// Signed in with the admin password: everything is allowed.
    pub owner: bool,
    /// The account, when signed in with an admin account from the app.
    #[serde(default)]
    pub handle: Option<String>,
    /// Account admins cannot see keys, run updates or hand out the role.
    pub keys: bool,
    pub updates: bool,
    pub roles: bool,
}

/// Space the server uses, in bytes. Caps of 0 mean "no limit known".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageUsage {
    /// Cached songs and covers.
    #[serde(default)]
    pub audio_bytes: Option<u64>,
    #[serde(default)]
    pub audio_files: Option<u64>,
    #[serde(default)]
    pub audio_cap_bytes: u64,
    /// The database (accounts, synced data, settings).
    #[serde(default)]
    pub db_bytes: Option<u64>,
    #[serde(default)]
    pub db_cap_bytes: u64,
    /// Synced data of all accounts together.
    #[serde(default)]
    pub sync_bytes: Option<u64>,
}

/// One day of the server's history (taken once a day).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayStats {
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    #[serde(default)]
    pub accounts: u64,
    #[serde(default)]
    pub devices: u64,
    #[serde(default)]
    pub active_1d: u64,
    #[serde(default)]
    pub audio_bytes: u64,
    #[serde(default)]
    pub db_bytes: u64,
    #[serde(default)]
    pub sync_bytes: u64,
}

/// `GET /admin/v1/history`: oldest day first, at most [`HISTORY_DAYS`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub days: Vec<DayStats>,
}

/// Days of history the server keeps.
pub const HISTORY_DAYS: usize = 90;

/// `YYYY-MM-DD` (UTC) of a time in ms.
pub fn day_of(ms: u64) -> String {
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Adds `today` to `days` unless that day is already there; keeps the last
/// [`HISTORY_DAYS`]. `false` when nothing changed.
pub fn push_day(days: &mut Vec<DayStats>, today: DayStats) -> bool {
    if days.last().is_some_and(|d| d.day >= today.day) {
        return false;
    }
    days.push(today);
    if days.len() > HISTORY_DAYS {
        let cut = days.len() - HISTORY_DAYS;
        days.drain(..cut);
    }
    true
}

/// One app release on GitHub (`GET /admin/v1/app-releases`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRelease {
    /// Without the leading `v`.
    pub version: String,
    /// GitHub's own "pre-release" flag (what Latest gets without a pick).
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppReleasesResponse {
    pub releases: Vec<AppRelease>,
}

/// Why a client or account is currently blocked by automatic protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// 15-minute abuse lockout after repeated rate-limit hits (`temporarily_blocked`).
    AbuseBlock,
    /// Short-lived per-minute rate limit (`rate_limited`).
    RateLimited,
    /// Daily YouTube song cap reached (`youtube_daily_limit`).
    YoutubeLimit,
    /// Admin login lockout after repeated wrong passwords (`locked_out`).
    LoginLockout,
}

impl BlockReason {
    /// Higher = more severe (keeps an active abuse block from being downgraded
    /// to a shorter rate-limit entry).
    pub fn severity(self) -> u8 {
        match self {
            Self::RateLimited => 1,
            Self::YoutubeLimit => 2,
            Self::LoginLockout => 3,
            Self::AbuseBlock => 4,
        }
    }
}

fn one() -> u32 { 1 }

/// One live automatic block or rate limit (`GET /admin/v1/blocks`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveBlock {
    /// Opaque id used to lift the block (`POST /admin/v1/blocks/clear`).
    pub id: String,
    pub reason: BlockReason,
    /// `"account"`, `"device"`, `"install"`, or `"ip"`.
    pub kind: String,
    /// Operator-facing label (`@handle`, `@handle / Laptop`, `install ab12cd34`,
    /// `IP ab12cd34`). Never contains a raw client IP address.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Route area (`"api"`, `"media"`, `"sync"`, `"youtube"`, `"admin"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(default = "one")]
    pub hits: u32,
    /// True when the admin can lift this block early via `POST /admin/v1/blocks/clear`.
    #[serde(default)]
    pub clearable: bool,
}

/// Internal persisted record of an [`ActiveBlock`], carrying precomputed
/// limiter hashes so the block can be lifted without storing raw client IPs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedBlock {
    #[serde(flatten)]
    pub block: ActiveBlock,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear_hashes: Vec<String>,
}

/// `GET /admin/v1/blocks` and `POST /admin/v1/blocks/clear`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlocksResponse {
    pub blocks: Vec<ActiveBlock>,
}

/// `POST /admin/v1/blocks/clear`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClearBlockRequest {
    pub id: String,
}

/// Maximum live automatic blocks kept for the admin view.
pub const MAX_ACTIVE_BLOCKS: usize = 200;

/// Prunes expired blocks and inserts or merges `entry` by `id`.
pub fn upsert_block(list: &mut Vec<TrackedBlock>, mut entry: TrackedBlock, now: u64) {
    list.retain(|x| x.block.expires_at > now);
    if let Some(cur) = list.iter_mut().find(|x| x.block.id == entry.block.id) {
        cur.block.hits = cur.block.hits.saturating_add(1);
        cur.block.expires_at = cur.block.expires_at.max(entry.block.expires_at);
        if entry.block.reason.severity() >= cur.block.reason.severity() {
            cur.block.reason = entry.block.reason;
            if entry.block.scope.is_some() {
                cur.block.scope = entry.block.scope;
            }
        }
        cur.block.clearable |= entry.block.clearable;
        if cur.block.handle.is_none() && entry.block.handle.is_some() {
            cur.block.handle = entry.block.handle;
            cur.block.label = entry.block.label;
            cur.block.kind = entry.block.kind;
        }
        if cur.block.user_id.is_none() && entry.block.user_id.is_some() {
            cur.block.user_id = entry.block.user_id;
        }
        for h in entry.clear_hashes {
            if !cur.clear_hashes.contains(&h) {
                cur.clear_hashes.push(h);
            }
        }
    } else {
        entry.block.hits = entry.block.hits.max(1);
        list.push(entry);
    }
    list.sort_by(|a, b| {
        b.block
            .reason
            .severity()
            .cmp(&a.block.reason.severity())
            .then_with(|| b.block.expires_at.cmp(&a.block.expires_at))
    });
    if list.len() > MAX_ACTIVE_BLOCKS {
        list.truncate(MAX_ACTIVE_BLOCKS);
    }
}
