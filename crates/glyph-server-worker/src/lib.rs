//! Cloudflare Worker host for glyph-server-core.
//!
//! Bindings (see wrangler.toml): KV namespace `GLYPH_KV`, secrets
//! `YOUTUBE_API_KEY`, `JAMENDO_CLIENT_ID`, `SOUNDCLOUD_CLIENT_ID`,
//! `SOUNDCLOUD_CLIENT_SECRET`, `SPOTIFY_CLIENT_ID`, `SPOTIFY_CLIENT_SECRET`,
//! `LASTFM_API_KEY`. Optional: D1 `GLYPH_DB` + var `PUBLIC_ORIGIN` enable
//! accounts and sync, R2 `GLYPH_COVERS` the cover cache. Compiled for wasm32 only.
#![cfg(target_arch = "wasm32")]
mod accounts;
mod abuse;
mod audio_cache;
mod storage;
mod variants;
mod media;

use async_trait::async_trait;
use futures::StreamExt;
use glyph_server_core::admin::host::{AdminHost, HostError};
use glyph_server_core::admin::types::{Capabilities, Limits};
use glyph_server_core::cache::{MinTtlCache, TieredCache};
use glyph_server_core::{
    Body, Cache, Config, HttpClient, HttpError, HttpRequest, HttpResponse, Method, NoCommands, Runtime,
    Secrets, Server, Services,
};
use std::sync::Arc;
use std::time::Duration;
use worker::{event, Context, Env, Fetch, Headers, RequestInit, Result};

// ------------------------------------------------------------------ HTTP

struct FetchClient;

#[async_trait(?Send)]
impl HttpClient for FetchClient {
    async fn send(&self, r: HttpRequest) -> std::result::Result<HttpResponse, HttpError> {
        // Error texts are deliberately generic: they must never contain the URL (API keys).
        let fail = |_: worker::Error| HttpError("fetch failed".into());
        let headers = Headers::new();
        for (k, v) in &r.headers {
            headers.set(k, v).map_err(fail)?;
        }
        let mut init = RequestInit::new();
        init.with_method(match r.method {
            Method::Post => worker::Method::Post,
            _ => worker::Method::Get,
        })
        .with_headers(headers);
        if let Some(body) = &r.body {
            init.with_body(Some(worker::js_sys::Uint8Array::from(body.as_slice()).into()));
        }
        let req = worker::Request::new_with_init(&r.url, &init).map_err(fail)?;
        let mut resp = Fetch::Request(req).send().await.map_err(fail)?;
        let status = resp.status_code();
        let body = resp.bytes().await.map_err(fail)?;
        Ok(HttpResponse { status, body })
    }
}

// ------------------------------------------------------------------ runtime

struct WorkerRuntime;

#[async_trait(?Send)]
impl Runtime for WorkerRuntime {
    fn now_ms(&self) -> u64 {
        worker::js_sys::Date::now() as u64
    }
    async fn sleep(&self, ms: u64) {
        worker::Delay::from(Duration::from_millis(ms)).await;
    }
}

// ------------------------------------------------------------------ caches

/// Cloudflare Cache API: per data center, free, no write quota.
struct CacheApi {
    rt: Arc<dyn Runtime>,
}

impl CacheApi {
    fn url(key: &str) -> String {
        let enc: String = url_encode(key);
        format!("https://glyph-cache.invalid/{enc}")
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

std::thread_local! {
    static FAST_CACHE: std::cell::RefCell<std::collections::HashMap<String, (u64, Vec<u8>)>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[async_trait(?Send)]
impl Cache for CacheApi {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let now = self.rt.now_ms();
        if let Some(v) = FAST_CACHE.with(|c| {
            let mut m = c.borrow_mut();
            match m.get(key) {
                Some((exp, v)) if *exp > now => Some(v.clone()),
                Some(_) => { m.remove(key); None }
                None => None,
            }
        }) {
            return Some(v);
        }
        // Ephemeral circuit-breakers and 15m provider search caches stay in isolate
        // memory to avoid 12+ Cache API HTTP subrequests during a parallel /v1/search.
        if key.starts_with("skip:") || key.starts_with("search:") {
            return None;
        }
        let cache = worker::Cache::default();
        let mut resp = cache.get(Self::url(key), false).await.ok()??;
        let bytes = resp.bytes().await.ok()?;
        if bytes.len() <= 65_536 {
            FAST_CACHE.with(|c| {
                let mut m = c.borrow_mut();
                if m.len() >= 128 { m.retain(|_, (exp, _)| *exp > now); }
                if m.len() < 128 { m.insert(key.to_string(), (now + 300_000, bytes.clone())); }
            });
        }
        Some(bytes)
    }
    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64) {
        let now = self.rt.now_ms();
        let in_mem_only = key.starts_with("skip:") || key.starts_with("search:");
        if value.len() <= 65_536 {
            let mem_ttl_ms = if in_mem_only { ttl_s * 1000 } else { ttl_s.min(300) * 1000 };
            FAST_CACHE.with(|c| {
                let mut m = c.borrow_mut();
                if m.len() >= 128 { m.retain(|_, (exp, _)| *exp > now); }
                if m.len() >= 128 { m.clear(); }
                m.insert(key.to_string(), (now + mem_ttl_ms, value.clone()));
            });
        }
        if in_mem_only {
            return;
        }
        let cache = worker::Cache::default();
        let Ok(resp) = worker::Response::from_bytes(value) else { return };
        let headers = Headers::new();
        let _ = headers.set("cache-control", &format!("public, max-age={ttl_s}"));
        let _ = headers.set("content-type", "application/octet-stream");
        let _ = cache.put(Self::url(key), resp.with_headers(headers)).await;
    }
}

/// Workers KV: global, eventually consistent. Free plan: 1,000 writes/day,
/// so only long-lived entries are written (see `MinTtlCache`).
struct KvCache {
    kv: worker::kv::KvStore,
}

#[async_trait(?Send)]
impl Cache for KvCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.kv.get(key).bytes().await.ok().flatten()
    }
    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64) {
        if let Ok(b) = self.kv.put_bytes(key, &value) {
            // KV requires expiration_ttl >= 60.
            let _ = b.expiration_ttl(ttl_s.max(60)).execute().await;
        }
    }
}

// ------------------------------------------------------------------ wiring

fn secret(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|s| s.to_string())
        .or_else(|_| env.var(name).map(|v| v.to_string()))
        .ok()
}

fn build_server(env: &Env, saved: Option<Limits>) -> Server {
    let rt: Arc<dyn Runtime> = Arc::new(WorkerRuntime);
    let api: Arc<dyn Cache> = Arc::new(CacheApi { rt: rt.clone() });
    let cache: Arc<dyn Cache> = match env.kv("GLYPH_KV") {
        Ok(kv) => {
            let l2: Arc<dyn Cache> = Arc::new(MinTtlCache {
                inner: Arc::new(KvCache { kv }),
                min_ttl_s: 3600,
            });
            Arc::new(TieredCache { l1: api.clone(), l2 })
        }
        Err(_) => api.clone(),
    };
    let secrets = Secrets {
        youtube_api_key: secret(env, "YOUTUBE_API_KEY"),
        jamendo_client_id: secret(env, "JAMENDO_CLIENT_ID"),
        soundcloud_client_id: secret(env, "SOUNDCLOUD_CLIENT_ID"),
        soundcloud_client_secret: secret(env, "SOUNDCLOUD_CLIENT_SECRET"),
        spotify_client_id: secret(env, "SPOTIFY_CLIENT_ID"),
        spotify_client_secret: secret(env, "SPOTIFY_CLIENT_SECRET"),
        lastfm_api_key: secret(env, "LASTFM_API_KEY"),
    };
    let mut config = Config::new(secrets);
    config.proxy_audio = true;
    config.server_bearer_hash = secret(env, "GLYPH_SERVER_BEARER").map(|t| glyph_server_core::access::token_hash(&t));
    if let Some(n) = secret(env, "RATE_LIMIT_PER_MIN").and_then(|v| v.parse().ok()) {
        config.rate_limit_per_min = n;
    }
    if let Some(n) = secret(env, "YOUTUBE_DAILY_BUDGET_UNITS").and_then(|v| v.parse().ok()) {
        config.youtube_daily_budget_units = n;
    }
    config.require_account = secret(env, "REQUIRE_ACCOUNT").is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"));
    // Limits saved with `glyph-server remote limits` win over wrangler.toml.
    if let Some(l) = &saved {
        config.require_account = l.require_account;
        config.rate_limit_per_min = l.rate_limit_per_min;
        config.youtube_daily_budget_units = l.youtube_daily_budget_units;
        config.import_max_pages = l.import_max_pages;
        config.companion_enabled = l.companion_enabled;
        config.audio_cache_days = l.audio_cache_days;
        config.audio_cache_max_gb = l.audio_cache_max_gb;
        config.d1_max_mb = l.d1_max_mb;
    }
    let commands: Arc<dyn glyph_server_core::CommandRunner> = match (secret(env, "YTDLP_SERVICE_URL"), secret(env, "YTDLP_SERVICE_TOKEN")) {
        // `remote companion off`: YouTube search is off, audio answers 403.
        _ if !config.companion_enabled => Arc::new(NoCommands),
        (Some(url), Some(token)) if url.starts_with("https://") && token.len() >= 32 => {
            config.ytdlp_path = Some("remote".into());
            // Remote extraction includes the network round trip and metadata lookup.
            Arc::new(RemoteCommands { url, token })
        }
        _ => Arc::new(NoCommands),
    };
    // Admin API is only enabled when this secret holds an Argon2id hash
    // (`glyph-server hash-password`, or `glyph-server cloudflare setup`).
    config.admin_password_hash = secret(env, "ADMIN_PASSWORD_HASH")
        .filter(|h| glyph_server_core::admin::auth::is_valid_hash(h));
    // Short playlist links (`/p/<id>`); the domain must route `/p/*` to this Worker.
    config.share_origin = secret(env, "SHARE_ORIGIN").filter(|o| o.starts_with("https://"));
    // Abuse counters must be atomic and shared across locations. Requires 0009.
    let api_cache = api.clone();
    let limiter = Arc::new(abuse::D1Limiter { db: env.d1("GLYPH_DB").ok(), rt: rt.clone(), admin: Arc::new(glyph_server_core::ratelimit::CacheRateLimiter::new(api.clone(), rt.clone())) });
    let server = Server::new(Services { http: Arc::new(FetchClient), cache, limiter, rt, cmds: commands, config }).with_host(Arc::new(WorkerHost { companion_url:secret(env,"YTDLP_SERVICE_URL"), companion_token:secret(env,"YTDLP_SERVICE_TOKEN"), db: env.d1("GLYPH_DB").ok(), cache: api_cache, bucket: env.bucket("GLYPH_COVERS").ok() }));
    // Accounts need the D1 database and the public origin (passkeys are bound to it).
    match (env.d1("GLYPH_DB"), secret(env, "PUBLIC_ORIGIN").filter(|o| o.starts_with("https://") || o.starts_with("http://localhost"))) {
        (Ok(db), Some(origin)) => server.with_accounts(glyph_server_core::accounts::Accounts {
            store: Arc::new(accounts::D1Store::new(db)),
            blobs: env.bucket("GLYPH_COVERS").ok().map(|bucket| Arc::new(accounts::R2Blobs { bucket, db: env.d1("GLYPH_DB").ok() }) as Arc<dyn glyph_server_core::accounts::store::BlobStore>),
            entropy: Arc::new(accounts::WebCrypto),
            origin: origin.trim_end_matches('/').to_string(),
            extra_origins: secret(env, "ACCOUNT_EXTRA_ORIGINS").map(|v| v.split(',').map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect()).unwrap_or_default(),
            rp_id: secret(env, "ACCOUNT_RP_ID"),
        }),
        _ => server,
    }
}

struct RemoteCommands { url: String, token: String }
#[async_trait(?Send)]
impl glyph_server_core::CommandRunner for RemoteCommands {
    async fn run(&self, _program: &str, args: &[String], _timeout_ms: u64) -> std::result::Result<Vec<u8>, glyph_server_core::CommandError> {
        use glyph_server_core::{media::{CommandRequest, CommandResponse, valid_command}, CommandError};
        if !valid_command(args) { return Err(CommandError::Failed); }
        let body = serde_json::to_vec(&CommandRequest { args: args.to_vec() }).map_err(|_| CommandError::Failed)?;
        let req = HttpRequest {
            method: Method::Post,
            url: format!("{}/internal/ytdlp", self.url.trim_end_matches('/')),
            headers: vec![("authorization".into(), format!("Bearer {}", self.token)), ("content-type".into(), "application/json".into())],
            body: Some(body),
        };
        let headers = Headers::new();
        for (k,v) in &req.headers { headers.set(k,v).map_err(|_| CommandError::Failed)?; }
        let mut init = RequestInit::new();
        init.with_method(worker::Method::Post).with_headers(headers).with_redirect(worker::RequestRedirect::Manual)
            .with_body(Some(worker::js_sys::Uint8Array::from(req.body.as_deref().unwrap_or_default()).into()));
        let request = worker::Request::new_with_init(&req.url, &init).map_err(|_| CommandError::Failed)?;
        let mut resp = Fetch::Request(request).send().await.map_err(|_| CommandError::Failed)?;
        if resp.status_code() != 200 { return Err(CommandError::Failed); }
        let bytes = resp.bytes().await.map_err(|_| CommandError::Failed)?;
        let reply: CommandResponse = serde_json::from_slice(&bytes).map_err(|_| CommandError::Failed)?;
        Ok(reply.stdout.into_bytes())
    }

    /// The companion runs every provider search natively; here only one small
    /// JSON answer is parsed. Busy or offline: `None`, the Worker resolves itself.
    async fn resolve(&self, req: &glyph_protocol::ResolveRequest) -> Option<glyph_protocol::ResolveResponse> {
        let body = serde_json::to_vec(req).ok()?;
        let headers = Headers::new();
        headers.set("authorization", &format!("Bearer {}", self.token)).ok()?;
        headers.set("content-type", "application/json").ok()?;
        let mut init = RequestInit::new();
        init.with_method(worker::Method::Post).with_headers(headers).with_redirect(worker::RequestRedirect::Manual)
            .with_body(Some(worker::js_sys::Uint8Array::from(body.as_slice()).into()));
        let url = format!("{}/internal/resolve", self.url.trim_end_matches('/'));
        let request = worker::Request::new_with_init(&url, &init).ok()?;
        let work = async {
            let mut resp = Fetch::Request(request).send().await.ok()?;
            if resp.status_code() != 200 { return None; }
            serde_json::from_slice(&resp.bytes().await.ok()?).ok()
        };
        glyph_server_core::traits::with_timeout(&WorkerRuntime, if req.youtube_only { 55_000 } else { 25_000 }, work).await.flatten()
    }

    /// LRCLIB rate-limits Cloudflare egress, so the companion (home
    /// connection) answers first; only a missing/broken companion lets the
    /// Worker look lyrics up itself.
    async fn lyrics(&self, query: &glyph_server_core::LyricsQuery) -> Option<glyph_server_core::LyricsLookup> {
        use glyph_server_core::LyricsLookup;
        let body = serde_json::to_vec(query).ok()?;
        let headers = Headers::new();
        headers.set("authorization", &format!("Bearer {}", self.token)).ok()?;
        headers.set("content-type", "application/json").ok()?;
        let mut init = RequestInit::new();
        init.with_method(worker::Method::Post).with_headers(headers).with_redirect(worker::RequestRedirect::Manual)
            .with_body(Some(worker::js_sys::Uint8Array::from(body.as_slice()).into()));
        let url = format!("{}/internal/lyrics", self.url.trim_end_matches('/'));
        let request = worker::Request::new_with_init(&url, &init).ok()?;
        let work = async {
            let mut resp = Fetch::Request(request).send().await.ok()?;
            match resp.status_code() {
                200 => Some(LyricsLookup::Found(serde_json::from_slice(&resp.bytes().await.ok()?).ok()?)),
                404 => Some(LyricsLookup::None),
                // Busy or broken companion: try the local lookup.
                _ => None,
            }
        };
        glyph_server_core::traits::with_timeout(&WorkerRuntime, 20_000, work).await.flatten()
    }
}

/// Secrets and updates are managed with wrangler / Cloudflare. Limits and the
/// companion switch are saved in D1 (`server_settings`), so `glyph-server
/// remote ...` changes them from any machine.
struct WorkerHost {
    db: Option<worker::D1Database>,
    cache: Arc<dyn Cache>,
    bucket: Option<worker::Bucket>,
    companion_url: Option<String>,
    companion_token: Option<String>,
}

impl WorkerHost {
    async fn companion_call<T: serde::de::DeserializeOwned>(&self, path: &str, post: bool) -> std::result::Result<T,HostError> {
        let base=self.companion_url.as_deref().filter(|s|s.starts_with("https://")).ok_or_else(||HostError::Unsupported("companion URL is not configured".into()))?;
        let token=self.companion_token.as_deref().filter(|s|s.len()>=32).ok_or_else(||HostError::Unsupported("companion token is not configured".into()))?;
        let work=async {
            let headers=worker::Headers::new();
            headers.set("authorization",&format!("Bearer {token}"))?;
            let mut init=worker::RequestInit::new();
            init.with_method(if post {worker::Method::Post}else{worker::Method::Get}).with_headers(headers).with_redirect(worker::RequestRedirect::Manual);
            let mut response=worker::Fetch::Request(worker::Request::new_with_init(&format!("{}{path}",base.trim_end_matches('/')),&init)?).send().await?;
            if response.status_code()!=200 {
                let status=response.status_code();
                if status==404 {return Err(worker::Error::RustError("companion management unavailable; update an older companion image first".into()));}
                let detail=response.text().await.unwrap_or_default().replace(token,"[redacted]").chars().take(512).collect::<String>();
                return Err(worker::Error::RustError(format!("companion management HTTP {status}: {detail}")));
            }
            response.json::<T>().await
        };
        glyph_server_core::traits::with_timeout(&WorkerRuntime,150_000,work).await
            .ok_or_else(||HostError::Failed("companion management timed out; check its status before retrying an update".into()))?
            .map_err(|e|HostError::Failed(format!("companion management failed: {e}")))
    }
}

const SETTINGS_CACHE_KEY: &str = "admin:limits:v2";
/// Each Cloudflare location re-reads D1 at most this often.
const SETTINGS_CACHE_S: u64 = 30;

std::thread_local! {
    static LOCAL_LIMITS: std::cell::RefCell<Option<(u64, Option<Limits>)>> = const { std::cell::RefCell::new(None) };
}

/// Saved limits, or `None` when nothing was saved (wrangler.toml applies).
async fn saved_limits(env: &Env) -> Option<Limits> {
    let now = worker::js_sys::Date::now() as u64;
    if let Some((_, cached)) = LOCAL_LIMITS.with(|c| c.borrow().clone()).filter(|(exp, _)| *exp > now) {
        return cached;
    }
    let rt: Arc<dyn Runtime> = Arc::new(WorkerRuntime);
    let cache = CacheApi { rt };
    if let Some(hit) = glyph_server_core::cache::get_json::<Option<Limits>>(&cache, SETTINGS_CACHE_KEY).await {
        LOCAL_LIMITS.with(|c| *c.borrow_mut() = Some((now + SETTINGS_CACHE_S * 1000, hit.clone())));
        return hit;
    }
    let db = env.d1("GLYPH_DB").ok()?;
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    let row = db.prepare("SELECT value FROM server_settings WHERE key = 'limits'").first::<Row>(None).await;
    let limits = match row {
        Ok(row) => row.map(|r| {
            let decoded = (|| {
                let mut value: serde_json::Value = serde_json::from_str(&r.value).ok()?;
                // Old stored limits must not override an explicit environment gate.
                let object = value.as_object_mut()?;
                if !object.contains_key("require_account") {
                    object.insert("require_account".into(),secret(env, "REQUIRE_ACCOUNT").is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false")).into());
                }
                serde_json::from_value::<Limits>(value).ok()
            })();
            decoded.unwrap_or_else(|| Limits { require_account: true, ..Default::default() })
        }),
        // Never reopen a protected API if its persisted access policy cannot be
        // loaded. Retain the last policy, otherwise deny anonymous requests.
        Err(_) => return LOCAL_LIMITS.with(|c| c.borrow().as_ref().and_then(|(_, l)| l.clone()))
            .or_else(|| Some(Limits { require_account: true, ..Default::default() })),
    };
    LOCAL_LIMITS.with(|c| *c.borrow_mut() = Some((now + SETTINGS_CACHE_S * 1000, limits.clone())));
    glyph_server_core::cache::put_json(&cache, SETTINGS_CACHE_KEY, &limits, SETTINGS_CACHE_S).await;
    limits
}

#[async_trait(?Send)]
impl AdminHost for WorkerHost {
    fn platform(&self) -> &'static str {
        "worker"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { set_limits: self.db.is_some(), ..Capabilities::default() }
    }

    async fn companion_status(&self) -> std::result::Result<glyph_server_core::admin::types::CompanionStatus,HostError> {
        self.companion_call("/internal/status",false).await
    }
    async fn companion_update(&self, rdlp: bool, apply: bool) -> std::result::Result<glyph_server_core::admin::types::MaintenanceResponse,HostError> {
        self.companion_call(match (rdlp,apply) {
            (false,false)=>"/internal/update/check",(false,true)=>"/internal/update/apply",
            (true,false)=>"/internal/r-dlp/check",(true,true)=>"/internal/r-dlp/apply",
        },true).await
    }
    async fn persist_limits(&self, l: &Limits) -> std::result::Result<(), HostError> {
        let db = self.db.as_ref().ok_or_else(|| HostError::Unsupported("needs the GLYPH_DB binding".into()))?;
        let value = serde_json::to_string(l).map_err(|_| HostError::Failed("could not encode limits".into()))?;
        db.prepare("INSERT INTO server_settings (key, value) VALUES ('limits', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(&[value.into()])
            .map_err(|_| HostError::Failed("could not save limits".into()))?
            .run()
            .await
            .map_err(|_| HostError::Failed(
                "could not save limits (run: npx wrangler d1 migrations apply glyph --remote)".into(),
            ))?;
        // This location sees the change at once, the others within SETTINGS_CACHE_S.
        let now = worker::js_sys::Date::now() as u64;
        LOCAL_LIMITS.with(|c| *c.borrow_mut() = Some((now + SETTINGS_CACHE_S * 1000, Some(l.clone()))));
        glyph_server_core::cache::put_json(self.cache.as_ref(), SETTINGS_CACHE_KEY, &Some(l.clone()), SETTINGS_CACHE_S).await;
        if let Some(bucket) = &self.bucket {
            if !audio_cache::inventory(db, bucket).await.map_err(|_| HostError::Failed("limits saved; storage inventory pending".into()))? {
                return Err(HostError::Failed("limits saved; storage inventory continues in scheduled maintenance".into()));
            }
            audio_cache::trim(db, bucket, l.audio_cache_days, audio_cache::target(u64::from(l.audio_cache_max_gb)*1_000_000_000)).await
                .map_err(|_| HostError::Failed("limits saved; audio cleanup pending".into()))?;
        }
        Ok(())
    }
}

// Retention also runs for cold objects that nobody requests again.
#[event(scheduled)]
async fn scheduled(_event: worker::ScheduledEvent, env: Env, _ctx: worker::ScheduleContext) {
    let Ok(db) = env.d1("GLYPH_DB") else { return; };
    if storage::maintain(&db).await.is_err() {
        worker::console_error!("D1 storage cleanup/measurement failed");
    }
    let Ok(bucket) = env.bucket("GLYPH_COVERS") else { return; };
    let limits = saved_limits(&env).await;
    let days = limits.as_ref().map(|l| l.audio_cache_days).unwrap_or(15);
    let cap = u64::from(limits.as_ref().map(|l| l.audio_cache_max_gb).unwrap_or(10))*1_000_000_000;
    // Start a fresh all-object recount once an hour. Minute ticks continue
    // interrupted pages/eviction without starting another full inventory.
    let now = worker::Date::now().as_millis();
    let reset = db.prepare("UPDATE audio_cache_inventory SET ready=0,cursor=NULL,scan_id=?1 WHERE id=1 AND ready=1 AND finished<=?2")
        .bind(&[(now as f64).into(),(now.saturating_sub(3_600_000) as f64).into()]);
    let Ok(reset) = reset else { return; };
    if reset.run().await.is_err() { return; }
    // JSON batch accounting keeps each page to a handful of D1 queries.
    for _ in 0..2 {
        match audio_cache::inventory(&db, &bucket).await {
            Ok(true) => {
                if audio_cache::trim(&db, &bucket, days, audio_cache::target(cap)).await.is_err() {
                    worker::console_error!("scheduled storage eviction failed; bytes remain charged");
                }
                #[derive(serde::Deserialize,serde::Serialize)]
                struct Usage { bytes: u64, files: u64, protected: u64, reserved: u64 }
                if let Ok(Some(usage)) = db.prepare("SELECT COALESCE(SUM(size),0) AS bytes,COUNT(*) AS files,COALESCE(SUM(CASE WHEN key NOT GLOB 'audio/*' AND key NOT GLOB 'covers/*' THEN size ELSE 0 END),0) AS protected,COALESCE(SUM(CASE WHEN state<>'ready' THEN size ELSE 0 END),0) AS reserved FROM audio_cache").first::<Usage>(None).await {
                    worker::console_log!("storage bytes={} files={} protected={} reserved={} target={}", usage.bytes,usage.files,usage.protected,usage.reserved,audio_cache::target(cap));
                }
                return;
            }
            Ok(false) => continue,
            Err(_) => {
                // Even under D1 pressure, evict known regenerable objects.
                let _ = audio_cache::trim(&db, &bucket, days, audio_cache::target(cap)).await;
                worker::console_error!("scheduled storage inventory failed; known cache cleanup attempted");
                return;
            }
        }
    }
}

fn to_core_method(m: worker::Method) -> Method {
    match m {
        worker::Method::Get | worker::Method::Head => Method::Get,
        worker::Method::Post => Method::Post,
        worker::Method::Put => Method::Put,
        worker::Method::Options => Method::Options,
        worker::Method::Patch => Method::Patch,
        worker::Method::Delete => Method::Delete,
        _ => Method::Other,
    }
}

#[event(fetch)]
async fn fetch(mut req: worker::Request, env: Env, ctx: Context) -> Result<worker::Response> {
    console_error_panic_hook::set_once();
    let url = req.url()?;
    let target = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };
    let headers: Vec<(String, String)> = req.headers().entries().collect();
    let ip = req.headers().get("cf-connecting-ip").ok().flatten();
    let method = to_core_method(req.method());
    let mut body = Vec::new();
    if matches!(method, Method::Post | Method::Put | Method::Patch | Method::Delete) {
        let max = glyph_server_core::server::max_body_bytes(url.path());
        if req.headers().get("content-length")?.and_then(|s| s.parse::<usize>().ok()).is_some_and(|n| n > max) {
            return worker::Response::error("request body too large", 413);
        }
        let mut stream = req.stream()?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > max { return worker::Response::error("request body too large", 413); }
            body.extend_from_slice(&chunk);
        }
    }

    let server = build_server(&env, saved_limits(&env).await);
    // Deployed Workers are HTTPS-only; `wrangler dev` serves http://localhost.
    let secure = url.scheme() == "https";
    let local = matches!(url.host_str(), Some("localhost") | Some("127.0.0.1") | Some("[::1]"));
    let mut request = glyph_server_core::Request::new(method, &target, headers, body, ip).with_transport(secure, local);
    // Cloudflare edge metadata is trusted; arbitrary CF-* client headers are not.
    request.approximate_location = req.cf().and_then(|cf| {
        let parts: Vec<String> = [cf.region(), cf.country()].into_iter().flatten().filter(|s| !s.is_empty() && s != "XX" && s != "T1").collect();
        (!parts.is_empty()).then(|| parts.join(", ").chars().filter(|c| !c.is_control()).take(100).collect())
    });
    if let Err(resp) = server.check_access(&request) {
        let headers = Headers::new();
        for (k,v) in &resp.headers { headers.set(k,v)?; }
        return Ok(worker::Response::from_bytes(resp.bytes().unwrap_or_default().to_vec())?.with_status(resp.status).with_headers(headers));
    }
    if request.path.starts_with("/v1/audio/") || request.path.starts_with("/v1/download/") || request.path == "/v1/upstream" {
        return media::handle(&req, &request, &server, &ctx, &env).await;
    }
    let resp = server.handle(request).await;

    let out = match resp.body {
        Body::Bytes(b) => worker::Response::from_bytes(b)?,
        Body::Stream(s) => worker::Response::from_stream(s.map(|chunk| Ok::<Vec<u8>, worker::Error>(chunk)))?,
    };
    let headers = Headers::new();
    for (k, v) in &resp.headers {
        headers.set(k, v)?;
    }
    Ok(out.with_status(resp.status).with_headers(headers))
}
