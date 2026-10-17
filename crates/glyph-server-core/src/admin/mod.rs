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
use crate::server::{parse_body, rate_limit, Request, Response, Server};
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
    AccountDefaultsGet,
    AccountDefaultsPut,
    AccountGet,
    AccountPut,
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
        "account-defaults" if m == Put => (R::AccountDefaultsPut, Put),
        "account-defaults" => (R::AccountDefaultsGet, Get),
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
        rate_limit_per_min: svc.config.rate_limit_per_min,
        youtube_daily_budget_units: svc.config.youtube_daily_budget_units,
        import_max_pages: svc.config.import_max_pages,
    }
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
        rate_limit(svc, &req).await?;
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
        let claims = auth::verify_token(&key, token, 'a', now).map_err(|e| {
            let msg = match e {
                auth::TokenError::Expired => "access token expired",
                _ => "invalid access token",
            };
            ApiErr::new(401, "unauthorized", msg)
        })?;
        if is_revoked(svc, &claims.sid).await {
            return Err(ApiErr::new(401, "unauthorized", "this session was logged out"));
        }

        match route {
            R::Logout => {
                cache::put_json(&*svc.cache, &format!("admin:revoked:{}", claims.sid), &true, REFRESH_TTL_S).await;
                Ok(Response::json(200, &serde_json::json!({"ok": true})))
            }
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
                let l: Limits = parse_body(&req)?;
                if !(1..=10_000).contains(&l.rate_limit_per_min) {
                    return Err(ApiErr::bad_request("rate_limit_per_min must be between 1 and 10000"));
                }
                if !(100..=1_000_000).contains(&l.youtube_daily_budget_units) {
                    return Err(ApiErr::bad_request("youtube_daily_budget_units must be between 100 and 1000000"));
                }
                if !(1..=50).contains(&l.import_max_pages) {
                    return Err(ApiErr::bad_request("import_max_pages must be between 1 and 50"));
                }
                self.host.persist_limits(&l).await.map_err(host_err)?;
                self.update_config(|c| {
                    c.rate_limit_per_min = l.rate_limit_per_min;
                    c.youtube_daily_budget_units = l.youtube_daily_budget_units;
                    c.import_max_pages = l.import_max_pages;
                });
                Ok(Response::json(200, &l))
            }
            R::Logs => {
                let n = req.query.get("n").and_then(|v| v.parse::<usize>().ok()).unwrap_or(200).clamp(1, 1000);
                let after = req.query.get("after").and_then(|v| v.parse::<u64>().ok());
                Ok(Response::json(200, &self.host.logs(n, after).await.map_err(host_err)?))
            }
            R::UpdateCheck => Ok(Response::json(200, &self.host.update_check().await.map_err(host_err)?)),
            R::UpdateApply => Ok(Response::json(200, &self.host.update_apply().await.map_err(host_err)?)),
            R::AccountDefaultsGet | R::AccountDefaultsPut | R::AccountGet | R::AccountPut => {
                let acc = self.accounts.as_ref().ok_or_else(|| ApiErr::new(501, "not_supported", "accounts are not enabled on this server"))?;
                let handle = sub.strip_prefix("accounts/").unwrap_or("");
                match route {
                    R::AccountDefaultsGet => Ok(Response::json(200, &acc.admin_defaults().await?)),
                    R::AccountDefaultsPut => Ok(Response::json(200, &acc.admin_set_defaults(&parse_body::<AccountDefaults>(&req)?).await?)),
                    R::AccountGet => Ok(Response::json(200, &acc.admin_account(handle).await?)),
                    _ => Ok(Response::json(200, &acc.admin_set_limit(handle, parse_body::<AccountLimitRequest>(&req)?.device_limit).await?)),
                }
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
            svc.limiter.check(&format!("admin-login:{who}"), LOGIN_PER_MIN, 60).await
        {
            return Err(ApiErr::rate_limited(retry_after_s));
        }
        let b: LoginRequest = parse_body(req)?;
        // Always verify (constant work), even for an empty password.
        if !auth::verify_password(&b.password, hash) {
            let n = cache::get_json::<u64>(&*svc.cache, &fail_key).await.unwrap_or(0) + 1;
            if n >= MAX_LOGIN_FAILURES {
                cache::put_json(&*svc.cache, &lock_key, &(now + LOCKOUT_S * 1000), LOCKOUT_S).await;
                cache::put_json(&*svc.cache, &fail_key, &0u64, LOCKOUT_S).await;
            } else {
                cache::put_json(&*svc.cache, &fail_key, &n, LOCKOUT_S).await;
            }
            return Err(ApiErr::new(401, "invalid_credentials", "wrong password"));
        }
        cache::put_json(&*svc.cache, &fail_key, &0u64, 60).await;
        let sid = auth::session_id(key, now, &who);
        Ok(Response::json(200, &pair(key, &sid, now)))
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
