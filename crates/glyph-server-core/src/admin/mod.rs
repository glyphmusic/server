//! Admin API (`/admin/v1/*`), shared by the native server and the Worker.
//! Disabled unless an Argon2id password hash is configured.
//!
//! Security: Argon2id hash, short-lived HMAC tokens (15 min access, 30 days
//! refresh), per-IP login rate limit and lockout, HTTPS (or loopback) only,
//! keys are only ever shown as `****abcd`.

pub mod auth;
pub mod host;
pub mod keytest;
pub mod types;

use crate::cache;
use crate::config::{mask_secret, KEY_NAMES};
use crate::error::ApiErr;
use crate::search::is_skipped;
use crate::server::{parse_body, Request, Response, Server};
use crate::traits::{Method, RateDecision, Services};
use auth::{ACCESS_TTL_S, REFRESH_TTL_S};
use glyph_protocol::{ApiError, ProviderId, PROTOCOL_VERSION};
use host::HostError;
use std::collections::BTreeMap;
use std::sync::Arc;
use types::*;

pub const MAX_LOGIN_FAILURES: u64 = 5;
pub const LOCKOUT_S: u64 = 15 * 60;
pub const LOGIN_PER_MIN: u32 = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum R {
    Login,
    Refresh,
    Logout,
    Status,
    KeysGet,
    KeysPut,
    KeysTest,
    CacheStats,
    CacheClear,
    LimitsGet,
    LimitsPut,
    Logs,
    UpdateCheck,
    UpdateApply,
    CompanionStatus,
    CompanionCheck,
    CompanionApply,
    RdlpCheck,
    RdlpApply,
    AccountDefaultsGet,
    AccountDefaultsPut,
    AccountsList,
    AccountGet,
    AccountPut,
    AccountRevoke,
    AccountProfileReset,
    BansGet,
    BansAdd,
    BansRemove,
    BlocksGet,
    BlocksClear,
    JamsList,
    Stats,
    Me,
    History,
    AccountRole,
    AppReleases,
}

impl R {
    /// Only the server owner (admin password) may call these; an admin
    /// account from the app may not.
    fn owner_only(self) -> bool {
        matches!(
            self,
            R::KeysGet
                | R::KeysPut
                | R::KeysTest
                | R::Logs
                | R::UpdateCheck
                | R::UpdateApply
                | R::CompanionCheck
                | R::CompanionApply
                | R::RdlpCheck
                | R::RdlpApply
                | R::AccountRole
        )
    }
}

/// Who called the admin API.
enum Caller {
    /// Admin password session.
    Owner { sid: String },
    /// An account with the admin role, signed in from the app.
    Account(crate::accounts::store::User),
}

fn route(sub: &str, m: Method) -> Result<R, ApiErr> {
    use Method::*;
    let (r, want) = match sub {
        "login" => (R::Login, Post),
        "refresh" => (R::Refresh, Post),
        "logout" => (R::Logout, Post),
        "status" => (R::Status, Get),
        "keys" if m == Put => (R::KeysPut, Put),
        "keys" => (R::KeysGet, Get),
        "keys/test" => (R::KeysTest, Post),
        "cache/stats" => (R::CacheStats, Get),
        "cache/clear" => (R::CacheClear, Post),
        "limits" if m == Put => (R::LimitsPut, Put),
        "limits" => (R::LimitsGet, Get),
        "logs" => (R::Logs, Get),
        "update/check" => (R::UpdateCheck, Post),
        "update/apply" => (R::UpdateApply, Post),
        "companion/status" => (R::CompanionStatus, Get),
        "companion/update/check" => (R::CompanionCheck, Post),
        "companion/update/apply" => (R::CompanionApply, Post),
        "companion/r-dlp/check" => (R::RdlpCheck, Post),
        "companion/r-dlp/apply" => (R::RdlpApply, Post),
        "account-defaults" if m == Put => (R::AccountDefaultsPut, Put),
        "account-defaults" => (R::AccountDefaultsGet, Get),
        "bans" if m == Post => (R::BansAdd, Post),
        "bans" => (R::BansGet, Get),
        "bans/remove" => (R::BansRemove, Post),
        "blocks" => (R::BlocksGet, Get),
        "blocks/clear" => (R::BlocksClear, Post),
        "jams" => (R::JamsList, Get),
        "stats" => (R::Stats, Get),
        "me" => (R::Me, Get),
        "history" => (R::History, Get),
        "app-releases" => (R::AppReleases, Get),
        "accounts" => (R::AccountsList, Get),
        s if s.strip_prefix("accounts/").is_some_and(|h| h.ends_with("/profile/reset")) => (R::AccountProfileReset, Post),
        s if s.strip_prefix("accounts/").is_some_and(|h| h.ends_with("/admin")) => (R::AccountRole, Post),
        s if s.starts_with("accounts/") && s.ends_with("/revoke") => (R::AccountRevoke, Post),
        s if s.starts_with("accounts/") && m == Put => (R::AccountPut, Put),
        s if s.starts_with("accounts/") => (R::AccountGet, Get),
        _ => return Err(ApiErr::not_found("unknown admin endpoint")),
    };
    if m != want {
        return Err(ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint"));
    }
    Ok(r)
}

fn host_err(e: HostError) -> ApiErr {
    match e {
        HostError::Unsupported(m) => ApiErr::new(501, "not_supported", m),
        HostError::Failed(m) => ApiErr::new(500, "host_error", m),
    }
}

fn limited(code: &str, msg: &str, retry: u64) -> ApiErr {
    ApiErr {
        status: 429,
        body: ApiError { code: code.into(), message: msg.into(), retry_after_s: Some(retry) },
    }
}

fn limits_of(svc: &Services) -> Limits {
    Limits {
        require_account: svc.config.require_account,
        rate_limit_per_min: svc.config.rate_limit_per_min,
        youtube_daily_budget_units: svc.config.youtube_daily_budget_units,
        import_max_pages: svc.config.import_max_pages,
        playlist_max_tracks: svc.config.playlist_max_tracks,
        companion_enabled: svc.config.companion_enabled,
        mcp_relay: svc.config.mcp_relay,
        audio_cache_days: svc.config.audio_cache_days,
        audio_cache_max_gb: svc.config.audio_cache_max_gb,
        d1_max_mb: svc.config.d1_max_mb,
        account_max_mb: svc.config.account_max_mb,
        max_accounts: svc.config.max_accounts,
        youtube_per_day: svc.config.youtube_per_day,
        youtube_require_account: svc.config.youtube_require_account,
        min_app_version: svc.config.min_app_version.clone(),
        stable_app_version: svc.config.stable_app_version.clone(),
        latest_app_version: svc.config.latest_app_version.clone(),
    }
}

fn valid_min_app_version(v: &str) -> bool {
    if v.len() > 32 || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+') {
        return false;
    }
    let core = v.trim_start_matches(['v', 'V']).split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    (1..=3).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.parse::<u64>().is_ok())
}

fn valid_key_value(v: &str) -> bool {
    v.len() <= 512 && v.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

impl Server {
    pub(crate) async fn handle_admin(&self, svc: &Arc<Services>, req: Request) -> Response {
        match self.admin_inner(svc, req).await {
            Ok(r) => r,
            Err(e) => Response::error(e),
        }
    }

    async fn admin_inner(&self, svc: &Arc<Services>, req: Request) -> Result<Response, ApiErr> {
        let Some(hash) = svc.config.admin_password_hash.clone() else {
            return Err(ApiErr::new(
                404,
                "admin_disabled",
                "the admin API is disabled on this server (no admin password is set)",
            ));
        };
        let sub = req.path.trim_end_matches('/').strip_prefix("/admin/v1/").unwrap_or("").to_string();
        let route = route(&sub, req.method)?;
        if !(req.secure || req.local) {
            return Err(ApiErr::new(
                403,
                "https_required",
                "the admin API is only available over HTTPS (or from this machine)",
            ));
        }
        let key = auth::token_key(&hash);
        let now = svc.rt.now_ms();

        match route {
            R::Login => return self.login(svc, &req, &hash, &key, now).await,
            R::Refresh => {
                let b: RefreshRequest = parse_body(&req)?;
                let claims = auth::verify_token(&key, &b.refresh_token, 'r', now)
                    .map_err(|_| ApiErr::new(401, "unauthorized", "refresh token is invalid or expired; log in again"))?;
                if is_revoked(svc, &claims.sid).await {
                    return Err(ApiErr::new(401, "unauthorized", "this session was logged out"));
                }
                return Ok(Response::json(200, &pair(&key, &claims.sid, now)));
            }
            _ => {}
        }

        // Everything below needs a valid access token.
        let token = req
            .header("authorization")
            .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
            .map(str::trim)
            .ok_or_else(|| ApiErr::new(401, "unauthorized", "missing bearer token; log in first"))?;
        let caller = match auth::verify_token(&key, token, 'a', now) {
            Ok(claims) => {
                if is_revoked(svc, &claims.sid).await {
                    return Err(ApiErr::new(401, "unauthorized", "this session was logged out"));
                }
                Caller::Owner { sid: claims.sid }
            }
            Err(e) => {
                let msg = match e {
                    auth::TokenError::Expired => "access token expired",
                    _ => "invalid access token",
                };
                // Not an admin token: maybe an app sign-in of an admin account.
                let Some(acc) = &self.accounts else {
                    return Err(ApiErr::new(401, "unauthorized", msg));
                };
                match acc.admin_session(svc, token).await {
                    Ok(user) => Caller::Account(user),
                    Err(err) if err.status == 401 && err.body.code == "unauthorized" => {
                        return Err(ApiErr::new(401, "unauthorized", msg));
                    }
                    Err(err) => return Err(err),
                }
            }
        };
        if matches!(caller, Caller::Account(_)) && route.owner_only() {
            return Err(ApiErr::new(403, "owner_only", "only the server owner (admin password) can do this"));
        }

        match route {
            R::Logout => {
                if let Caller::Owner { sid } = &caller {
                    cache::put_json(&*svc.cache, &format!("admin:revoked:{sid}"), &true, REFRESH_TTL_S).await;
                }
                Ok(Response::json(200, &serde_json::json!({"ok": true})))
            }
            R::Me => {
                let owner = matches!(caller, Caller::Owner { .. });
                Ok(Response::json(200, &AdminMe {
                    owner,
                    handle: match &caller {
                        Caller::Account(u) => Some(u.handle.clone()),
                        Caller::Owner { .. } => None,
                    },
                    keys: owner,
                    updates: owner,
                    roles: owner && self.accounts.is_some(),
                }))
            }
            R::History => Ok(Response::json(200, &HistoryResponse { days: self.host.history().await })),
            R::AppReleases => Ok(Response::json(200, &app_releases(svc).await?)),
            R::Status => Ok(Response::json(200, &self.status(svc).await)),
            R::KeysGet => {
                let keys = KEY_NAMES
                    .iter()
                    .map(|(name, provider)| {
                        let v = svc.config.secrets.get(name);
                        KeyInfo {
                            name: (*name).to_string(),
                            provider: *provider,
                            set: v.is_some(),
                            masked: v.map(mask_secret),
                        }
                    })
                    .collect();
                Ok(Response::json(200, &KeysResponse { keys }))
            }
            R::KeysPut => {
                let b: SetKeysRequest = parse_body(&req)?;
                if b.values.is_empty() {
                    return Err(ApiErr::bad_request("no keys given"));
                }
                let mut changes: BTreeMap<String, Option<String>> = BTreeMap::new();
                for (name, value) in &b.values {
                    if !KEY_NAMES.iter().any(|(n, _)| n == name) {
                        return Err(ApiErr::bad_request(format!("unknown key name `{name}`")));
                    }
                    if !valid_key_value(value) {
                        return Err(ApiErr::bad_request(format!(
                            "the value for `{name}` contains spaces/control characters or is too long"
                        )));
                    }
                    changes.insert(name.clone(), Some(value.clone()).filter(|v| !v.is_empty()));
                }
                let warnings = self.host.persist_keys(&changes).await.map_err(host_err)?;
                self.update_config(|c| {
                    for (n, v) in &changes {
                        c.secrets.set(n, v.clone());
                    }
                });
                let touched: Vec<ProviderId> = KEY_NAMES
                    .iter()
                    .filter(|(n, _)| changes.contains_key(*n))
                    .map(|(_, p)| *p)
                    .collect();
                for p in touched {
                    invalidate_provider(svc, p).await;
                }
                Ok(Response::json(200, &SetKeysResponse { updated: changes.keys().cloned().collect(), warnings }))
            }
            R::KeysTest => {
                let b: TestKeysRequest = parse_body(&req)?;
                let mut secrets = svc.config.secrets.clone();
                if let Some(values) = &b.values {
                    for (n, v) in values {
                        if !valid_key_value(v) || !secrets.set(n, Some(v.clone())) {
                            return Err(ApiErr::bad_request(format!("bad key `{n}`")));
                        }
                    }
                }
                let o = keytest::test_provider(&*svc.http, &secrets, b.provider).await;
                Ok(Response::json(200, &TestResult { provider: b.provider, ok: o.ok, message: o.message }))
            }
            R::CacheStats => {
                let stats = svc.cache.stats().await;
                Ok(Response::json(200, &CacheStatsResponse { supported: stats.is_some(), stats }))
            }
            R::CacheClear => Ok(Response::json(200, &CacheClearResponse { cleared: svc.cache.clear().await })),
            R::LimitsGet => Ok(Response::json(200, &limits_of(svc))),
            R::LimitsPut => {
                let mut l: Limits = parse_body(&req)?;
                // Older admin clients must not silently reopen a protected API.
                let body: serde_json::Value = parse_body(&req)?;
                if body.get("require_account").is_none() { l.require_account = svc.config.require_account; }
                if body.get("playlist_max_tracks").is_none() { l.playlist_max_tracks = svc.config.playlist_max_tracks; }
                if body.get("account_max_mb").is_none() { l.account_max_mb = svc.config.account_max_mb; }
                if body.get("max_accounts").is_none() { l.max_accounts = svc.config.max_accounts; }
                if body.get("youtube_require_account").is_none() { l.youtube_require_account = svc.config.youtube_require_account; }
                if body.get("min_app_version").is_none() {
                    l.min_app_version = svc.config.min_app_version.clone();
                } else {
                    l.min_app_version = l.min_app_version.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
                }
                if let Some(ref v) = l.min_app_version {
                    if !valid_min_app_version(v) {
                        return Err(ApiErr::bad_request("min_app_version must look like a version, e.g. 0.13.0"));
                    }
                }
                // Release channels: absent keeps them, "" or null clears them.
                for (key, field, old) in [
                    ("stable_app_version", &mut l.stable_app_version, &svc.config.stable_app_version),
                    ("latest_app_version", &mut l.latest_app_version, &svc.config.latest_app_version),
                ] {
                    if body.get(key).is_none() {
                        *field = old.clone();
                    } else {
                        *field = field.take().map(|v| v.trim().trim_start_matches(['v', 'V']).to_string()).filter(|v| !v.is_empty());
                    }
                    if let Some(v) = field.as_deref() {
                        if !valid_min_app_version(v) {
                            return Err(ApiErr::bad_request(format!("{key} must look like a version, e.g. 0.12.10")));
                        }
                    }
                }
                if l.require_account && self.accounts.is_none() {
                    return Err(ApiErr::bad_request("require_account needs an enabled account backend"));
                }
                if l.youtube_require_account && self.accounts.is_none() {
                    return Err(ApiErr::bad_request("youtube_require_account needs an enabled account backend"));
                }
                if !(1..=10_000).contains(&l.rate_limit_per_min) {
                    return Err(ApiErr::bad_request("rate_limit_per_min must be between 1 and 10000"));
                }
                if !(100..=1_000_000).contains(&l.youtube_daily_budget_units) {
                    return Err(ApiErr::bad_request("youtube_daily_budget_units must be between 100 and 1000000"));
                }
                if !(1..=50_000).contains(&l.playlist_max_tracks) {
                    return Err(ApiErr::bad_request("playlist_max_tracks must be between 1 and 50000"));
                }
                if !(1..=10_000).contains(&l.account_max_mb) {
                    return Err(ApiErr::bad_request("account_max_mb must be between 1 and 10000"));
                }
                if l.max_accounts > 1_000_000 {
                    return Err(ApiErr::bad_request("max_accounts must be between 0 (no limit) and 1000000"));
                }
                if !(1..=50).contains(&l.import_max_pages) {
                    return Err(ApiErr::bad_request("import_max_pages must be between 1 and 50"));
                }
                if !(1..=365).contains(&l.audio_cache_days) {
                    return Err(ApiErr::bad_request("audio_cache_days must be between 1 and 365"));
                }
                if !(1..=1000).contains(&l.audio_cache_max_gb) {
                    return Err(ApiErr::bad_request("audio_cache_max_gb must be between 1 and 1000"));
                }
                if l.youtube_per_day > 100_000 {
                    return Err(ApiErr::bad_request("youtube_per_day must be between 0 (no limit) and 100000"));
                }
                if !(64..=4_000).contains(&l.d1_max_mb) {
                    return Err(ApiErr::bad_request("d1_max_mb must be between 64 and 4000; provider plan limits still apply"));
                }
                self.host.persist_limits(&l).await.map_err(host_err)?;
                self.update_config(|c| {
                    c.require_account = l.require_account;
                    c.rate_limit_per_min = l.rate_limit_per_min;
                    c.youtube_daily_budget_units = l.youtube_daily_budget_units;
                    c.import_max_pages = l.import_max_pages;
                    c.playlist_max_tracks = l.playlist_max_tracks;
                    c.companion_enabled = l.companion_enabled;
                    c.mcp_relay = l.mcp_relay;
                    c.audio_cache_days = l.audio_cache_days;
                    c.audio_cache_max_gb = l.audio_cache_max_gb;
                    c.d1_max_mb = l.d1_max_mb;
                    c.account_max_mb = l.account_max_mb;
                    c.max_accounts = l.max_accounts;
                    c.youtube_per_day = l.youtube_per_day;
                    c.youtube_require_account = l.youtube_require_account;
                    c.min_app_version = l.min_app_version.clone();
                    c.stable_app_version = l.stable_app_version.clone();
                    c.latest_app_version = l.latest_app_version.clone();
                });
                Ok(Response::json(200, &l))
            }
            R::Logs => {
                let n = req.query.get("n").and_then(|v| v.parse::<usize>().ok()).unwrap_or(200).clamp(1, 1000);
                let after = req.query.get("after").and_then(|v| v.parse::<u64>().ok());
                Ok(Response::json(200, &self.host.logs(n, after).await.map_err(host_err)?))
            }
            R::CompanionStatus => Ok(Response::json(200, &self.host.companion_status().await.map_err(host_err)?)),
            R::CompanionCheck | R::CompanionApply | R::RdlpCheck | R::RdlpApply => Ok(Response::json(200,
                &self.host.companion_update(matches!(route,R::RdlpCheck|R::RdlpApply), matches!(route,R::CompanionApply|R::RdlpApply)).await.map_err(host_err)?)),
            R::UpdateCheck => Ok(Response::json(200, &self.host.update_check().await.map_err(host_err)?)),
            R::UpdateApply => Ok(Response::json(200, &self.host.update_apply().await.map_err(host_err)?)),
            R::AccountDefaultsGet | R::AccountDefaultsPut | R::AccountsList | R::AccountGet | R::AccountPut | R::AccountRevoke | R::AccountProfileReset | R::AccountRole => {
                let acc = self.accounts.as_ref().ok_or_else(|| ApiErr::new(501, "not_supported", "accounts are not enabled on this server"))?;
                let handle = sub.strip_prefix("accounts/").unwrap_or("");
                let target = handle
                    .strip_suffix("/profile/reset")
                    .or_else(|| handle.strip_suffix("/revoke"))
                    .or_else(|| handle.strip_suffix("/admin"))
                    .unwrap_or(handle);
                if matches!(route, R::AccountPut | R::AccountRevoke | R::AccountProfileReset) {
                    self.guard_target(&caller, target, svc, now).await?;
                }
                match route {
                    R::AccountRole => {
                        let b: AdminRoleRequest = parse_body(&req)?;
                        Ok(Response::json(200, &acc.admin_set_role(target, b.admin, &svc.config.bans, now).await?))
                    }
                    R::AccountProfileReset => {
                        let b: ResetProfileImagesRequest = parse_body(&req)?;
                        Ok(Response::json(200, &acc.admin_reset_profile_images(target, b.avatar, b.banner, &svc.config.bans, now).await?))
                    }
                    R::AccountDefaultsGet => Ok(Response::json(200, &acc.admin_defaults().await?)),
                    R::AccountDefaultsPut => Ok(Response::json(200, &acc.admin_set_defaults(&parse_body::<AccountDefaults>(&req)?).await?)),
                    R::AccountsList => {
                        let limit = req.query.get("limit").and_then(|v| v.parse::<u32>().ok()).unwrap_or(100).clamp(1, 500);
                        let offset = req.query.get("offset").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                        let q = req.query.get("q").map(|s| s.trim()).filter(|s| !s.is_empty());
                        Ok(Response::json(200, &acc.admin_list(limit, offset, q, &svc.config.bans, now).await?))
                    }
                    R::AccountGet => Ok(Response::json(200, &acc.admin_account(handle, &svc.config.bans, now).await?)),
                    R::AccountRevoke => {
                        let handle = handle.strip_suffix("/revoke").unwrap_or(handle);
                        Ok(Response::json(200, &acc.admin_revoke(handle, parse_body::<RevokeRequest>(&req)?.device.as_deref(), &svc.config.bans, now).await?))
                    }
                    _ => Ok(Response::json(200, &acc.admin_set_limit(handle, parse_body::<AccountLimitRequest>(&req)?.device_limit, &svc.config.bans, now).await?)),
                }
            }
            R::BansGet => {
                let now = svc.rt.now_ms();
                let bans = svc.config.bans.bans.iter().filter(|b| b.live(now)).cloned().collect();
                Ok(Response::json(200, &BansResponse { bans, changed: Vec::new(), dry_run: false }))
            }
            R::BansAdd => {
                let b: BanRequest = parse_body(&req)?;
                if matches!(b.target, BanTarget::Account | BanTarget::Device) {
                    self.guard_target(&caller, b.value.trim(), svc, now).await?;
                }
                self.ban(svc, b).await
            }
            R::BansRemove => self.unban(svc, parse_body(&req)?).await,
            R::BlocksGet => {
                let blocks = self.list_blocks(now).await.into_iter().map(|t| t.block).collect();
                Ok(Response::json(200, &BlocksResponse { blocks }))
            }
            R::BlocksClear => {
                let b: ClearBlockRequest = parse_body(&req)?;
                let id = b.id.trim();
                if id.is_empty() || id.len() > 64 {
                    return Err(ApiErr::bad_request("invalid block id"));
                }
                let blocks = self.clear_block(svc, id, now).await?;
                Ok(Response::json(200, &BlocksResponse { blocks }))
            }
            R::JamsList => {
                let res = match &self.accounts {
                    Some(a) => a.admin_jams(now).await?,
                    None => AdminJamsResponse::default(),
                };
                Ok(Response::json(200, &res))
            }
            R::Stats => {
                let now = svc.rt.now_ms();
                let storage = self.host.storage().await.map(|mut s| {
                    s.audio_cap_bytes = u64::from(svc.config.audio_cache_max_gb) * 1_000_000_000;
                    s.db_cap_bytes = u64::from(svc.config.d1_max_mb) * 1_000_000;
                    s
                });
                Ok(Response::json(200, &StatsResponse {
                    uptime_s: self.host.uptime_s(),
                    storage,
                    accounts: match &self.accounts { Some(a) => a.admin_stats(now).await, None => None },
                    max_accounts: svc.config.max_accounts,
                    bans: svc.config.bans.bans.iter().filter(|b| b.live(now)).count(),
                    cache: svc.cache.stats().await,
                    companion_enabled: svc.config.companion_enabled,
                    youtube_per_day: svc.config.youtube_per_day,
                }))
            }
            R::Login | R::Refresh => unreachable!(),
        }
    }

    async fn login(
        &self,
        svc: &Arc<Services>,
        req: &Request,
        hash: &str,
        key: &[u8],
        now: u64,
    ) -> Result<Response, ApiErr> {
        let who = req.client_ip.clone().unwrap_or_else(|| "unknown".into());
        let lock_key = format!("admin:lock:{who}");
        let fail_key = format!("admin:fail:{who}");
        if let Some(until) = cache::get_json::<u64>(&*svc.cache, &lock_key).await {
            if until > now {
                return Err(limited(
                    "locked_out",
                    "too many wrong passwords; try again later",
                    (until - now).div_ceil(1000),
                ));
            }
        }
        if let RateDecision::Limited { retry_after_s } =
            svc.limiter.check_admin(&format!("admin-login:{who}"), LOGIN_PER_MIN, 60).await
        {
            return Err(ApiErr::rate_limited(retry_after_s));
        }
        let b: LoginRequest = parse_body(req)?;
        // Always verify (constant work), even for an empty password.
        if !auth::verify_password(&b.password, hash) {
            let n = cache::get_json::<u64>(&*svc.cache, &fail_key).await.unwrap_or(0) + 1;
            if n >= MAX_LOGIN_FAILURES {
                let until = now + LOCKOUT_S * 1000;
                cache::put_json(&*svc.cache, &lock_key, &until, LOCKOUT_S).await;
                cache::put_json(&*svc.cache, &fail_key, &0u64, LOCKOUT_S).await;
                let principal = format!("ip:{who}");
                let (kind, label) = crate::server::block_identity(svc, &principal, None, None);
                let id = crate::access::token_hash(&format!("login|{principal}"))[..12].to_string();
                self.record_block(
                    TrackedBlock {
                        block: ActiveBlock {
                            id,
                            reason: BlockReason::LoginLockout,
                            kind,
                            label,
                            handle: None,
                            user_id: None,
                            scope: Some("admin".into()),
                            created_at: now,
                            expires_at: until,
                            hits: 1,
                            clearable: false,
                        },
                        clear_hashes: Vec::new(),
                    },
                    now,
                )
                .await;
            } else {
                cache::put_json(&*svc.cache, &fail_key, &n, LOCKOUT_S).await;
            }
            return Err(ApiErr::new(401, "invalid_credentials", "wrong password"));
        }
        cache::put_json(&*svc.cache, &fail_key, &0u64, 60).await;
        let sid = auth::session_id(key, now, &who);
        Ok(Response::json(200, &pair(key, &sid, now)))
    }

    /// An admin account may not sign out, limit or ban another admin account;
    /// only the owner can (the owner hands out the role, too).
    async fn guard_target(&self, caller: &Caller, handle: &str, svc: &Arc<Services>, now: u64) -> Result<(), ApiErr> {
        let Caller::Account(me) = caller else { return Ok(()) };
        let Some(acc) = &self.accounts else { return Ok(()) };
        let Ok(target) = acc.admin_account(handle, &svc.config.bans, now).await else { return Ok(()) };
        if target.admin && target.id != me.id {
            return Err(ApiErr::new(403, "owner_only", "only the server owner can change another admin"));
        }
        Ok(())
    }

    async fn ban(&self, svc: &Arc<Services>, b: BanRequest) -> Result<Response, ApiErr> {
        use crate::bans::{Ban, BanKind};
        let now = svc.rt.now_ms();
        let mut list = (*svc.config.bans).clone();
        list.prune(now);
        if list.salt.is_empty() {
            return Err(ApiErr::unavailable("ban storage is not ready yet; retry in a minute"));
        }
        let reason = b.reason.map(|r| r.chars().filter(|c| !c.is_control()).take(200).collect::<String>()).filter(|r| !r.trim().is_empty());
        let expires_at = match b.days {
            Some(0) => return Err(ApiErr::bad_request("days must be at least 1 (leave it out for a permanent ban)")),
            Some(d) => Some(now.saturating_add(u64::from(d.min(3650)) * 86_400_000)),
            None => None,
        };
        let mut wanted: Vec<(BanKind, String, Option<String>, Option<String>)> = Vec::new();
        let value = b.value.trim();
        match b.target {
            BanTarget::Ip => {
                let net = crate::bans::parse_net(value).ok_or_else(|| ApiErr::bad_request(format!("`{value}` is not an IP address or CIDR range")))?;
                wanted.push((BanKind::Ip, net, None, None));
            }
            BanTarget::Hardware => {
                if value.len() != 64 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return Err(ApiErr::bad_request("use the 64-character hardware hash from `glyph remote accounts show`"));
                }
                wanted.push((BanKind::Hardware, value.to_ascii_lowercase(), None, None));
            }
            BanTarget::Install => {
                let h = crate::bans::install_hash(value).ok_or_else(|| ApiErr::bad_request("not an install id"))?;
                wanted.push((BanKind::Install, h, Some(format!("install {}", &value[..value.len().min(8)])), None));
            }
            BanTarget::Account | BanTarget::Device => {
                let acc = self.accounts.as_ref().ok_or_else(|| ApiErr::new(501, "not_supported", "accounts are not enabled on this server"))?;
                let (user, devices) = acc.admin_devices(value).await?;
                let label = format!("@{}", user.handle);
                if b.target == BanTarget::Account {
                    wanted.push((BanKind::Account, user.id.clone(), Some(label.clone()), None));
                }
                let chosen: Vec<_> = devices.iter().filter(|d| b.target == BanTarget::Account || b.device.as_deref() == Some(d.id.as_str())).collect();
                if b.target == BanTarget::Device && chosen.is_empty() {
                    return Err(ApiErr::not_found("no such device on this account (see glyph remote accounts show)"));
                }
                for d in chosen {
                    let l = Some(format!("{label} / {}", d.name));
                    if b.ip || b.target == BanTarget::Device {
                        if let Some(h) = &d.ip_hash { wanted.push((BanKind::IpHash, h.clone(), l.clone(), Some(user.id.clone()))); }
                    }
                    if b.hardware || b.target == BanTarget::Device {
                        if let Some(h) = &d.hw_hash { wanted.push((BanKind::Hardware, h.clone(), l.clone(), Some(user.id.clone()))); }
                    }
                }
                if wanted.is_empty() {
                    return Err(ApiErr::bad_request("this device has not been seen since bans were enabled; ban the account instead"));
                }
            }
        }
        let mut changed = Vec::new();
        for (kind, value, label, origin) in wanted {
            if list.bans.iter().chain(&changed).any(|x: &Ban| x.kind == kind && x.value == value) {
                continue;
            }
            let id = crate::access::token_hash(&format!("{kind:?}|{value}|{now}"))[..10].to_string();
            changed.push(Ban { id, kind, value, label, reason: reason.clone(), created_at: now, expires_at, origin });
        }
        if list.bans.len() + changed.len() > crate::bans::MAX_BANS {
            return Err(ApiErr::bad_request(format!("at most {} bans; remove old ones first", crate::bans::MAX_BANS)));
        }
        if !b.dry_run && !changed.is_empty() {
            list.bans.extend(changed.iter().cloned());
            self.host.persist_bans(&list).await.map_err(host_err)?;
            self.update_config(|c| c.bans = Arc::new(list.clone()));
        }
        Ok(Response::json(200, &BansResponse { bans: list.bans, changed, dry_run: b.dry_run }))
    }

    async fn unban(&self, svc: &Arc<Services>, b: UnbanRequest) -> Result<Response, ApiErr> {
        let now = svc.rt.now_ms();
        let mut list = (*svc.config.bans).clone();
        list.prune(now);
        let t = b.target.trim();
        let net = crate::bans::parse_net(t);
        let account = match (&self.accounts, net.is_none() && t.len() < 40) {
            (Some(a), true) => a.admin_devices(t).await.ok().map(|(u, _)| u.id),
            _ => None,
        };
        let hit = |x: &crate::bans::Ban| {
            x.id == t
                || x.value == t.to_ascii_lowercase()
                || net.as_deref() == Some(x.value.as_str())
                || account.as_deref().is_some_and(|a| x.value == a || x.origin.as_deref() == Some(a))
        };
        let changed: Vec<_> = list.bans.iter().filter(|&x| hit(x)).cloned().collect();
        if changed.is_empty() {
            return Err(ApiErr::not_found("no ban matches; list them with glyph remote ban list"));
        }
        if !b.dry_run {
            list.bans.retain(|x| !hit(x));
            self.host.persist_bans(&list).await.map_err(host_err)?;
            self.update_config(|c| c.bans = Arc::new(list.clone()));
        }
        Ok(Response::json(200, &BansResponse { bans: list.bans, changed, dry_run: b.dry_run }))
    }

    async fn status(&self, svc: &Arc<Services>) -> StatusResponse {
        let mut providers = Vec::new();
        for id in ProviderId::ALL {
            if id == ProviderId::MusicBrainz {
                continue;
            }
            let needs_key = KEY_NAMES.iter().any(|(_, p)| *p == id);
            providers.push(ProviderStatus {
                id,
                name: id.display_name().to_string(),
                needs_key,
                enabled: svc.config.provider_enabled(id),
                skipped: is_skipped(svc, id).await,
            });
        }
        StatusResponse {
            server_version: svc.config.server_version.clone(),
            protocol_version: PROTOCOL_VERSION,
            platform: self.host.platform().to_string(),
            uptime_s: self.host.uptime_s(),
            providers,
            limits: limits_of(svc),
            capabilities: Capabilities { cache_clear: svc.cache.stats().await.is_some(), ..self.host.capabilities() },
        }
    }
}

fn pair(key: &[u8], sid: &str, now: u64) -> TokenPair {
    TokenPair {
        access_token: auth::issue(key, 'a', sid, now, ACCESS_TTL_S),
        access_expires_in_s: ACCESS_TTL_S,
        refresh_token: auth::issue(key, 'r', sid, now, REFRESH_TTL_S),
        refresh_expires_in_s: REFRESH_TTL_S,
    }
}

/// Where the Glyph apps are released (the update channels pick from these).
pub const APPS_REPO: &str = "glyphmusic/apps";

/// The app's GitHub releases for the version picker, newest first (cached
/// 10 minutes so the dashboard does not use up GitHub's rate limit).
async fn app_releases(svc: &Services) -> Result<AppReleasesResponse, ApiErr> {
    const KEY: &str = "admin:app_releases";
    if let Some(r) = cache::get_json::<AppReleasesResponse>(&*svc.cache, KEY).await {
        return Ok(r);
    }
    #[derive(serde::Deserialize)]
    struct Gh {
        tag_name: String,
        #[serde(default)]
        prerelease: bool,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        published_at: Option<String>,
    }
    let mut req = crate::traits::HttpRequest::get(format!("https://api.github.com/repos/{APPS_REPO}/releases?per_page=20"));
    req.headers.push(("accept".into(), "application/vnd.github+json".into()));
    req.headers.push(("user-agent".into(), "glyph-server".into()));
    let resp = svc.http.send(req).await.map_err(|_| ApiErr::unavailable("GitHub did not answer; try again later"))?;
    if resp.status != 200 {
        return Err(ApiErr::unavailable("GitHub did not answer; try again later"));
    }
    let list: Vec<Gh> = serde_json::from_slice(&resp.body).map_err(|_| ApiErr::unavailable("GitHub sent something unexpected"))?;
    let releases = list
        .into_iter()
        .filter(|r| !r.draft)
        .map(|r| AppRelease { version: r.tag_name.trim_start_matches(['v', 'V']).to_string(), prerelease: r.prerelease, published_at: r.published_at })
        .filter(|r| valid_min_app_version(&r.version))
        .collect();
    let out = AppReleasesResponse { releases };
    cache::put_json(&*svc.cache, KEY, &out, 600).await;
    Ok(out)
}

async fn is_revoked(svc: &Services, sid: &str) -> bool {
    cache::get_json::<bool>(&*svc.cache, &format!("admin:revoked:{sid}")).await.unwrap_or(false)
}

/// After a key change: forget cached OAuth tokens and the 10-minute skip marker.
async fn invalidate_provider(svc: &Services, p: ProviderId) {
    svc.cache.put(&format!("skip:{}", p.badge()), vec![], 0).await;
    match p {
        ProviderId::Spotify => svc.cache.put("token:spotify", b"null".to_vec(), 0).await,
        ProviderId::SoundCloud => svc.cache.put("token:soundcloud", b"null".to_vec(), 0).await,
        _ => {}
    }
}

#[cfg(test)]
mod tests;
