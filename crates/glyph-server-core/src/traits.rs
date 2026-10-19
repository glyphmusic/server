//! Seams between the core and the host platform.
//!
//! On `wasm32` the futures are `?Send` (JS values are not `Send`); on every
//! other target they are `Send` so axum/tokio can drive them.

use crate::config::Config;
use std::future::Future;
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync + ?Sized> MaybeSendSync for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSendSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSendSync for T {}

#[cfg(not(target_arch = "wasm32"))]
pub type BoxStream<T> = futures::stream::BoxStream<'static, T>;
#[cfg(target_arch = "wasm32")]
pub type BoxStream<T> = futures::stream::LocalBoxStream<'static, T>;

#[cfg(not(target_arch = "wasm32"))]
pub type BoxFut<T> = futures::future::BoxFuture<'static, T>;
#[cfg(target_arch = "wasm32")]
pub type BoxFut<T> = futures::future::LocalBoxFuture<'static, T>;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn boxed_fut<T>(f: impl Future<Output = T> + Send + 'static) -> BoxFut<T> {
    Box::pin(f)
}
#[cfg(target_arch = "wasm32")]
pub(crate) fn boxed_fut<T>(f: impl Future<Output = T> + 'static) -> BoxFut<T> {
    Box::pin(f)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn boxed_stream<T>(s: impl futures::Stream<Item = T> + Send + 'static) -> BoxStream<T> {
    Box::pin(s)
}
#[cfg(target_arch = "wasm32")]
pub(crate) fn boxed_stream<T>(s: impl futures::Stream<Item = T> + 'static) -> BoxStream<T> {
    Box::pin(s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Put,
    Get,
    Post,
    Options,
    Patch,
    Delete,
    Other,
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self { method: Method::Get, url: url.into(), headers: Vec::new(), body: None }
    }

    pub fn post_form(url: impl Into<String>, fields: &[(&str, &str)]) -> Self {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish();
        Self {
            method: Method::Post,
            url: url.into(),
            headers: vec![("content-type".into(), "application/x-www-form-urlencoded".into())],
            body: Some(body.into_bytes()),
        }
    }

    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Opaque transport failure. Implementations must NOT put the request URL in
/// the message (URLs can carry API keys).
#[derive(Debug, Clone)]
pub struct HttpError(pub String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for HttpError {}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait HttpClient: MaybeSendSync {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// Key/value cache with per-entry TTL (seconds). Best effort: a miss is always
/// acceptable and write errors are swallowed by the implementation.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait Cache: MaybeSendSync {
    async fn get(&self, key: &str) -> Option<Vec<u8>>;
    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64);
    /// Optional statistics (native only).
    async fn stats(&self) -> Option<CacheStats> {
        None
    }
    /// Drop cached data (admin keys such as lockouts survive). False = unsupported.
    async fn clear(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CacheStats {
    pub entries: u64,
    pub hits: u64,
    pub misses: u64,
    pub disk_entries: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    Allowed,
    Limited { retry_after_s: u64 },
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait RateLimiter: MaybeSendSync {
    /// Count one hit for `key`; allow at most `limit` hits per `window_s`.
    async fn check(&self, key: &str, limit: u32, window_s: u64) -> RateDecision;
    /// Administrative recovery must remain usable when database growth stops.
    async fn check_admin(&self, key: &str, limit: u32, window_s: u64) -> RateDecision {
        self.check(key, limit, window_s).await
    }
    /// Read a temporary abuse lockout without extending it.
    async fn blocked(&self, key: &str) -> Option<u64>;
    /// Record a rejected request; repeated violations trigger a temporary ban.
    async fn penalize(&self, key: &str);
    /// Check `(quota_key, principal, limit)` rules in order for `window_s`.
    /// Returns `Err((true, retry_after_s))` if `principal` is blocked, or
    /// `Err((false, retry_after_s))` if rate-limited.
    async fn admit_rules(&self, rules: &[(String, String, u32)], window_s: u64) -> Result<(), (bool, u64)> {
        for (quota_key, principal, limit) in rules {
            if let Some(retry) = self.blocked(principal).await {
                return Err((true, retry));
            }
            if let RateDecision::Limited { retry_after_s } = self.check(quota_key, *limit, window_s).await {
                self.penalize(principal).await;
                if let Some(retry) = self.blocked(principal).await {
                    return Err((true, retry));
                }
                return Err((false, retry_after_s));
            }
        }
        Ok(())
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait Runtime: MaybeSendSync {
    fn now_ms(&self) -> u64;
    async fn sleep(&self, ms: u64);
}

/// Why a helper program could not be run. Display strings are safe to send
/// to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// The platform has no process support (Workers) or the binary is missing.
    Unavailable,
    Timeout,
    /// The program ran and failed (non-zero exit or unreadable output).
    Failed,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait CommandRunner: MaybeSendSync {
    /// Run `program` with `args`, capture stdout and give up after `timeout_ms`.
    async fn run(&self, program: &str, args: &[String], timeout_ms: u64) -> Result<Vec<u8>, CommandError>;

    /// Resolve a song on the companion machine instead of here: the Worker's
    /// CPU budget cannot afford several provider searches per request.
    /// `None` = no companion or it failed; resolve locally.
    async fn resolve(&self, _req: &glyph_protocol::ResolveRequest) -> Option<glyph_protocol::ResolveResponse> {
        None
    }
}

/// Used where no process can be spawned (Cloudflare Workers).
pub struct NoCommands;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl CommandRunner for NoCommands {
    async fn run(&self, _program: &str, _args: &[String], _timeout_ms: u64) -> Result<Vec<u8>, CommandError> {
        Err(CommandError::Unavailable)
    }
}

/// Everything a request handler needs.
pub struct Services {
    pub http: Arc<dyn HttpClient>,
    pub cache: Arc<dyn Cache>,
    pub limiter: Arc<dyn RateLimiter>,
    pub rt: Arc<dyn Runtime>,
    /// Helper programs (yt-dlp); `NoCommands` where there are none.
    pub cmds: Arc<dyn CommandRunner>,
    pub config: Config,
}

/// Run `fut`, giving up after `ms` milliseconds.
pub async fn with_timeout<T>(rt: &dyn Runtime, ms: u64, fut: impl Future<Output = T>) -> Option<T> {
    use futures::FutureExt;
    let fut = fut.fuse();
    let sleep = rt.sleep(ms).fuse();
    futures::pin_mut!(fut, sleep);
    futures::select! {
        v = fut => Some(v),
        _ = sleep => None,
    }
}
