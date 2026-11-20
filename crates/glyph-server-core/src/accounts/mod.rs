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
    /// R2 user uploads (`users/`); `None` disables profile/playlist cover uploads.
    pub blobs: Option<Arc<dyn BlobStore>>,
    /// R2 cover cache (`covers/`); falls back to `blobs` when unset.
    pub cache_blobs: Option<Arc<dyn BlobStore>>,
    pub entropy: Arc<dyn Entropy>,
    /// Public origin of this server, e.g. `https://server.glyphmusic.workers.dev`.
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

/// `K7PX-2QMD` from a typed code. Codes may carry four more characters that
/// only the two devices know (`K7PX-2QMD-H4XA`, see
/// `glyph_protocol::capability::PAIR_KEYS`); they are dropped here.
pub fn normalize_user_code(raw: &str) -> Option<String> {
    let c: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    ((c.len() == 8 || c.len() == 12) && c.bytes().all(|b| USER_CODE_ALPHABET.contains(&b))).then(|| format!("{}-{}", &c[..4], &c[4..8]))
}

/// Mailbox scope of the pairing messages (`LoginStartRequest::pake`) by login id.
const PAIR_SCOPE: &str = "_pair";

/// A SPAKE2 message as the apps send it: base64url, 33 bytes.
fn valid_pake(s: &str) -> bool {
    (40..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// JPEG, PNG, WebP or GIF by magic bytes.
/// Every picture an account stores is WebP. Apps send small lossy WebP; one
/// in the plain format (a single VP8 frame: no metadata, animation or extra
/// bytes) that fits is kept as sent. Anything else is decoded with bounded
/// resources and written again as our own WebP, smaller until it fits.
pub fn to_webp(bytes: &[u8], max_w: u32, max_h: u32, max_bytes: usize) -> Result<Vec<u8>, ApiErr> {
    let invalid = || ApiErr::bad_request("invalid or oversized image");
    let mut reader = ::image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|_| invalid())?;
    let format = reader.format();
    if !matches!(format, Some(::image::ImageFormat::Png | ::image::ImageFormat::Jpeg | ::image::ImageFormat::WebP | ::image::ImageFormat::Gif)) {
        return Err(invalid());
    }
    let mut limits = ::image::Limits::default();
    limits.max_alloc = Some(32 * 1024 * 1024);
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| invalid())?;
    if plain_webp(bytes) && decoded.width() <= max_w && decoded.height() <= max_h && bytes.len() <= max_bytes {
        return Ok(bytes.to_vec());
    }
    let mut picture = decoded.thumbnail(max_w, max_h);
    loop {
        let rgba = picture.to_rgba8();
        let mut output = Vec::new();
        ::image::codecs::webp::WebPEncoder::new_lossless(&mut output)
            .encode(&rgba, rgba.width(), rgba.height(), ::image::ExtendedColorType::Rgba8)
            .map_err(|_| invalid())?;
        if output.len() <= max_bytes {
            return Ok(output);
        }
        if picture.width().max(picture.height()) <= 64 {
            return Err(invalid());
        }
        picture = picture.thumbnail((picture.width() * 3 / 4).max(1), (picture.height() * 3 / 4).max(1));
    }
}

/// `RIFF <size> WEBP VP8 ` with the RIFF size covering exactly the file: one
/// lossy frame and nothing else (no EXIF/XMP, no animation, nothing appended).
fn plain_webp(b: &[u8]) -> bool {
    if b.len() <= 20 || &b[..4] != b"RIFF" || &b[8..16] != b"WEBPVP8 " {
        return false;
    }
    let riff = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
    let frame = u32::from_le_bytes([b[16], b[17], b[18], b[19]]) as usize;
    riff + 8 == b.len() && 20 + frame + (frame & 1) == b.len()
}

/// The account has used all the space the server gives it.
pub fn account_full() -> ApiErr {
    ApiErr::new(413, "account_full", "this account is full; delete playlists or pictures to make room")
}

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
struct LinkPasswordReq {
    login_id: String,
    salt: String,
    key: String,
}
#[derive(Deserialize)]
struct LinkDeleteReq {
    login_id: String,
    confirm_handle: String,
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

    /// Keeps the device's ban hashes and app version current; writes only when one changed.
    pub(crate) async fn mark(&self, svc: &Services, device: &Device, req: &Request) {
        let ip = req.client_ip.as_deref().and_then(|ip| crate::bans::ip_hash(&svc.config.bans.salt, ip));
        let hw = req.header(glyph_protocol::HARDWARE_ID_HEADER).and_then(crate::bans::hardware_hash);
        let ver = device_app_version(req);
        let ip = ip.filter(|h| device.ip_hash.as_deref() != Some(h.as_str()));
        let hw = hw.filter(|h| device.hw_hash.as_deref() != Some(h.as_str()));
        let ver = ver.filter(|v| device.app_version.as_deref() != Some(v.as_str()));
        if ip.is_some() || hw.is_some() || ver.is_some() {
            let _ = self.store.mark_device(&device.id, ip.as_deref(), hw.as_deref(), ver.as_deref()).await;
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
            app_version: None,
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

/// App version reported by a Glyph client: `X-Glyph-App-Version` (web player)
/// or `User-Agent: Glyph/<version>` (desktop / Android apps).
fn device_app_version(req: &Request) -> Option<String> {
    if let Some(raw) = req.header("x-glyph-app-version") {
        let v = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
        if !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')) {
            return Some(v.to_string());
        }
    }
    req.header("user-agent").and_then(glyph_protocol::app_version_from_user_agent).map(str::to_string)
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

/// Most accounts that can hold the admin role at once.
pub const MAX_ADMINS: usize = 20;

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

    pub(super) async fn check_account_cap(&self, svc: &Services) -> Result<(), ApiErr> {
        if svc.config.max_accounts == 0 {
            return Ok(());
        }
        if self.store.user_count().await.map_err(store_err)? >= u64::from(svc.config.max_accounts) {
            return Err(ApiErr::new(
                403,
                "account_limit_reached",
                "this server is not accepting new accounts right now",
            ));
        }
        Ok(())
    }

    pub async fn admin_list(
        &self,
        limit: u32,
        offset: u32,
        query: Option<&str>,
        bans: &crate::bans::BanList,
        now: u64,
    ) -> Result<crate::admin::types::AdminAccountsResponse, ApiErr> {
        use crate::admin::types::{AdminAccountSummary, AdminAccountsResponse};
        use crate::bans::BanKind;
        let (rows, total) = self.store.list_users(limit, offset, query).await.map_err(store_err)?;
        let admins = self.store.admins().await.map_err(store_err)?;
        let accounts = rows
            .into_iter()
            .map(|u| {
                let banned = bans.bans.iter().any(|b| b.kind == BanKind::Account && b.value == u.id && b.live(now));
                let admin = admins.contains(&u.id);
                AdminAccountSummary {
                    id: u.id,
                    handle: u.handle,
                    created_at: u.created_at,
                    device_limit: u.device_limit,
                    device_limit_set: (u.device_limit_set > 0).then_some(u.device_limit_set),
                    devices: u.devices,
                    last_seen: u.last_seen,
                    sync_bytes: u.sync_bytes,
                    banned,
                    admin,
                    avatar: u.avatar,
                    banner: u.banner,
                }
            })
            .collect();
        Ok(AdminAccountsResponse { accounts, total, limit, offset })
    }

    pub async fn admin_account(
        &self,
        handle: &str,
        bans: &crate::bans::BanList,
        now: u64,
    ) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        use crate::admin::types::{AdminAccount, AdminDevice};
        use crate::bans::BanKind;
        let handle = normalize_handle(handle).map_err(|_| ApiErr::not_found("no such account"))?;
        let u = self.store.user_by_handle(&handle).await.map_err(store_err)?.ok_or_else(|| ApiErr::not_found("no such account"))?;
        let devices = self.store.devices(&u.id).await.map_err(store_err)?;
        let used_bytes = self.used_bytes(&u.id).await?;
        let banned = bans.bans.iter().any(|b| b.kind == BanKind::Account && b.value == u.id && b.live(now));
        let admin = self.store.admins().await.map_err(store_err)?.contains(&u.id);
        let prof = self.store.profile(&u.id).await.map_err(store_err)?;
        Ok(AdminAccount {
            id: u.id,
            handle: u.handle,
            created_at: u.created_at,
            device_limit: u.device_limit,
            device_limit_set: (u.device_limit_set > 0).then_some(u.device_limit_set),
            devices: devices.into_iter().map(|d| AdminDevice { id: d.id, name: d.name, platform: d.platform, last_seen: d.last_seen, hardware: d.hw_hash, ip_known: d.ip_hash.is_some(), app_version: d.app_version }).collect(),
            used_bytes,
            banned,
            admin,
            avatar: prof.as_ref().and_then(|p| p.avatar.clone()),
            banner: prof.and_then(|p| p.banner),
        })
    }

    /// Removes an account's profile picture and/or banner and deletes the
    /// unreferenced blob(s).
    pub async fn admin_reset_profile_images(
        &self,
        handle: &str,
        avatar: bool,
        banner: bool,
        bans: &crate::bans::BanList,
        now: u64,
    ) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        if !avatar && !banner {
            return Err(ApiErr::bad_request("set avatar and/or banner to true"));
        }
        let handle = normalize_handle(handle).map_err(|_| ApiErr::not_found("no such account"))?;
        let u = self.store.user_by_handle(&handle).await.map_err(store_err)?.ok_or_else(|| ApiErr::not_found("no such account"))?;
        if let Some(mut p) = self.store.profile(&u.id).await.map_err(store_err)? {
            let old_avatar = if avatar { p.avatar.take() } else { None };
            let old_banner = if banner { p.banner.take() } else { None };
            if old_avatar.is_some() || old_banner.is_some() {
                if let Some(blobs) = &self.blobs {
                    // Avatar and banner may share the same sha256; only delete blobs no longer referenced.
                    if let Some(sha) = old_avatar.as_deref().filter(|s| p.banner.as_deref() != Some(*s)) {
                        blobs.delete_prefix(&format!("users/{}/profile/{sha}", u.id)).await.map_err(store_err)?;
                    }
                    if let Some(sha) = old_banner
                        .as_deref()
                        .filter(|s| p.avatar.as_deref() != Some(*s) && old_avatar.as_deref() != Some(*s))
                    {
                        blobs.delete_prefix(&format!("users/{}/profile/{sha}", u.id)).await.map_err(store_err)?;
                    }
                }
                p.updated_at = now;
                self.store.put_profile(&u.id, &p).await.map_err(store_err)?;
            }
        }
        self.admin_account(&handle, bans, now).await
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
    pub async fn admin_revoke(
        &self,
        handle: &str,
        device: Option<&str>,
        bans: &crate::bans::BanList,
        now: u64,
    ) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        let (u, devices) = self.admin_devices(handle).await?;
        let mut removed = 0;
        for d in devices.iter().filter(|d| device.is_none_or(|id| id == d.id)) {
            if self.store.delete_device(&u.id, &d.id).await.map_err(store_err)? { removed += 1; }
        }
        if removed == 0 && device.is_some() {
            return Err(ApiErr::not_found("no such device on this account (see glyph remote accounts show)"));
        }
        self.admin_account(handle, bans, now).await
    }

    /// Gives or takes the admin role (server owner only, checked by the caller).
    pub async fn admin_set_role(
        &self,
        handle: &str,
        admin: bool,
        bans: &crate::bans::BanList,
        now: u64,
    ) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        let id = self.admin_account(handle, bans, now).await?.id;
        let mut ids = self.store.admins().await.map_err(store_err)?;
        ids.retain(|x| x != &id);
        if admin {
            if ids.len() >= MAX_ADMINS {
                return Err(ApiErr::bad_request(format!("at most {MAX_ADMINS} admins")));
            }
            ids.push(id);
        }
        self.store.set_admins(&ids).await.map_err(store_err)?;
        self.admin_account(handle, bans, now).await
    }

    /// The account behind an app access token, when it holds the admin role
    /// and is not banned. A live sign-in without the role gets 403 (not 401,
    /// which would sign the app out); an expired one gets `token_expired`.
    pub async fn admin_session(&self, svc: &Services, token: &str) -> Result<User, ApiErr> {
        use crate::bans::BanKind;
        let token = token.trim();
        if token.is_empty() || token.len() > 128 {
            return Err(unauthorized());
        }
        let now = svc.rt.now_ms();
        let device = self.store.device_by_access(&hash(token)).await.map_err(store_err)?.ok_or_else(unauthorized)?;
        if device.access_exp <= now {
            return Err(ApiErr::new(401, "token_expired", "refresh the access token"));
        }
        let user = self.store.user(&device.user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
        let not_admin = || ApiErr::new(403, "not_admin", "this account is not an admin on this server");
        if !self.store.admins().await.map_err(store_err)?.contains(&user.id) {
            return Err(not_admin());
        }
        let bans = &svc.config.bans;
        if bans.bans.iter().any(|b| b.kind == BanKind::Account && b.value == user.id && b.live(now)) {
            return Err(not_admin());
        }
        if now.saturating_sub(device.last_seen) > TOUCH_EVERY {
            let _ = self.store.touch_device(&device.id, now, None).await;
        }
        Ok(user)
    }

    pub async fn admin_stats(&self, now: u64) -> Option<store::AccountStats> {
        self.store.account_stats(now).await.ok().flatten()
    }

    /// Existing devices beyond a lowered limit stay signed in; no new ones until below it.
    pub async fn admin_set_limit(
        &self,
        handle: &str,
        limit: Option<u8>,
        bans: &crate::bans::BanList,
        now: u64,
    ) -> Result<crate::admin::types::AdminAccount, ApiErr> {
        let id = self.admin_account(handle, bans, now).await?.id;
        let raw = match limit {
            Some(l) => Self::check_limit(l)?,
            None => 0,
        };
        self.store.set_device_limit(&id, raw).await.map_err(store_err)?;
        self.admin_account(handle, bans, now).await
    }
}
