//! Optional accounts: @handles, passkey / pairing-code sign-in, per-device
//! tokens with a device limit, last-writer-wins library sync and a cover cache.
//! Design: `docs/ACCOUNTS.md` in the app repo.
//!
//! Storage is behind [`store::AccountStore`] / [`store::BlobStore`]; randomness
//! behind [`Entropy`] (this crate has no RNG of its own). Hosts enable accounts
//! with [`crate::Server::with_accounts`]; without it every route here is a 404.

pub mod jam;
pub mod mcp;
pub mod profile;
pub mod playback;
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

mod routes;
mod sharing;
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
    /// Jam pushes over WebSockets (the Worker's Durable Object); `None`:
    /// the apps long poll.
    pub push: Option<Arc<dyn jam::JamPush>>,
}

/// Largest request body a route accepts (hosts cap uploads with this).
pub fn max_body_bytes(path: &str) -> Option<usize> {
    if path.starts_with("/v1/devices/playback/") {
        Some(256 * 1024)
    } else if path == "/v1/sync/push" {
        Some(MAX_PUSH_BYTES)
    } else if let Some(n) = crate::share::max_body_bytes(path) {
        Some(n)
    } else if let Some(n) = jam::max_body_bytes(path) {
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
    mcp::owns(path)
        || jam::owns(path)
        || ["/v1/account", "/v1/devices", "/v1/sync", "/v1/covers/", "/account/", "/v1/share", "/p/", "/embed/p/", "/oembed", "/v1/profile", "/v1/profiles/"]
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
        approximate_location: d.approximate_location.clone(),
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
        if now.saturating_sub(device.last_seen) > TOUCH_EVERY || device.approximate_location.is_none() && req.approximate_location.is_some() {
            self.store.touch_device(&device.id, now, req.approximate_location.as_deref()).await.map_err(store_err)?;
        }
        Ok(Authed { user, device })
    }

    /// Only a live server-side session grants higher quotas or priority.
    pub(crate) async fn abuse_identity(&self, svc: &Services, req: &Request) -> Option<(String, Device)> {
        let mut headers = req.headers.iter().filter(|(key,_)| key.eq_ignore_ascii_case("authorization"));
        let token = headers.next()?.1.strip_prefix("Bearer ")?.trim();
        if headers.next().is_some() { return None; }
        if token.len() > 128 { return None; }
        let device = self.store.device_by_access(&hash(token)).await.ok()??;
        if device.access_exp <= svc.rt.now_ms() { return None; }
        let user = self.store.user(&device.user_id).await.ok()??;
        Some((user.id, device))
    }

    /// Keeps the device's ban hashes current; writes only when one changed.
    pub(crate) async fn mark(&self, svc: &Services, device: &Device, req: &Request) {
        let ip = req.client_ip.as_deref().and_then(|ip| crate::bans::ip_hash(&svc.config.bans.salt, ip));
        let hw = req.header(glyph_protocol::HARDWARE_ID_HEADER).and_then(crate::bans::hardware_hash);
        let ip = ip.filter(|h| device.ip_hash.as_deref() != Some(h.as_str()));
        let hw = hw.filter(|h| device.hw_hash.as_deref() != Some(h.as_str()));
        if ip.is_some() || hw.is_some() {
            let _ = self.store.mark_device(&device.id, ip.as_deref(), hw.as_deref()).await;
        }
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
            approximate_location: None,
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
            ip_hash: None,
            hw_hash: None,
        };
        if !self.store.add_device(&device, user.device_limit).await.map_err(store_err)? {
            return Err(ApiErr::new(409, "device_limit", "remove a device first"));
        }
        Ok(Session { access_token: access, refresh_token: refresh, access_expires_in_s: ACCESS_TTL / SEC, device_id: device.id, account: account_of(user) })
    }

    pub async fn handle(&self, svc: &Arc<Services>, req: Request) -> Response {
        match self.dispatch(svc, &req).await {
            Ok(r) => r,
            Err(e) => Response::error(e),
        }
    }

}

/// A 302 to `to` (a provider URL or a path on this server).
fn redirect(to: &str, cache: &str) -> Response {
    Response {
        status: 302,
        headers: vec![("location".into(), to.into()), ("cache-control".into(), cache.into()), ("referrer-policy".into(), "no-referrer".into())],
        body: Body::Bytes(vec![]),
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
            devices: devices.into_iter().map(|d| AdminDevice { id: d.id, name: d.name, platform: d.platform, last_seen: d.last_seen, hardware: d.hw_hash, ip_known: d.ip_hash.is_some() }).collect(),
        })
    }

    /// The account and its devices, for bans derived from them.
    pub async fn admin_devices(&self, handle: &str) -> Result<(User, Vec<Device>), ApiErr> {
        let handle = normalize_handle(handle).map_err(|_| ApiErr::not_found("no such account"))?;
        let u = self.store.user_by_handle(&handle).await.map_err(store_err)?.ok_or_else(|| ApiErr::not_found("no such account"))?;
        let devices = self.store.devices(&u.id).await.map_err(store_err)?;
        Ok((u, devices))
    }

    /// Signs out one device (or all with `None`); they can sign in again
    /// unless banned.
    pub async fn admin_revoke(&self, handle: &str, device: Option<&str>) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        let (u, devices) = self.admin_devices(handle).await?;
        let mut removed = 0;
        for d in devices.iter().filter(|d| device.is_none_or(|id| id == d.id)) {
            if self.store.delete_device(&u.id, &d.id).await.map_err(store_err)? { removed += 1; }
        }
        if removed == 0 && device.is_some() {
            return Err(ApiErr::not_found("no such device on this account (see glyph remote accounts show)"));
        }
        self.admin_account(handle).await
    }

    pub async fn admin_stats(&self, now: u64) -> Option<store::AccountStats> {
        self.store.account_stats(now).await.ok().flatten()
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
