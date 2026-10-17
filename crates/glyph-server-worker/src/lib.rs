//! Cloudflare Worker host for glyph-server-core.
//!
//! Bindings (see wrangler.toml): KV namespace `GLYPH_KV`, secrets
//! `YOUTUBE_API_KEY`, `JAMENDO_CLIENT_ID`, `SOUNDCLOUD_CLIENT_ID`,
//! `SOUNDCLOUD_CLIENT_SECRET`, `SPOTIFY_CLIENT_ID`, `SPOTIFY_CLIENT_SECRET`,
//! `LASTFM_API_KEY`. Compiled for wasm32 only.
#![cfg(target_arch = "wasm32")]

use async_trait::async_trait;
use futures::StreamExt;
use glyph_server_core::admin::host::AdminHost;
use glyph_server_core::cache::{MinTtlCache, TieredCache};
use glyph_server_core::ratelimit::CacheRateLimiter;
use glyph_server_core::{
    Body, Cache, Config, HttpClient, HttpError, HttpRequest, HttpResponse, Method, Runtime, Secrets,
    Server, Services,
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

#[async_trait(?Send)]
impl Cache for CacheApi {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let cache = worker::Cache::default();
        let mut resp = cache.get(Self::url(key), false).await.ok()??;
        resp.bytes().await.ok()
    }
    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64) {
        let _ = &self.rt;
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

fn build_server(env: &Env) -> Server {
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
    // Admin API is only enabled when this secret holds an Argon2id hash
    // (`glyph-server hash-password`, or `glyph-server cloudflare setup`).
    config.admin_password_hash = secret(env, "ADMIN_PASSWORD_HASH")
        .filter(|h| glyph_server_core::admin::auth::is_valid_hash(h));
    if let Some(n) = secret(env, "RATE_LIMIT_PER_MIN").and_then(|v| v.parse().ok()) {
        config.rate_limit_per_min = n;
    }
    if let Some(n) = secret(env, "YOUTUBE_DAILY_BUDGET_UNITS").and_then(|v| v.parse().ok()) {
        config.youtube_daily_budget_units = n;
    }
    // Rate-limit counters live in the (free, unmetered) Cache API, not in KV.
    let limiter = Arc::new(CacheRateLimiter::new(api, rt.clone()));
    Server::new(Services { http: Arc::new(FetchClient), cache, limiter, rt, config }).with_host(Arc::new(WorkerHost))
}

/// Secrets, limits and updates are managed with wrangler / Cloudflare, not at runtime.
struct WorkerHost;

impl AdminHost for WorkerHost {
    fn platform(&self) -> &'static str {
        "worker"
    }
}

fn to_core_method(m: worker::Method) -> Method {
    match m {
        worker::Method::Get | worker::Method::Head => Method::Get,
        worker::Method::Post => Method::Post,
        worker::Method::Put => Method::Put,
        worker::Method::Options => Method::Options,
        _ => Method::Other,
    }
}

#[event(fetch)]
async fn fetch(mut req: worker::Request, env: Env, _ctx: Context) -> Result<worker::Response> {
    console_error_panic_hook::set_once();
    let url = req.url()?;
    let target = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };
    let headers: Vec<(String, String)> = req.headers().entries().collect();
    let ip = req.headers().get("cf-connecting-ip").ok().flatten();
    let method = to_core_method(req.method());
    let body = if matches!(method, Method::Post | Method::Put) { req.bytes().await.unwrap_or_default() } else { Vec::new() };

    let server = build_server(&env);
    // Deployed Workers are HTTPS-only; `wrangler dev` serves http://localhost.
    let secure = url.scheme() == "https";
    let local = matches!(url.host_str(), Some("localhost") | Some("127.0.0.1") | Some("[::1]"));
    let request = glyph_server_core::Request::new(method, &target, headers, body, ip).with_transport(secure, local);
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
