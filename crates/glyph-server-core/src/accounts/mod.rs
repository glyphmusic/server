//! Optional accounts: @handles, passkey / pairing-code sign-in, per-device
//! tokens with a device limit, last-writer-wins library sync and a cover cache.
//! Design: `docs/ACCOUNTS.md` in the app repo.
//!
//! Storage is behind [`store::AccountStore`] / [`store::BlobStore`]; randomness
//! behind [`Entropy`] (this crate has no RNG of its own). Hosts enable accounts
//! with [`crate::Server::with_accounts`]; without it every route here is a 404.

pub mod profile;
pub mod store;
pub mod webauthn;

#[cfg(test)]
mod tests;

use crate::error::ApiErr;
use crate::server::{Body, Request, Response};
use crate::traits::{MaybeSendSync, Method, RateDecision, Services};
use glyph_protocol::{
    Account, AccountPatch, ApproveRequest, ApproveResponse, DeleteAccountRequest, DevicePatch, DevicesResponse,
    HandleCheck, LinkStartResponse, LoginPollRequest, LoginPollResponse, LoginStartRequest, LoginStartResponse,
    PasswordLoginRequest, PasswordRegisterRequest, PasswordSalt, PasswordSetRequest, RefreshRequest, Session,
    SignupResponse, SyncItem, SyncPullResponse, SyncPushRequest, SyncPushResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use crate::share::{ShareRequest, ShareResponse, SharedPlaylist};
use store::{AccountStore, BlobStore, Device, Item, Login, Passkey, Share, StoreError, User};
use webauthn::{b64, unb64, Expect};

const SEC: u64 = 1000;
const MIN: u64 = 60 * SEC;
const DAY: u64 = 24 * 60 * MIN;
pub const ACCESS_TTL: u64 = 60 * MIN;
pub const REFRESH_TTL: u64 = 90 * DAY;
pub const LOGIN_TTL: u64 = 10 * MIN;
pub const HANDLE_HOLD: u64 = 30 * DAY;
pub const HANDLE_COOLDOWN: u64 = 30 * DAY;
pub const TOMBSTONE_TTL: u64 = 180 * DAY;
const CLOCK_SKEW: u64 = 5 * MIN;
const TOUCH_EVERY: u64 = 60 * MIN;
pub const MAX_PUSH_ITEMS: usize = 500;
pub const MAX_PUSH_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ITEM_BYTES: usize = 1024 * 1024;
pub const MAX_OWN_COVER: usize = 512 * 1024;
pub const MAX_CACHED_COVER: usize = 2 * 1024 * 1024;
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const RESERVED: &[&str] = &[
    "admin", "administrator", "glyph", "glyphmusic", "support", "help", "root", "system", "api", "www", "mod",
    "moderator", "official", "staff", "security", "null", "undefined", "me", "account", "settings", "team",
];

/// Cryptographically secure random bytes from the host.
pub trait Entropy: MaybeSendSync {
    fn fill(&self, buf: &mut [u8]);
}

pub struct Accounts {
    pub store: Arc<dyn AccountStore>,
    /// R2 on the Worker; `None` disables both cover routes.
    pub blobs: Option<Arc<dyn BlobStore>>,
    pub entropy: Arc<dyn Entropy>,
    /// Public origin of this server, e.g. `https://glyph-server.glyphmusic.workers.dev`.
    pub origin: String,
    /// More allowed WebAuthn origins (mobile apps, `android:apk-key-hash:...`).
    pub extra_origins: Vec<String>,
    /// Passkey domain, e.g. `glyph.my` for `https://api.glyph.my`; must be the
    /// origin's host or a parent of it. `None` uses the origin's host.
    pub rp_id: Option<String>,
}

/// Largest request body a route accepts (hosts cap uploads with this).
pub fn max_body_bytes(path: &str) -> Option<usize> {
    if path == "/v1/sync/push" {
        Some(MAX_PUSH_BYTES)
    } else if let Some(n) = crate::share::max_body_bytes(path) {
        Some(n)
    } else if path.starts_with("/v1/sync/covers/") || path == "/v1/profile/avatar" {
        Some(MAX_OWN_COVER)
    } else if path == "/v1/profile/banner" {
        Some(profile::MAX_BANNER)
    } else {
        None
    }
}

/// Paths owned by this module.
pub fn owns(path: &str) -> bool {
    ["/v1/account", "/v1/devices", "/v1/sync", "/v1/covers/", "/account/", "/v1/share", "/p/", "/v1/profile", "/v1/profiles/"]
        .iter()
        .any(|p| path == p.trim_end_matches('/') || path.starts_with(&format!("{}/", p.trim_end_matches('/'))))
}

fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn store_err(_: StoreError) -> ApiErr {
    ApiErr::new(500, "storage_error", "account storage unavailable")
}

fn unauthorized() -> ApiErr {
    ApiErr::new(401, "unauthorized", "sign in again")
}

fn json_body<T: serde::de::DeserializeOwned>(req: &Request, max: usize) -> Result<T, ApiErr> {
    if req.body.len() > max {
        return Err(ApiErr::new(413, "payload_too_large", "request body too large"));
    }
    serde_json::from_slice(&req.body).map_err(|_| ApiErr::bad_request("request body is not valid JSON for this endpoint"))
}

/// `Ok(handle)` normalized, or the reason it cannot be used.
pub fn normalize_handle(raw: &str) -> Result<String, &'static str> {
    let h = raw.trim().trim_start_matches('@').to_ascii_lowercase();
    if !(3..=20).contains(&h.len())
        || !h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || h.bytes().all(|b| b == b'_')
    {
        return Err("invalid");
    }
    if RESERVED.contains(&h.as_str()) {
        return Err("reserved");
    }
    Ok(h)
}

fn clean_name(raw: &str) -> String {
    let s: String = raw.chars().filter(|c| !c.is_control()).collect();
    let s = s.trim();
    let s: String = s.chars().take(48).collect();
    if s.is_empty() { "Device".into() } else { s }
}

fn clean_platform(raw: &str) -> String {
    let p = raw.trim().to_ascii_lowercase();
    if ["windows", "macos", "linux", "android", "ios"].contains(&p.as_str()) { p } else { "other".into() }
}

pub fn normalize_user_code(raw: &str) -> Option<String> {
    let c: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    (c.len() == 8 && c.bytes().all(|b| USER_CODE_ALPHABET.contains(&b))).then(|| format!("{}-{}", &c[..4], &c[4..]))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// JPEG, PNG, WebP or GIF by magic bytes.
pub fn sniff_image(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

fn to_wire(i: Item) -> SyncItem {
    SyncItem {
        value: if i.deleted { None } else { i.value.and_then(|v| serde_json::from_str(&v).ok()) },
        kind: i.kind,
        key: i.key,
        updated_at: i.updated_at,
        deleted: i.deleted,
        rev: i.rev,
    }
}

fn account_of(u: &User) -> Account {
    Account { id: u.id.clone(), handle: u.handle.clone(), device_limit: u.device_limit, created_at: u.created_at, has_password: u.pw_hash.is_some() }
}

/// Server-side hash of a client-derived password key (already Argon2id output).
fn key_hash(key: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"glyph-pw-v1:");
    h.update(key);
    hex(&h.finalize())
}

fn decode_exact(s: &str, len: usize, what: &str) -> Result<Vec<u8>, ApiErr> {
    unb64(s).filter(|b| b.len() == len).ok_or_else(|| ApiErr::bad_request(format!("{what} must be {len} bytes base64url")))
}

fn wire_device(d: &Device, current: &str) -> glyph_protocol::Device {
    glyph_protocol::Device {
        id: d.id.clone(),
        name: d.name.clone(),
        platform: d.platform.clone(),
        created_at: d.created_at,
        last_seen: d.last_seen,
        current: d.id == current,
    }
}

fn image(status: u16, bytes: Vec<u8>, ct: &str, cache: &str) -> Response {
    Response {
        status,
        headers: vec![
            ("content-type".into(), ct.into()),
            ("cache-control".into(), cache.into()),
            ("x-content-type-options".into(), "nosniff".into()),
            ("content-security-policy".into(), "default-src 'none'; sandbox".into()),
        ],
        body: Body::Bytes(bytes),
    }
}

/// The signed-in device behind a request.
struct Authed {
    user: User,
    device: Device,
}

// Bodies used only by the login page / mobile passkey flow.
#[derive(Deserialize)]
struct RegisterOptionsReq {
    login_id: String,
    /// Ignored when the login adds a passkey to an existing account.
    #[serde(default)]
    handle: String,
}
#[derive(Deserialize)]
struct LoginIdReq {
    login_id: String,
}
#[derive(Deserialize)]
struct RegisterVerifyReq {
    login_id: String,
    client_data_json: String,
    attestation_object: String,
}
#[derive(Deserialize)]
struct AssertVerifyReq {
    login_id: String,
    id: String,
    client_data_json: String,
    authenticator_data: String,
    signature: String,
    #[serde(default)]
    user_handle: Option<String>,
}
#[derive(Deserialize)]
struct RecoverReq {
    login_id: String,
    handle: String,
    recovery_code: String,
}
/// Password sign-in / sign-up from the login page (completes that login).
#[derive(Deserialize)]
struct PagePasswordReq {
    login_id: String,
    handle: String,
    key: String,
    /// Only for sign-up.
    #[serde(default)]
    salt: Option<String>,
}
#[derive(Deserialize)]
struct PageReq {
    login_id: String,
    page_secret: String,
    #[serde(default)]
    device_id: Option<String>,
}

/// What the login page shows after a successful passkey / recovery step.
#[derive(Serialize)]
struct PageResult {
    handle: String,
    page_secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_code: Option<String>,
    devices: Vec<glyph_protocol::Device>,
    device_limit: u8,
    user_code: String,
}

impl Accounts {
    fn rp_id(&self) -> &str {
        let host = self.origin.split("://").nth(1).unwrap_or(&self.origin).split(['/', ':']).next().unwrap_or("");
        match self.rp_id.as_deref() {
            Some(rp) if !rp.is_empty() && (host == rp || host.ends_with(&format!(".{rp}"))) => rp,
            _ => host,
        }
    }

    fn origins(&self) -> Vec<String> {
        let mut v = vec![self.origin.trim_end_matches('/').to_string()];
        v.extend(self.extra_origins.iter().cloned());
        v
    }

    fn random(&self, n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        self.entropy.fill(&mut b);
        b
    }

    fn token(&self, prefix: &str) -> String {
        format!("{prefix}{}", b64(&self.random(32)))
    }

    fn user_code(&self) -> String {
        let r = self.random(8);
        let c: String = r.iter().map(|b| USER_CODE_ALPHABET[(*b & 31) as usize] as char).collect();
        format!("{}-{}", &c[..4], &c[4..])
    }

    fn recovery_code(&self) -> String {
        let r = self.random(15);
        let c: String = r.iter().map(|b| USER_CODE_ALPHABET[(*b & 31) as usize] as char).collect();
        format!("glyph-rc-{}-{}-{}", &c[..5], &c[5..10], &c[10..])
    }

    async fn limit(&self, svc: &Services, key: &str, n: u32, window_s: u64) -> Result<(), ApiErr> {
        match svc.limiter.check(key, n, window_s).await {
            RateDecision::Allowed => Ok(()),
            RateDecision::Limited { retry_after_s } => Err(ApiErr::rate_limited(retry_after_s)),
        }
    }

    async fn auth(&self, svc: &Services, req: &Request) -> Result<Authed, ApiErr> {
        let token = req.header("authorization").and_then(|h| h.strip_prefix("Bearer ")).ok_or_else(unauthorized)?;
        let now = svc.rt.now_ms();
        let device = self.store.device_by_access(&hash(token.trim())).await.map_err(store_err)?.ok_or_else(unauthorized)?;
        if device.access_exp <= now {
            return Err(ApiErr::new(401, "token_expired", "refresh the access token"));
        }
        let user = self.store.user(&device.user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
        if now.saturating_sub(device.last_seen) > TOUCH_EVERY {
            self.store.touch_device(&device.id, now).await.map_err(store_err)?;
        }
        Ok(Authed { user, device })
    }

    /// A pending, unexpired login.
    async fn pending(&self, svc: &Services, id: &str) -> Result<Login, ApiErr> {
        let l = self.store.login(id).await.map_err(store_err)?.ok_or_else(|| ApiErr::new(404, "login_expired", "start the sign-in again"))?;
        if l.expires <= svc.rt.now_ms() {
            self.store.delete_login(id).await.map_err(store_err)?;
            return Err(ApiErr::new(404, "login_expired", "start the sign-in again"));
        }
        Ok(l)
    }

    /// Mark the login as signed in for `user` and hand the page its secret.
    async fn approve_on_page(&self, mut login: Login, user: &User, recovery_code: Option<String>) -> Result<Response, ApiErr> {
        let page_secret = self.token("gp_");
        login.user_id = Some(user.id.clone());
        login.challenge = None;
        login.page_hash = Some(hash(&page_secret));
        self.store.put_login(&login).await.map_err(store_err)?;
        let devices = self.store.devices(&user.id).await.map_err(store_err)?;
        Ok(Response::json(
            200,
            &PageResult {
                handle: user.handle.clone(),
                page_secret,
                recovery_code,
                devices: devices.iter().map(|d| wire_device(d, "")).collect(),
                device_limit: user.device_limit,
                user_code: login.user_code,
            },
        ))
    }

    fn challenge(&self) -> String {
        b64(&self.random(32))
    }

    async fn blank_user(&self, id: String, handle: String, recovery: &str, now: u64) -> Result<User, ApiErr> {
        Ok(User {
            id,
            handle,
            created_at: now,
            device_limit: self.store.default_device_limit().await.map_err(store_err)?,
            device_limit_set: 0,
            rev: 0,
            purged_rev: 0,
            recovery_hash: hash(recovery),
            handle_changed_at: 0,
            pw_salt: None,
            pw_hash: None,
        })
    }

    /// Register a device for `user` and hand out its first tokens.
    async fn new_device(&self, user: &User, name: &str, platform: &str, now: u64) -> Result<Session, ApiErr> {
        let access = self.token("ga_");
        let refresh = self.token("gr_");
        let device = Device {
            id: format!("d_{}", hex(&self.random(8))),
            user_id: user.id.clone(),
            name: clean_name(name),
            platform: clean_platform(platform),
            created_at: now,
            last_seen: now,
            access_hash: hash(&access),
            access_exp: now + ACCESS_TTL,
            refresh_hash: hash(&refresh),
            refresh_exp: now + REFRESH_TTL,
        };
        if !self.store.add_device(&device, user.device_limit).await.map_err(store_err)? {
            return Err(ApiErr::new(409, "device_limit", "remove a device first"));
        }
        Ok(Session { access_token: access, refresh_token: refresh, access_expires_in_s: ACCESS_TTL / SEC, device_id: device.id, account: account_of(user) })
    }

    /// Where short playlist links point: the share origin, else this server.
    fn share_link(&self, svc: &Services, id: &str) -> String {
        let origin = svc.config.share_origin.as_deref().unwrap_or(&self.origin);
        format!("{}/p/{id}", origin.trim_end_matches('/'))
    }

    async fn shared(&self, id: &str) -> Result<SharedPlaylist, ApiErr> {
        let missing = || ApiErr::not_found("no such playlist link");
        if !crate::share::valid_id(id) {
            return Err(missing());
        }
        let s = self.store.share(id).await.map_err(store_err)?.ok_or_else(missing)?;
        let author = if s.kind == "album" { None } else { self.store.user(&s.owner_id).await.map_err(store_err)?.map(|u| u.handle) };
        Ok(SharedPlaylist { code: s.code, name: s.name, songs: s.songs, kind: s.kind, author })
    }

    /// `POST /v1/share`, `GET|DELETE /v1/share/<id>` and the `/p/<id>` page.
    async fn share(&self, svc: &Services, req: &Request, path: &str) -> Result<Response, ApiErr> {
        let m = req.method;
        if let Some(id) = path.strip_prefix("/p/") {
            if m != Method::Get {
                return Err(method());
            }
            return Ok(match self.shared(id).await {
                Ok(p) => crate::share::page(Some((&p, &self.share_link(svc, id)))),
                Err(_) => crate::share::page(None),
            });
        }
        if let Some(id) = path.strip_prefix("/v1/share/") {
            return match m {
                Method::Get => Ok(Response::json(200, &self.shared(id).await?)),
                Method::Delete => {
                    let a = self.auth(svc, req).await?;
                    if !self.store.delete_share(&a.user.id, id).await.map_err(store_err)? {
                        return Err(ApiErr::not_found("no such playlist link"));
                    }
                    Ok(Response::json(200, &serde_json::json!({})))
                }
                _ => Err(method()),
            };
        }
        if m != Method::Post {
            return Err(method());
        }
        let a = self.auth(svc, req).await?;
        self.limit(svc, &format!("share:{}", a.user.id), crate::share::CREATES_PER_HOUR, 3600).await?;
        let r: ShareRequest = json_body(req, crate::share::MAX_CODE_BYTES + 4096)?;
        let code = crate::share::clean_code(&r.code).ok_or_else(|| ApiErr::bad_request("code must be a glyph:playlist: code"))?;
        let mut id = crate::share::id_for(&a.user.id, &code, 8);
        if self.store.share(&id).await.map_err(store_err)?.is_some_and(|s| s.owner_id != a.user.id || s.code != code) {
            id = crate::share::id_for(&a.user.id, &code, crate::share::LONG_ID_LEN);
        }
        let share = Share {
            id: id.clone(),
            owner_id: a.user.id.clone(),
            name: crate::share::clean_name(r.name),
            songs: r.songs.unwrap_or(0),
            kind: crate::share::clean_kind(r.kind.as_deref()),
            created_at: svc.rt.now_ms(),
            code,
        };
        self.store.put_share(&share).await.map_err(store_err)?;
        Ok(Response::json(200, &ShareResponse { url: self.share_link(svc, &id), id }))
    }

    pub async fn handle(&self, svc: &Arc<Services>, req: Request) -> Response {
        match self.dispatch(svc, &req).await {
            Ok(r) => r,
            Err(e) => Response::error(e),
        }
    }

    async fn dispatch(&self, svc: &Arc<Services>, req: &Request) -> Result<Response, ApiErr> {
        let path = req.path.trim_end_matches('/');
        let m = req.method;
        if m == Method::Options {
            return Ok(Response { status: 204, headers: vec![("allow".into(), "GET, POST, PUT, PATCH, DELETE, OPTIONS".into())], body: Body::Bytes(vec![]) });
        }
        crate::server::rate_limit(svc, req).await?;
        let now = svc.rt.now_ms();
        let ip = req.client_ip.clone().unwrap_or_else(|| "unknown".into());

        if let Some(id) = path.strip_prefix("/account/login/") {
            if m != Method::Get {
                return Err(method());
            }
            let _ = id;
            return Ok(page());
        }
        if let Some(hash_hex) = path.strip_prefix("/v1/covers/") {
            if m != Method::Get {
                return Err(method());
            }
            return self.cached_cover(svc, req, hash_hex).await;
        }
        if let Some(h) = path.strip_prefix("/v1/account/handle/") {
            if m != Method::Get {
                return Err(method());
            }
            self.limit(svc, &format!("acct-handle:{ip}"), 30, 60).await?;
            let check = match normalize_handle(h) {
                Err(reason) => HandleCheck { handle: h.to_ascii_lowercase(), available: false, reason: Some(reason.into()) },
                Ok(h) => {
                    let free = self.store.handle_free(&h, now).await.map_err(store_err)?;
                    HandleCheck { handle: h, available: free, reason: (!free).then(|| "taken".into()) }
                }
            };
            return Ok(Response::json(200, &check));
        }
        if let Some(h) = path.strip_prefix("/v1/account/password/salt/") {
            if m != Method::Get {
                return Err(method());
            }
            self.limit(svc, &format!("acct-salt:{ip}"), 30, 60).await?;
            let salt = match normalize_handle(h) {
                Ok(h) => self.store.user_by_handle(&h).await.map_err(store_err)?.and_then(|u| u.pw_salt),
                Err(_) => None,
            };
            let salt = salt.ok_or_else(|| ApiErr::new(404, "no_password", "no password set for this handle"))?;
            return Ok(Response::json(200, &PasswordSalt { salt }));
        }
        if path == "/v1/profile" || path.starts_with("/v1/profile/") || path.starts_with("/v1/profiles/") {
            return self.profile_route(svc, req, path).await;
        }
        if let Some(rest) = path.strip_prefix("/v1/devices/") {
            let a = self.auth(svc, req).await?;
            if rest == "self/signout" {
                if m != Method::Post {
                    return Err(method());
                }
                self.store.delete_device(&a.user.id, &a.device.id).await.map_err(store_err)?;
                return Ok(Response::json(200, &serde_json::json!({})));
            }
            return match m {
                Method::Delete => {
                    if !self.store.delete_device(&a.user.id, rest).await.map_err(store_err)? {
                        return Err(ApiErr::not_found("no such device"));
                    }
                    Ok(Response::json(200, &serde_json::json!({})))
                }
                Method::Patch => {
                    let p: DevicePatch = json_body(req, 4096)?;
                    let mut d = self.store.devices(&a.user.id).await.map_err(store_err)?.into_iter().find(|d| d.id == rest).ok_or_else(|| ApiErr::not_found("no such device"))?;
                    d.name = clean_name(&p.name);
                    self.store.put_device(&d).await.map_err(store_err)?;
                    Ok(Response::json(200, &wire_device(&d, &a.device.id)))
                }
                _ => Err(method()),
            };
        }
        if let Some(name) = path.strip_prefix("/v1/sync/covers/") {
            return self.own_cover(svc, req, name).await;
        }
        if path == "/v1/share" || path.starts_with("/v1/share/") || path.starts_with("/p/") {
            return self.share(svc, req, path).await;
        }

        match (path, m) {
            ("/v1/account/login/start", Method::Post) => {
                self.limit(svc, &format!("acct-start:{ip}"), 10, 600).await?;
                let r: LoginStartRequest = json_body(req, 4096)?;
                let login_id = b64(&self.random(16));
                let poll_secret = self.token("gs_");
                let login = Login {
                    id: login_id.clone(),
                    poll_hash: hash(&poll_secret),
                    user_code: self.user_code(),
                    device_name: clean_name(&r.device_name),
                    platform: clean_platform(&r.platform),
                    expires: now + LOGIN_TTL,
                    ..Default::default()
                };
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &LoginStartResponse {
                        url: format!("{}/account/login/{login_id}", self.origin.trim_end_matches('/')),
                        login_id,
                        poll_secret,
                        user_code: login.user_code,
                        expires_in_s: LOGIN_TTL / SEC,
                    },
                ))
            }
            ("/v1/account/login/poll", Method::Post) => {
                let r: LoginPollRequest = json_body(req, 4096)?;
                let Some(login) = self.store.login(&r.login_id).await.map_err(store_err)? else {
                    return Ok(Response::json(200, &LoginPollResponse::Expired));
                };
                if login.poll_hash != hash(&r.poll_secret) {
                    return Err(unauthorized());
                }
                if login.expires <= now {
                    self.store.delete_login(&login.id).await.map_err(store_err)?;
                    return Ok(Response::json(200, &LoginPollResponse::Expired));
                }
                let Some(user_id) = login.user_id.clone() else {
                    return Ok(Response::json(200, &LoginPollResponse::Pending));
                };
                let Some(user) = self.store.user(&user_id).await.map_err(store_err)? else {
                    self.store.delete_login(&login.id).await.map_err(store_err)?;
                    return Ok(Response::json(200, &LoginPollResponse::Expired));
                };
                if self.store.devices(&user.id).await.map_err(store_err)?.len() >= user.device_limit as usize {
                    return Ok(Response::json(200, &LoginPollResponse::DeviceLimit));
                }
                let session = self.new_device(&user, &login.device_name, &login.platform, now).await?;
                self.store.delete_login(&login.id).await.map_err(store_err)?;
                Ok(Response::json(200, &LoginPollResponse::Done { session }))
            }
            ("/v1/account/login/approve", Method::Post) => {
                let a = self.auth(svc, req).await?;
                self.limit(svc, &format!("acct-approve:{}", a.user.id), 10, 60).await?;
                let r: ApproveRequest = json_body(req, 1024)?;
                let code = normalize_user_code(&r.user_code).ok_or_else(|| ApiErr::bad_request("invalid code"))?;
                let mut login = self.store.login_by_code(&code).await.map_err(store_err)?
                    .filter(|l| l.expires > now && l.user_id.is_none())
                    .ok_or_else(|| ApiErr::not_found("unknown or expired code"))?;
                login.user_id = Some(a.user.id.clone());
                login.challenge = None;
                self.store.put_login(&login).await.map_err(store_err)?;
                let n = self.store.devices(&a.user.id).await.map_err(store_err)?.len();
                Ok(Response::json(
                    200,
                    &ApproveResponse { device_name: login.device_name, platform: login.platform, device_limit_reached: n >= a.user.device_limit as usize },
                ))
            }
            ("/v1/account/login/devices", Method::Post) | ("/v1/account/login/remove_device", Method::Post) => {
                let r: PageReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                let (Some(user_id), Some(ph)) = (login.user_id.as_deref(), login.page_hash.as_deref()) else { return Err(unauthorized()) };
                if ph != hash(&r.page_secret) {
                    return Err(unauthorized());
                }
                if let Some(id) = r.device_id.as_deref().filter(|_| path.ends_with("remove_device")) {
                    self.store.delete_device(user_id, id).await.map_err(store_err)?;
                }
                let user = self.store.user(user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                let devices = self.store.devices(user_id).await.map_err(store_err)?;
                Ok(Response::json(200, &DevicesResponse { devices: devices.iter().map(|d| wire_device(d, "")).collect(), device_limit: user.device_limit }))
            }
            ("/v1/account/passkey/register/options", Method::Post) => {
                self.limit(svc, &format!("acct-wa:{ip}"), 20, 600).await?;
                let r: RegisterOptionsReq = json_body(req, 4096)?;
                let mut login = self.pending(svc, &r.login_id).await?;
                let (handle, user_id) = if login.link {
                    (login.pending_handle.clone().unwrap_or_default(), login.pending_user_id.clone().unwrap_or_default())
                } else {
                    let handle = normalize_handle(&r.handle).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                    if !self.store.handle_free(&handle, now).await.map_err(store_err)? {
                        return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                    }
                    (handle, format!("u_{}", hex(&self.random(12))))
                };
                let challenge = self.challenge();
                login.challenge = Some(challenge.clone());
                login.pending_handle = Some(handle.clone());
                login.pending_user_id = Some(user_id.clone());
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "publicKey": {
                    "challenge": challenge,
                    "rp": { "id": self.rp_id(), "name": "Glyph" },
                    "user": { "id": b64(user_id.as_bytes()), "name": format!("@{handle}"), "displayName": format!("@{handle}") },
                    "pubKeyCredParams": [{ "type": "public-key", "alg": webauthn::ES256 }, { "type": "public-key", "alg": webauthn::RS256 }],
                    "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "required" },
                    "attestation": "none",
                    "timeout": 120000
                }})))
            }
            ("/v1/account/passkey/register/verify", Method::Post) => {
                let r: RegisterVerifyReq = json_body(req, 64 * 1024)?;
                let login = self.pending(svc, &r.login_id).await?;
                let (Some(ch), Some(handle), Some(user_id)) = (login.challenge.clone(), login.pending_handle.clone(), login.pending_user_id.clone()) else {
                    return Err(ApiErr::bad_request("request options first"));
                };
                let cd = unb64(&r.client_data_json).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let att = unb64(&r.attestation_object).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let origins = self.origins();
                let cred = webauthn::verify_registration(&Expect { rp_id: self.rp_id(), origins: &origins, challenge: &ch }, &cd, &att)
                    .map_err(|e| ApiErr::new(400, "passkey_invalid", e.to_string()))?;
                let pk = Passkey { cred_id: cred.cred_id, user_id: user_id.clone(), alg: cred.alg, public_key: cred.public_key, sign_count: cred.sign_count, created_at: now };
                if self.store.passkey(&pk.cred_id).await.map_err(store_err)?.is_some() {
                    return Err(ApiErr::new(409, "passkey_exists", "this passkey is already registered"));
                }
                if login.link {
                    // Adding a passkey to a signed-in account: no new device, no new session.
                    self.store.user(&user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                    self.store.add_passkey(&pk).await.map_err(store_err)?;
                    self.store.delete_login(&login.id).await.map_err(store_err)?;
                    return Ok(Response::json(200, &serde_json::json!({ "handle": handle, "linked": true })));
                }
                let recovery = self.recovery_code();
                let user = self.blank_user(user_id, handle, &recovery, now).await?;
                if !self.store.create_user(&user, Some(&pk), now).await.map_err(store_err)? {
                    return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                }
                self.approve_on_page(login, &user, Some(recovery)).await
            }
            ("/v1/account/passkey/login/options", Method::Post) => {
                self.limit(svc, &format!("acct-wa:{ip}"), 20, 600).await?;
                let r: LoginIdReq = json_body(req, 4096)?;
                let mut login = self.pending(svc, &r.login_id).await?;
                let challenge = self.challenge();
                login.challenge = Some(challenge.clone());
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "publicKey": {
                    "challenge": challenge,
                    "rpId": self.rp_id(),
                    "allowCredentials": [],
                    "userVerification": "required",
                    "timeout": 120000
                }})))
            }
            ("/v1/account/passkey/login/verify", Method::Post) => {
                let r: AssertVerifyReq = json_body(req, 64 * 1024)?;
                let mut login = self.pending(svc, &r.login_id).await?;
                let Some(ch) = login.challenge.take() else { return Err(ApiErr::bad_request("request options first")) };
                // One attempt per challenge.
                self.store.put_login(&login).await.map_err(store_err)?;
                let bad = || ApiErr::new(400, "passkey_invalid", "unknown passkey");
                let pk = self.store.passkey(r.id.trim_end_matches('=')).await.map_err(store_err)?.ok_or_else(bad)?;
                if let Some(uh) = r.user_handle.as_deref().filter(|s| !s.is_empty()) {
                    if unb64(uh).as_deref() != Some(pk.user_id.as_bytes()) {
                        return Err(bad());
                    }
                }
                let cd = unb64(&r.client_data_json).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let ad = unb64(&r.authenticator_data).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let sig = unb64(&r.signature).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let origins = self.origins();
                let count = webauthn::verify_assertion(&Expect { rp_id: self.rp_id(), origins: &origins, challenge: &ch }, pk.alg, &pk.public_key, pk.sign_count, &cd, &ad, &sig)
                    .map_err(|e| ApiErr::new(400, "passkey_invalid", e.to_string()))?;
                self.store.set_sign_count(&pk.cred_id, count).await.map_err(store_err)?;
                let user = self.store.user(&pk.user_id).await.map_err(store_err)?.ok_or_else(bad)?;
                self.approve_on_page(login, &user, None).await
            }
            ("/v1/account/recover", Method::Post) => {
                self.limit(svc, &format!("acct-recover:{ip}"), 5, 600).await?;
                let r: RecoverReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                let handle = normalize_handle(&r.handle).map_err(|_| ApiErr::new(401, "recovery_failed", "handle or code wrong"))?;
                self.limit(svc, &format!("acct-recover-h:{handle}"), 5, 3600).await?;
                let user = self.store.user_by_handle(&handle).await.map_err(store_err)?
                    .filter(|u| u.recovery_hash == hash(r.recovery_code.trim()))
                    .ok_or_else(|| ApiErr::new(401, "recovery_failed", "handle or code wrong"))?;
                // A used code is replaced right away.
                let fresh = self.recovery_code();
                self.store.set_recovery(&user.id, &hash(&fresh)).await.map_err(store_err)?;
                self.approve_on_page(login, &user, Some(fresh)).await
            }
            ("/v1/account/login/info", Method::Post) => {
                let r: LoginIdReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                Ok(Response::json(200, &serde_json::json!({
                    "link": login.link,
                    "handle": if login.link { login.pending_handle.clone() } else { None },
                    "user_code": if login.link { None } else { Some(login.user_code.clone()) },
                })))
            }
            ("/v1/account/passkey/link", Method::Post) => {
                let a = self.auth(svc, req).await?;
                self.limit(svc, &format!("acct-link:{}", a.user.id), 10, 600).await?;
                let login_id = b64(&self.random(16));
                let login = Login {
                    id: login_id.clone(),
                    // Nobody polls a link login: an unguessable, unused poll hash.
                    poll_hash: hash(&self.token("gs_")),
                    user_code: self.user_code(),
                    device_name: a.device.name.clone(),
                    platform: a.device.platform.clone(),
                    pending_handle: Some(a.user.handle.clone()),
                    pending_user_id: Some(a.user.id.clone()),
                    link: true,
                    expires: now + LOGIN_TTL,
                    ..Default::default()
                };
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &LinkStartResponse { url: format!("{}/account/login/{login_id}", self.origin.trim_end_matches('/')), expires_in_s: LOGIN_TTL / SEC },
                ))
            }
            ("/v1/account/login/password", Method::Post) | ("/v1/account/login/password_register", Method::Post) => {
                let r: PagePasswordReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                if login.link || login.user_id.is_some() {
                    return Err(ApiErr::bad_request("this sign-in is already finished"));
                }
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                if path.ends_with("password_register") {
                    self.limit(svc, &format!("acct-signup:{ip}"), 5, 3600).await?;
                    let handle = normalize_handle(&r.handle).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                    let salt = decode_exact(r.salt.as_deref().unwrap_or_default(), glyph_protocol::password_kdf::SALT_LEN, "salt")?;
                    let recovery = self.recovery_code();
                    let mut user = self.blank_user(format!("u_{}", hex(&self.random(12))), handle, &recovery, now).await?;
                    user.pw_salt = Some(b64(&salt));
                    user.pw_hash = Some(key_hash(&key));
                    if !self.store.create_user(&user, None, now).await.map_err(store_err)? {
                        return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                    }
                    return self.approve_on_page(login, &user, Some(recovery)).await;
                }
                self.limit(svc, &format!("acct-pw:{ip}"), 20, 600).await?;
                let failed = || ApiErr::new(401, "login_failed", "handle or password wrong");
                let handle = normalize_handle(&r.handle).map_err(|_| failed())?;
                self.limit(svc, &format!("acct-pw-h:{handle}"), 10, 3600).await?;
                let user = self.store.user_by_handle(&handle).await.map_err(store_err)?
                    .filter(|u| u.pw_hash.as_deref() == Some(key_hash(&key).as_str()))
                    .ok_or_else(failed)?;
                self.approve_on_page(login, &user, None).await
            }
            ("/v1/account/password/register", Method::Post) => {
                self.limit(svc, &format!("acct-signup:{ip}"), 5, 3600).await?;
                let r: PasswordRegisterRequest = json_body(req, 4096)?;
                let handle = normalize_handle(&r.handle).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                let salt = decode_exact(&r.salt, glyph_protocol::password_kdf::SALT_LEN, "salt")?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                let recovery = self.recovery_code();
                let mut user = self.blank_user(format!("u_{}", hex(&self.random(12))), handle, &recovery, now).await?;
                user.pw_salt = Some(b64(&salt));
                user.pw_hash = Some(key_hash(&key));
                if !self.store.create_user(&user, None, now).await.map_err(store_err)? {
                    return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                }
                let session = self.new_device(&user, &r.device_name, &r.platform, now).await?;
                Ok(Response::json(200, &SignupResponse { session, recovery_code: recovery }))
            }
            ("/v1/account/password/login", Method::Post) => {
                self.limit(svc, &format!("acct-pw:{ip}"), 20, 600).await?;
                let r: PasswordLoginRequest = json_body(req, 4096)?;
                let failed = || ApiErr::new(401, "login_failed", "handle or password wrong");
                let handle = normalize_handle(&r.handle).map_err(|_| failed())?;
                self.limit(svc, &format!("acct-pw-h:{handle}"), 10, 3600).await?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                let user = self.store.user_by_handle(&handle).await.map_err(store_err)?
                    .filter(|u| u.pw_hash.as_deref() == Some(key_hash(&key).as_str()))
                    .ok_or_else(failed)?;
                if self.store.devices(&user.id).await.map_err(store_err)?.len() >= user.device_limit as usize {
                    return Err(ApiErr::new(409, "device_limit", "remove a device first"));
                }
                let session = self.new_device(&user, &r.device_name, &r.platform, now).await?;
                Ok(Response::json(200, &session))
            }
            // A device signed in without the password (passkey, browser) checks
            // it before deriving the key for the encrypted settings with it.
            ("/v1/account/password/check", Method::Post) => {
                let a = self.auth(svc, req).await?;
                self.limit(svc, &format!("acct-pw-h:{}", a.user.handle), 10, 3600).await?;
                let r: glyph_protocol::PasswordCheckRequest = json_body(req, 4096)?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                if a.user.pw_hash.as_deref() != Some(key_hash(&key).as_str()) {
                    // 403, not 401: the session itself is fine.
                    return Err(ApiErr::new(403, "wrong_password", "the password is wrong"));
                }
                Ok(Response::json(200, &serde_json::json!({ "ok": true })))
            }
            ("/v1/account/password", Method::Put) => {
                let a = self.auth(svc, req).await?;
                let r: PasswordSetRequest = json_body(req, 4096)?;
                let salt = decode_exact(&r.salt, glyph_protocol::password_kdf::SALT_LEN, "salt")?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                self.store.set_password(&a.user.id, &b64(&salt), &key_hash(&key)).await.map_err(store_err)?;
                let user = self.store.user(&a.user.id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                Ok(Response::json(200, &account_of(&user)))
            }
            ("/v1/account/token/refresh", Method::Post) => {
                self.limit(svc, &format!("acct-refresh:{ip}"), 30, 60).await?;
                let r: RefreshRequest = json_body(req, 4096)?;
                let mut d = self.store.device_by_refresh(&hash(r.refresh_token.trim())).await.map_err(store_err)?
                    .filter(|d| d.refresh_exp > now)
                    .ok_or_else(unauthorized)?;
                let user = self.store.user(&d.user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                let access = self.token("ga_");
                let refresh = self.token("gr_");
                d.access_hash = hash(&access);
                d.access_exp = now + ACCESS_TTL;
                d.refresh_hash = hash(&refresh);
                d.refresh_exp = now + REFRESH_TTL;
                d.last_seen = now;
                self.store.put_device(&d).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &Session { access_token: access, refresh_token: refresh, access_expires_in_s: ACCESS_TTL / SEC, device_id: d.id, account: account_of(&user) },
                ))
            }
            ("/v1/account/recovery_code", Method::Post) => {
                let a = self.auth(svc, req).await?;
                let fresh = self.recovery_code();
                self.store.set_recovery(&a.user.id, &hash(&fresh)).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "recovery_code": fresh })))
            }
            ("/v1/account", Method::Get) => {
                let a = self.auth(svc, req).await?;
                Ok(Response::json(200, &account_of(&a.user)))
            }
            ("/v1/account", Method::Patch) => {
                let a = self.auth(svc, req).await?;
                let p: AccountPatch = json_body(req, 4096)?;
                // The client is open source and untrusted: limits are server settings.
                if p.device_limit.is_some() {
                    return Err(ApiErr::new(403, "server_setting", "the device limit is set by the server operator"));
                }
                if let Some(h) = p.handle.as_deref() {
                    let h = normalize_handle(h).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                    if h != a.user.handle {
                        if a.user.handle_changed_at != 0 && now < a.user.handle_changed_at + HANDLE_COOLDOWN {
                            return Err(ApiErr::new(409, "handle_cooldown", "the handle can change once every 30 days"));
                        }
                        if !self.store.change_handle(&a.user.id, &h, now, now + HANDLE_HOLD).await.map_err(store_err)? {
                            return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                        }
                    }
                }
                let user = self.store.user(&a.user.id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                Ok(Response::json(200, &account_of(&user)))
            }
            ("/v1/account", Method::Delete) => {
                let a = self.auth(svc, req).await?;
                let r: DeleteAccountRequest = json_body(req, 1024)?;
                if r.confirm_handle.trim().trim_start_matches('@').to_ascii_lowercase() != a.user.handle {
                    return Err(ApiErr::bad_request("confirm_handle does not match"));
                }
                if let Some(b) = &self.blobs {
                    b.delete_prefix(&format!("users/{}/", a.user.id)).await.map_err(store_err)?;
                }
                self.store.delete_user(&a.user.id, now + HANDLE_HOLD).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({})))
            }
            ("/v1/devices", Method::Get) => {
                let a = self.auth(svc, req).await?;
                let devices = self.store.devices(&a.user.id).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &DevicesResponse { devices: devices.iter().map(|d| wire_device(d, &a.device.id)).collect(), device_limit: a.user.device_limit },
                ))
            }
            ("/v1/sync/push", Method::Post) => {
                let a = self.auth(svc, req).await?;
                let r: SyncPushRequest = json_body(req, MAX_PUSH_BYTES)?;
                if r.items.len() > MAX_PUSH_ITEMS {
                    return Err(ApiErr::new(413, "payload_too_large", "at most 500 items per push"));
                }
                let mut items = Vec::with_capacity(r.items.len());
                for i in r.items {
                    let kind_ok = (1..=24).contains(&i.kind.len()) && i.kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
                    if !kind_ok || i.key.is_empty() || i.key.len() > 512 || i.deleted == i.value.is_some() {
                        return Err(ApiErr::bad_request("invalid sync item"));
                    }
                    let value = match i.value {
                        Some(v) => {
                            let s = v.to_string();
                            if s.len() > MAX_ITEM_BYTES {
                                return Err(ApiErr::new(413, "payload_too_large", "sync item too large"));
                            }
                            Some(s)
                        }
                        None => None,
                    };
                    items.push(Item {
                        kind: i.kind,
                        key: i.key,
                        value,
                        updated_at: i.updated_at.min(now + CLOCK_SKEW),
                        device_id: a.device.id.clone(),
                        deleted: i.deleted,
                        rev: 0,
                    });
                }
                self.store.purge_tombstones(&a.user.id, now.saturating_sub(TOMBSTONE_TTL)).await.map_err(store_err)?;
                let (accepted, rev) = self.store.push_items(&a.user.id, &items).await.map_err(store_err)?;
                let lost: Vec<(String, String)> = items.iter().zip(&accepted).filter(|(_, ok)| !**ok).map(|(i, _)| (i.kind.clone(), i.key.clone())).collect();
                let stale = if lost.is_empty() { vec![] } else { self.store.items(&a.user.id, &lost).await.map_err(store_err)? };
                Ok(Response::json(
                    200,
                    &SyncPushResponse { rev, accepted: accepted.iter().filter(|a| **a).count() as u32, stale: stale.into_iter().map(to_wire).collect() },
                ))
            }
            ("/v1/sync/pull", Method::Get) => {
                let a = self.auth(svc, req).await?;
                let since: u64 = match req.query.get("since") {
                    Some(s) => s.parse().map_err(|_| ApiErr::bad_request("since must be a number"))?,
                    None => 0,
                };
                let limit: u32 = match req.query.get("limit") {
                    Some(s) => s.parse().map_err(|_| ApiErr::bad_request("limit must be a number"))?,
                    None => 500,
                }
                .clamp(1, 1000);
                if since > 0 && since < a.user.purged_rev {
                    return Ok(Response::json(200, &SyncPullResponse { reset: true, ..Default::default() }));
                }
                let mut items = self.store.items_since(&a.user.id, since, limit + 1).await.map_err(store_err)?;
                let more = items.len() > limit as usize;
                items.truncate(limit as usize);
                let last = items.last().map_or(since, |i| i.rev);
                let rev = if more { last } else { a.user.rev.max(last) };
                Ok(Response::json(200, &SyncPullResponse { items: items.into_iter().map(to_wire).collect(), rev, more, reset: false }))
            }
            ("/v1/sync/export", Method::Get) => {
                let a = self.auth(svc, req).await?;
                let items = self.store.items_since(&a.user.id, 0, 1_000_000).await.map_err(store_err)?;
                let live: Vec<SyncItem> = items.into_iter().filter(|i| !i.deleted).map(to_wire).collect();
                let mut r = Response::json(200, &serde_json::json!({ "account": account_of(&a.user), "items": live }));
                r.headers.push(("content-disposition".into(), "attachment; filename=\"glyph-export.json\"".into()));
                Ok(r)
            }
            (p, _) if matches!(p, "/v1/account" | "/v1/devices" | "/v1/sync/push" | "/v1/sync/pull" | "/v1/sync/export" | "/v1/account/recovery_code")
                || p.starts_with("/v1/account/login/") || p.starts_with("/v1/account/passkey/") || p.starts_with("/v1/account/password")
                || p == "/v1/account/recover" || p == "/v1/account/token/refresh" =>
            {
                Err(method())
            }
            _ => Err(ApiErr::not_found("unknown endpoint")),
        }
    }

    /// `GET /v1/covers/{sha256(url)}?u={url}`: fetch once, keep in R2.
    async fn cached_cover(&self, svc: &Arc<Services>, req: &Request, name: &str) -> Result<Response, ApiErr> {
        let Some(blobs) = &self.blobs else { return Err(ApiErr::not_found("cover cache disabled")) };
        let url = req.query.required("u")?;
        if !is_sha256_hex(name) || hash(url) != name || !glyph_protocol::cover_cache_allowed(url) {
            return Err(ApiErr::bad_request("cover url not allowed"));
        }
        const CACHE: &str = "public, max-age=31536000, immutable";
        let key = format!("covers/{name}");
        if let Some((bytes, ct)) = blobs.get(&key).await.map_err(store_err)? {
            return Ok(image(200, bytes, &ct, CACHE));
        }
        let fetched = crate::traits::with_timeout(&*svc.rt, svc.config.provider_timeout_ms.max(5000), svc.http.send(crate::HttpRequest::get(url))).await;
        let Some(Ok(resp)) = fetched else { return Err(ApiErr::new(502, "upstream_error", "cover fetch failed")) };
        if resp.status != 200 || resp.body.len() > MAX_CACHED_COVER {
            return Err(ApiErr::new(502, "upstream_error", "cover fetch failed"));
        }
        let ct = sniff_image(&resp.body).ok_or_else(|| ApiErr::new(502, "upstream_error", "not an image"))?;
        blobs.put(&key, resp.body.clone(), ct).await.map_err(store_err)?;
        Ok(image(200, resp.body, ct, CACHE))
    }

    /// `PUT|GET /v1/sync/covers/{sha256}`: the user's own playlist pictures.
    async fn own_cover(&self, svc: &Arc<Services>, req: &Request, name: &str) -> Result<Response, ApiErr> {
        let Some(blobs) = &self.blobs else { return Err(ApiErr::not_found("cover storage disabled")) };
        let a = self.auth(svc, req).await?;
        if !is_sha256_hex(name) {
            return Err(ApiErr::bad_request("name must be the sha256 of the image"));
        }
        let key = format!("users/{}/covers/{name}", a.user.id);
        match req.method {
            Method::Put => {
                if req.body.len() > MAX_OWN_COVER {
                    return Err(ApiErr::new(413, "payload_too_large", "cover larger than 512 KB"));
                }
                if hex(&Sha256::digest(&req.body)) != name {
                    return Err(ApiErr::bad_request("sha256 does not match"));
                }
                let ct = sniff_image(&req.body).ok_or_else(|| ApiErr::bad_request("JPEG, PNG, WebP or GIF only"))?;
                blobs.put(&key, req.body.clone(), ct).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({})))
            }
            Method::Get => {
                let (bytes, ct) = blobs.get(&key).await.map_err(store_err)?.ok_or_else(|| ApiErr::not_found("no such cover"))?;
                Ok(image(200, bytes, &ct, "private, max-age=31536000, immutable"))
            }
            _ => Err(method()),
        }
    }
}

fn method() -> ApiErr {
    ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint")
}

fn page() -> Response {
    Response {
        status: 200,
        headers: vec![
            ("content-type".into(), "text/html; charset=utf-8".into()),
            ("cache-control".into(), "no-store".into()),
            ("x-content-type-options".into(), "nosniff".into()),
            ("x-frame-options".into(), "DENY".into()),
            ("referrer-policy".into(), "no-referrer".into()),
            (
                "content-security-policy".into(),
                "default-src 'none'; script-src 'unsafe-inline' 'wasm-unsafe-eval' https://cdn.jsdelivr.net; style-src 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".into(),
            ),
        ],
        body: Body::Bytes(include_str!("page.html").as_bytes().to_vec()),
    }
}

/// Highest device limit an operator can set.
pub const MAX_DEVICE_LIMIT: u8 = 100;

/// Operator side (admin API): device limits live only on the server.
impl Accounts {
    fn check_limit(limit: u8) -> Result<u8, ApiErr> {
        if (1..=MAX_DEVICE_LIMIT).contains(&limit) {
            Ok(limit)
        } else {
            Err(ApiErr::bad_request(format!("device_limit must be between 1 and {MAX_DEVICE_LIMIT}")))
        }
    }

    pub async fn admin_defaults(&self) -> Result<crate::admin::types::AccountDefaults, ApiErr> {
        Ok(crate::admin::types::AccountDefaults { device_limit: self.store.default_device_limit().await.map_err(store_err)? })
    }

    pub async fn admin_set_defaults(&self, d: &crate::admin::types::AccountDefaults) -> Result<crate::admin::types::AccountDefaults, ApiErr> {
        self.store.set_default_device_limit(Self::check_limit(d.device_limit)?).await.map_err(store_err)?;
        self.admin_defaults().await
    }

    pub async fn admin_account(&self, handle: &str) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        use crate::admin::types::{AdminAccount, AdminDevice};
        let handle = normalize_handle(handle).map_err(|_| ApiErr::not_found("no such account"))?;
        let u = self.store.user_by_handle(&handle).await.map_err(store_err)?.ok_or_else(|| ApiErr::not_found("no such account"))?;
        let devices = self.store.devices(&u.id).await.map_err(store_err)?;
        Ok(AdminAccount {
            id: u.id,
            handle: u.handle,
            created_at: u.created_at,
            device_limit: u.device_limit,
            device_limit_set: (u.device_limit_set > 0).then_some(u.device_limit_set),
            devices: devices.into_iter().map(|d| AdminDevice { name: d.name, platform: d.platform, last_seen: d.last_seen }).collect(),
        })
    }

    /// Existing devices beyond a lowered limit stay signed in; no new ones until below it.
    pub async fn admin_set_limit(&self, handle: &str, limit: Option<u8>) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        let id = self.admin_account(handle).await?.id;
        let raw = match limit {
            Some(l) => Self::check_limit(l)?,
            None => 0,
        };
        self.store.set_device_limit(&id, raw).await.map_err(store_err)?;
        self.admin_account(handle).await
    }
}
