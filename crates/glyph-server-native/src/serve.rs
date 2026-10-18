//! Run the HTTP(S) server.

use crate::host::NativeHost;
use crate::logbuf::LogBuffer;
use crate::platform::{LruDiskCache, ReqwestClient, TokioCommands, TokioRuntime};
use crate::settings::Settings;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method as HMethod, StatusCode};
use axum::response::Response;
use axum::Router;
use futures::StreamExt;
use glyph_server_core::ratelimit::MemoryRateLimiter;
use glyph_server_core::{Body as CoreBody, Config, Method, Request, Runtime, Server, Services};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
struct AppState {
    server: Server,
    trust_proxy: bool,
    /// We terminate TLS ourselves.
    tls: bool,
}

pub fn build_server(s: &Settings, logs: Arc<LogBuffer>) -> Server {
    let rt: Arc<dyn Runtime> = Arc::new(TokioRuntime);
    let mut config = Config::new(s.secrets.clone());
    config.rate_limit_per_min = s.rate_limit_per_min;
    config.youtube_daily_budget_units = s.youtube_daily_budget_units;
    // YouTube search runs through the local yt-dlp binary.
    config.ytdlp_path = crate::platform::resolve_ytdlp(s.ytdlp_path.as_deref());
    // Clients get /v1/audio/... instead of short-lived provider URLs.
    config.proxy_audio = s.proxy_audio;
    config.server_bearer_hash = s.server_bearer_hash.clone();
    config.import_max_pages = s.import_max_pages;
    config.companion_enabled = !companion_off_flag(s).exists();
    config.admin_password_hash = s
        .admin_password_hash
        .clone()
        .filter(|h| glyph_server_core::admin::auth::is_valid_hash(h));
    let services = Services {
        http: Arc::new(ReqwestClient::new()),
        cache: if companion_mode() {
            // Keep resolved URLs in bounded memory so seeks/refills do not
            // launch yt-dlp again. Audio bytes still live only on Cloudflare;
            // nothing is persisted to the companion's disk.
            Arc::new(LruDiskCache::new(128, None)) as Arc<dyn glyph_server_core::Cache>
        } else {
            Arc::new(LruDiskCache::new(s.cache_entries, s.cache_dir.clone()))
        },
        limiter: Arc::new(MemoryRateLimiter::new(rt.clone())),
        rt,
        cmds: Arc::new(TokioCommands),
        config,
    };
    Server::new(services).with_host(Arc::new(NativeHost {
        config_path: s.config_path.clone(),
        companion_flag: companion_off_flag(s),
        logs,
        started: Instant::now(),
        update_repo: s.update_repo.clone(),
    }))
}

/// Serve until `shutdown` completes.
/// Marker file: while it exists the companion answers 403
/// (`glyph-server companion off` / `on`). Next to the cache, so a Docker
/// volume keeps it across restarts.
/// Companion of a Cloudflare Worker (YTDLP_SERVICE_TOKEN set).
fn companion_mode() -> bool {
    std::env::var("YTDLP_SERVICE_TOKEN").ok().is_some_and(|t| t.len() >= 32)
}

pub fn companion_off_flag(s: &Settings) -> std::path::PathBuf {
    let dir = s.cache_dir.clone().unwrap_or_else(|| s.config_path.parent().map(std::path::Path::to_path_buf).unwrap_or_default());
    dir.join("companion.off")
}

static COMPANION_OFF: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Hover warmups, clicks and encoders for the same video share extraction.
/// The cache is checked again after acquiring the per-video gate.
async fn audio_url(svc: &Services, provider: glyph_protocol::ProviderId, id: &str) -> Result<String, glyph_server_core::providers::ProviderError> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock, Weak};
    static GATES: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    if provider != glyph_protocol::ProviderId::YouTube {
        return glyph_server_core::media::audio_url(svc, provider, id).await;
    }
    let gate = {
        let mut gates = GATES.get_or_init(Default::default).lock().unwrap();
        gates.retain(|_, gate| gate.strong_count() > 0);
        match gates.get(id).and_then(Weak::upgrade) {
            Some(gate) => gate,
            None => {
                let gate = Arc::new(tokio::sync::Mutex::new(()));
                gates.insert(id.to_string(), Arc::downgrade(&gate));
                gate
            }
        }
    };
    let _guard = gate.lock().await;
    glyph_server_core::media::audio_url(svc, provider, id).await
}

/// Checked per request, so switching needs no restart.
fn companion_disabled() -> bool {
    COMPANION_OFF.get().is_some_and(|p| p.exists())
}

pub async fn run(s: Settings, logs: Arc<LogBuffer>, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<(), String> {
    let _ = COMPANION_OFF.set(companion_off_flag(&s));
    if s.admin_password_hash.is_some() && !glyph_server_core::admin::auth::is_valid_hash(s.admin_password_hash.as_deref().unwrap_or("")) {
        tracing::warn!("admin.password_hash is not a valid Argon2id hash: the admin API stays OFF (run `glyph-server setup`)");
    }
    let server = build_server(&s, logs);
    if let Some(program) = server.services().config.ytdlp_path.clone() { crate::ytdlp_pool::warm(program); }
    let enabled = server.services().config.enabled_providers();
    let state = AppState { server, trust_proxy: s.trust_proxy, tls: s.tls.is_some() };
    let app = Router::new().fallback(handle).with_state(state);
    let make = app.into_make_service_with_connect_info::<SocketAddr>();
    let addr: SocketAddr = s
        .bind
        .parse()
        .map_err(|_| format!("`{}` is not a valid address:port (example: 0.0.0.0:8787)", s.bind))?;

    tracing::info!(
        "glyph-server {} listening on {}{} (providers: {}; admin API {})",
        env!("CARGO_PKG_VERSION"),
        if s.tls.is_some() { "https://" } else { "http://" },
        s.bind,
        enabled.iter().map(|p| p.display_name()).collect::<Vec<_>>().join(", "),
        if s.admin_password_hash.is_some() { "on" } else { "off" }
    );

    let handle = axum_server::Handle::new();
    let h2 = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        h2.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });

    match &s.tls {
        None => axum_server::bind(addr).handle(handle).serve(make).await.map_err(|e| bind_error(&s.bind, e)),
        Some(tls) => {
            use rustls_acme::caches::DirCache;
            use rustls_acme::AcmeConfig;
            let mut cfg = AcmeConfig::new([tls.domain.clone()])
                .cache(DirCache::new(tls.cache_dir.clone()))
                .directory_lets_encrypt(!tls.staging);
            if let Some(email) = &tls.email {
                cfg = cfg.contact_push(format!("mailto:{email}"));
            }
            let mut state = cfg.state();
            let acceptor = state.axum_acceptor(state.default_rustls_config());
            tokio::spawn(async move {
                while let Some(ev) = state.next().await {
                    match ev {
                        Ok(ok) => tracing::info!("certificate: {ok:?}"),
                        Err(e) => tracing::error!("certificate error: {e:?} (is port 443 reachable from the internet and does the domain point here?)"),
                    }
                }
            });
            axum_server::bind(addr).acceptor(acceptor).handle(handle).serve(make).await.map_err(|e| bind_error(&s.bind, e))
        }
    }
}

fn bind_error(bind: &str, e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::AddrInUse => format!("{bind} is already in use. Stop the other program or choose another port with --bind."),
        std::io::ErrorKind::PermissionDenied => format!(
            "no permission to listen on {bind}. Ports below 1024 need root/administrator rights (or use a higher port behind a proxy)."
        ),
        _ => format!("server error on {bind}: {e}"),
    }
}

const PROXY_HEADERS: [&str; 5] = ["x-forwarded-for", "forwarded", "x-real-ip", "cf-connecting-ip", "cf-ray"];

fn hv<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// (client ip, secure, local)
pub fn transport(headers: &HeaderMap, peer: SocketAddr, trust_proxy: bool, tls: bool) -> (String, bool, bool) {
    let via_proxy = PROXY_HEADERS.iter().any(|n| headers.contains_key(*n));
    let local = peer.ip().is_loopback() && !via_proxy;
    let mut secure = tls;
    let mut ip = peer.ip().to_string();
    // The compatibility switch trusts only a local reverse proxy. A direct
    // remote caller must never select its own IP or claim HTTPS.
    if trust_proxy && peer.ip().is_loopback() {
        if let Some(v) = hv(headers, "cf-connecting-ip").or_else(|| hv(headers, "x-real-ip")) {
            ip = v.trim().to_string();
        } else if let Some(first) = hv(headers, "x-forwarded-for").and_then(|v| v.split(',').next()) {
            if !first.trim().is_empty() {
                ip = first.trim().to_string();
            }
        }
        if hv(headers, "x-forwarded-proto").map(|p| p.eq_ignore_ascii_case("https")).unwrap_or(false)
            || hv(headers, "cf-visitor").map(|v| v.contains("\"https\"")).unwrap_or(false)
        {
            secure = true;
        }
    }
    ip = ip.parse::<std::net::IpAddr>().unwrap_or(peer.ip()).to_string();
    (ip, secure, local)
}

async fn handle(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: axum::http::Request<Body>) -> Response {
    let (ip, secure, local) = transport(req.headers(), peer, st.trust_proxy, st.tls);
    let method = match *req.method() {
        HMethod::GET | HMethod::HEAD => Method::Get,
        HMethod::POST => Method::Post,
        HMethod::PUT => Method::Put,
        HMethod::OPTIONS => Method::Options,
        HMethod::PATCH => Method::Patch,
        HMethod::DELETE => Method::Delete,
        _ => Method::Other,
    };
    let target = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let path_only = req.uri().path().to_string();
    let log_it = path_only != "/admin/v1/logs";
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    let method_s = req.method().to_string();
    let body = match axum::body::to_bytes(req.into_body(), glyph_server_core::server::max_body_bytes(&path_only)).await {
        Ok(b) => b.to_vec(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    if path_only == "/internal/ytdlp" || path_only.starts_with("/internal/audio/") {
        return companion(&st.server, &path_only, &headers, method, &body).await;
    }
    if let Err(resp) = st.server.check_access(&Request::new(method, &target, headers.clone(), vec![], Some(ip.clone())).with_transport(secure, local)) {
        let mut out = Response::builder().status(resp.status);
        for (k,v) in &resp.headers { out = out.header(k.as_str(),v.as_str()); }
        return out.body(Body::from(resp.bytes().unwrap_or_default().to_vec())).unwrap();
    }
    if std::env::var("YTDLP_SERVICE_TOKEN").ok().is_some_and(|t| t.len() >= 32) && path_only != "/v1/health" {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    // Audio/cover streaming happens here, not in the core router (streamed bytes,
    // byte ranges and provider-side redirects).
    if path_only.starts_with("/v1/audio/") || path_only.starts_with("/v1/download/") {
        return media(&st.server, &target, &headers, method, Some(ip)).await;
    }

    let resp = st
        .server
        .handle(Request::new(method, &target, headers, body, Some(ip)).with_transport(secure, local))
        .await;
    if log_it {
        // Path only: no query (search terms), no client address.
        tracing::info!("{method_s} {path_only} -> {}", resp.status);
    }
    let mut out = Response::builder().status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (k, v) in &resp.headers {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k.as_str()), HeaderValue::try_from(v.as_str())) {
            out = out.header(k, v);
        }
    }
    let body = match resp.body {
        CoreBody::Bytes(b) => Body::from(b),
        CoreBody::Stream(s) => Body::from_stream(s.map(|chunk| Ok::<Bytes, Infallible>(Bytes::from(chunk)))),
    };
    out.body(body).unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "response build failed"))
}

/// Largest audio payload we hand out (a long mix in M4A/AAC).
const MAX_AUDIO_BYTES: u64 = 32 * 1024 * 1024;

/// Concurrency cap for audio transfers, so a few long tracks cannot exhaust
/// the machine (yt-dlp runs here too).
fn audio_slots() -> &'static Arc<tokio::sync::Semaphore> {
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(8)))
}

/// `(provider, id)` of a media path; `None` for anything that is not a
/// well-formed `/v1/audio/<provider>/<id>` or `/v1/download/<provider>/<id>`.
fn audio_target(path: &str) -> Option<(glyph_protocol::ProviderId, String)> {
    let (prefix, id) = if let Some(rest) = path.strip_prefix("/v1/audio/") {
        ("/v1/audio/", rest)
    } else {
        ("/v1/download/", path.strip_prefix("/v1/download/")?)
    };
    debug_assert!(path.starts_with(prefix));
    let (provider, id) = id.split_once('/')?;
    if !glyph_server_core::media::valid_id(id) {
        return None;
    }
    let provider = serde_json::from_value::<glyph_protocol::ProviderId>(serde_json::Value::String(provider.into())).ok()?;
    Some((provider, id.to_string()))
}

/// `/v1/audio/{provider}/{id}` and `/v1/download/{provider}/{id}` for our own
/// clients: the provider URL is resolved here (yt-dlp for YouTube) and the
/// bytes are streamed through, so nothing short-lived ever reaches the app.
async fn media(server: &Server, path: &str, headers: &[(String, String)], method: Method, ip: Option<String>) -> Response {
    use glyph_server_core::media;
    if !matches!(method, Method::Get) {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "GET required");
    }
    let svc = server.services();
    let request = Request::new(method, path, headers.to_vec(), vec![], ip);
    if let Err(e) = server.admit(&request, true).await {
        let r = glyph_server_core::Response::error(e);
        let mut out = Response::builder().status(r.status);
        for (k, v) in &r.headers { out = out.header(k.as_str(), v.as_str()); }
        return out.body(Body::from(r.bytes().unwrap_or_default().to_vec())).unwrap();
    }
    let path = path.split('?').next().unwrap_or(path);
    let is_download = path.starts_with("/v1/download/");
    let Some((provider, id)) = audio_target(path) else {
        return plain(StatusCode::BAD_REQUEST, "invalid audio path");
    };
    if provider == glyph_protocol::ProviderId::YouTube && !is_download {
        if let Some(raw) = request.query.get("quality") {
            let Some(variant) = media::AudioVariant::parse(raw) else { return plain(StatusCode::BAD_REQUEST,"invalid audio quality"); };
            // High already means the best original source. Keep standalone
            // playback working without FFmpeg; lower bitrates need encoding.
            if variant != media::AudioVariant::High {
                let Ok(permit)=audio_slots().clone().try_acquire_owned() else { return plain(StatusCode::TOO_MANY_REQUESTS,"busy, retry later"); };
                let response=companion_variant(&svc,&id,variant,permit,false).await;
                if response.status()!=StatusCode::OK {return response;}
                let range=headers.iter().find(|(k,_)|k=="range").map(|(_,v)|v.as_str());
                return slice_variant(response,range).await;
            }
        }
    }
    let url = if is_download {
        match media::download_url(&svc, provider, &id).await {
            Ok(Some(u)) => u,
            _ => return plain(StatusCode::FORBIDDEN, "download not allowed for this track"),
        }
    } else {
        match audio_url(&svc, provider, &id).await {
            Ok(u) => u,
            Err(_) => return plain(StatusCode::BAD_GATEWAY, "audio source unavailable"),
        }
    };
    let range = headers.iter().find(|(k, _)| k == "range").map(|(_, v)| v.as_str());
    let Ok(permit) = audio_slots().clone().try_acquire_owned() else {
        return plain(StatusCode::TOO_MANY_REQUESTS, "busy, retry later");
    };
    let mut permit = Some(permit);
    let out = fetch_upstream_audio(&url, range, &mut permit).await;
    if provider == glyph_protocol::ProviderId::YouTube && !is_download {
        return retry_stale_youtube(&svc, &id, range, out, &mut permit).await;
    }
    out
}

/// A cached YouTube URL the upstream refuses is dropped and resolved again once.
async fn retry_stale_youtube(
    svc: &glyph_server_core::Services,
    id: &str,
    range: Option<&str>,
    out: Response,
    permit: &mut Option<tokio::sync::OwnedSemaphorePermit>,
) -> Response {
    if !matches!(out.status().as_u16(), 403 | 404) {
        return out;
    }
    glyph_server_core::media::forget_audio_url(svc, id).await;
    match audio_url(svc, glyph_protocol::ProviderId::YouTube, id).await {
        Ok(url) => fetch_upstream_audio(&url, range, permit).await,
        Err(_) => out,
    }
}

/// Streams one audio response from the provider. Byte ranges are forwarded
/// upstream, so a seek only costs the requested bytes.
/// The permit moves into the body only on success, so a retry can reuse it.
async fn fetch_upstream_audio(url: &str, range: Option<&str>, permit: &mut Option<tokio::sync::OwnedSemaphorePermit>) -> Response {
    // Fetch on the extraction host: YouTube signatures can be bound to its IP.
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && attempt.url().scheme() == "https" && attempt.url().host_str().is_some_and(|h| h.ends_with(".googlevideo.com")) { attempt.follow() } else { attempt.stop() }
        })).build()
    {
        Ok(c) => c,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "audio client unavailable"),
    };
    let mut req = client.get(url).header("accept-encoding", "identity");
    if let Some(r) = range.filter(|r| r.len() <= 64 && r.starts_with("bytes=")) {
        req = req.header("range", r);
    }
    let resp = match req.send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = match r.status().as_u16() {
                401 | 403 | 404 => StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                _ => StatusCode::BAD_GATEWAY,
            };
            return plain(status, "audio upstream denied the request");
        }
        Err(_) => return plain(StatusCode::BAD_GATEWAY, "audio upstream unavailable"),
    };
    let total = resp.content_length();
    if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        match resp.headers().get("content-range").and_then(|v| v.to_str().ok()).and_then(glyph_server_core::media::content_range_total) {
            Some(size) if size <= MAX_AUDIO_BYTES => {},
            Some(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "full audio object exceeds size limit"),
            None => return plain(StatusCode::BAD_GATEWAY, "invalid upstream content range"),
        }
    }
    if total.is_some_and(|n| n > MAX_AUDIO_BYTES) {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "audio too large");
    }
    let content_type = resp
        .headers()
        .get("content-type")
        .cloned()
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    if !(content_type.to_str().is_ok_and(|c| c.starts_with("audio/") || c.starts_with("video/") || c.starts_with("application/octet-stream"))) {
        return plain(StatusCode::BAD_GATEWAY, "upstream did not return audio");
    }
    let content_range = resp.headers().get("content-range").cloned();
    let validators: Vec<_> = ["etag", "last-modified"].into_iter().filter_map(|key|
        resp.headers().get(key).cloned().map(|value| (key, value))).collect();
    let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut out = Response::builder()
        .status(if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .header("accept-ranges", "bytes");
    for (key, value) in validators { out = out.header(key, value); }
    if let Some(n) = total {
        out = out.header("content-length", n);
    }
    if let Some(cr) = content_range {
        out = out.header("content-range", cr);
    }
    let permit = permit.take();
    let mut received = 0u64;
    let stream = resp.bytes_stream().map(move |chunk| {
        let _ = &permit;
        let chunk = chunk.map_err(|_| std::io::Error::other("audio transfer failed"))?;
        received += chunk.len() as u64;
        if received > MAX_AUDIO_BYTES {
            return Err(std::io::Error::other("audio too large"));
        }
        Ok(chunk)
    });
    out.body(Body::from_stream(stream)).unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "audio transfer failed"))
}

/// Optional, private companion for the Worker. No arbitrary URLs or tool arguments.
async fn companion(server: &Server, path: &str, headers: &[(String, String)], method: Method, body: &[u8]) -> Response {
    use glyph_server_core::media::{valid_command, CommandRequest, CommandResponse};
    let token = std::env::var("YTDLP_SERVICE_TOKEN").ok().filter(|t| t.len() >= 32);
    let Some(token) = token else { return plain(StatusCode::NOT_FOUND, "not found") };
    let expected = format!("Bearer {token}");
    let supplied = headers.iter().find(|(k,_)| k == "authorization").map(|(_,v)| v.as_bytes()).unwrap_or_default();
    let mut diff = expected.len() ^ supplied.len();
    for (i,b) in expected.bytes().enumerate() { diff |= usize::from(b ^ supplied.get(i).copied().unwrap_or(0)); }
    if diff != 0 { return plain(StatusCode::UNAUTHORIZED, "unauthorized"); }
    // Switched off with `glyph-server companion off` (only the Worker, which
    // holds the token, learns about it).
    if companion_disabled() { return plain(StatusCode::FORBIDDEN, "companion disabled"); }
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let slots = if path.starts_with("/internal/audio/") { audio_slots() }
        else { SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4))) };
    let Ok(_permit) = slots.clone().try_acquire_owned() else { return plain(StatusCode::TOO_MANY_REQUESTS, "busy, retry later") };
    let svc = server.services();
    if let Some(id) = path.strip_prefix("/internal/audio/") {
        if method != Method::Get || !glyph_server_core::media::valid_video_id(id) { return plain(StatusCode::BAD_REQUEST, "invalid video request"); }
        if let Some((_, quality)) = headers.iter().find(|(k,_)| k == "x-glyph-audio-quality") {
            let Some(variant) = glyph_server_core::media::AudioVariant::parse(quality) else { return plain(StatusCode::BAD_REQUEST, "invalid audio quality"); };
            let background = headers.iter().any(|(k,v)| k == "x-glyph-audio-background" && v == "1");
            if background {
                // Finish bounded preparation even when the Worker's short
                // background-request lifetime ends. Output remains cached for
                // its next attempt instead of restarting the full download.
                let (svc, id) = (svc.clone(), id.to_string());
                return tokio::spawn(async move { companion_variant(&svc, &id, variant, _permit, true).await })
                    .await.unwrap_or_else(|_| plain(StatusCode::BAD_GATEWAY, "background encoder failed"));
            }
            return companion_variant(&svc, id, variant, _permit, background).await;
        }
        let url = match audio_url(&svc, glyph_protocol::ProviderId::YouTube, id).await {
            Ok(u) => u, Err(_) => return plain(StatusCode::BAD_GATEWAY, "audio lookup failed"),
        };
        let mut permit = Some(_permit);
        let range = headers.iter().find(|(k, _)| k == "range").map(|(_, v)| v.as_str());
        let out = fetch_upstream_audio(&url, range, &mut permit).await;
        return retry_stale_youtube(&svc, id, range, out, &mut permit).await;
    }
    if method != Method::Post { return plain(StatusCode::METHOD_NOT_ALLOWED, "POST required"); }
    let Ok(command) = serde_json::from_slice::<CommandRequest>(body) else { return plain(StatusCode::BAD_REQUEST, "invalid command") };
    if !valid_command(&command.args) { return plain(StatusCode::BAD_REQUEST, "command denied"); }
    let Some(bin) = svc.config.ytdlp_path.as_deref() else { return plain(StatusCode::SERVICE_UNAVAILABLE, "yt-dlp missing") };
    match svc.cmds.run(bin, &command.args, 20_000).await {
        Ok(bytes) if bytes.len() <= 4 * 1024 * 1024 => {
            let Ok(stdout) = String::from_utf8(bytes) else { return plain(StatusCode::BAD_GATEWAY, "invalid output") };
            let body = serde_json::to_vec(&CommandResponse { stdout }).unwrap();
            Response::builder().header("content-type", "application/json").header("cache-control", "no-store").body(Body::from(body)).unwrap()
        }
        _ => plain(StatusCode::BAD_GATEWAY, "yt-dlp failed"),
    }
}

type VariantCache = std::collections::VecDeque<(String,std::time::Instant,Bytes)>;
fn variant_cache() -> &'static std::sync::Mutex<VariantCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<VariantCache>>=std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}
fn variant_response(bytes:Bytes) -> Response {
    Response::builder().header("content-type","audio/mp4").header("content-length",bytes.len())
        .header("cache-control","no-store").header("accept-ranges","bytes").body(Body::from(bytes)).unwrap()
}
async fn slice_variant(response:Response,range:Option<&str>) -> Response {
    let Some(range)=range else{return response;};
    let Ok(bytes)=axum::body::to_bytes(response.into_body(),MAX_AUDIO_BYTES as usize).await else{return plain(StatusCode::BAD_GATEWAY,"audio unavailable");};
    let total=bytes.len() as u64;
    let Some((start,end))=glyph_server_core::media::byte_range(range,total) else {
        return Response::builder().status(416).header("content-range",format!("bytes */{total}")).body(Body::empty()).unwrap();
    };
    let mut out=variant_response(bytes.slice(start as usize..=end as usize));
    *out.status_mut()=StatusCode::PARTIAL_CONTENT;
    out.headers_mut().insert("content-range",format!("bytes {start}-{end}/{total}").parse().unwrap());
    out
}

/// The authenticated Worker selects a fixed, validated bitrate. Local files
/// allow MP4 fast-start, so a client need not fetch the tail to begin playback.
async fn companion_variant(svc: &Arc<Services>, id: &str, variant: glyph_server_core::media::AudioVariant, permit: tokio::sync::OwnedSemaphorePermit, background: bool) -> Response {
    let cache_key=format!("{id}/{}",variant.name());
    {
        let mut cache=variant_cache().lock().unwrap();
        cache.retain(|(_,at,_)|at.elapsed()<std::time::Duration::from_secs(600));
        if let Some((_,_,bytes))=cache.iter().find(|(key,_,_)|key==&cache_key) {return variant_response(bytes.clone());}
    }
    static BACKGROUND: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let _background = if background {
        match BACKGROUND.get_or_init(|| tokio::sync::Semaphore::new(2)).try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => return plain(StatusCode::TOO_MANY_REQUESTS, "background encoder busy"),
        }
    } else { None };
    static ENCODERS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let Ok(_encoder) = ENCODERS.get_or_init(|| tokio::sync::Semaphore::new(3)).try_acquire() else {
        return plain(StatusCode::TOO_MANY_REQUESTS, "audio encoder busy; retry later");
    };
    let url = match audio_url(svc, glyph_protocol::ProviderId::YouTube, id).await {
        Ok(url) => url, Err(_) => return plain(StatusCode::BAD_GATEWAY,"audio lookup failed"),
    };
    let mut slot = Some(permit);
    let response = fetch_upstream_audio(&url, None, &mut slot).await;
    let response = retry_stale_youtube(svc, id, None, response, &mut slot).await;
    if response.status() != StatusCode::OK { return response; }
    let bytes = match axum::body::to_bytes(response.into_body(), MAX_AUDIO_BYTES as usize).await {
        Ok(bytes) => bytes, Err(_) => return plain(StatusCode::BAD_GATEWAY,"audio source exceeds size limit"),
    };
    // Force a known demuxer rather than allowing playlist auto-detection to
    // introduce arbitrary network URLs or local file references.
    let input_format = if bytes.get(4..8) == Some(b"ftyp") { "mov" }
        else if bytes.starts_with(b"OggS") { "ogg" }
        else if bytes.starts_with(&[0x1a,0x45,0xdf,0xa3]) { "matroska" }
        else { return plain(StatusCode::BAD_GATEWAY,"unsupported audio container"); };
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir, Err(_) => return plain(StatusCode::SERVICE_UNAVAILABLE,"audio workspace unavailable"),
    };
    let input = dir.path().join("source.audio");
    let output = dir.path().join("variant.m4a");
    if tokio::fs::write(&input, &bytes).await.is_err() { return plain(StatusCode::SERVICE_UNAVAILABLE,"audio workspace unavailable"); }
    let ffmpeg = std::env::var("GLYPH_FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into());
    let mut command = tokio::process::Command::new(ffmpeg);
    command.args(["-nostdin","-hide_banner","-loglevel","error","-y","-protocol_whitelist","file,pipe","-f",input_format,"-i"])
        .arg(&input).args(["-map","0:a:0","-vn"]);
    if variant == glyph_server_core::media::AudioVariant::High && input_format == "mov" {
        // Keep the best source AAC rather than introducing another lossy encode.
        command.args(["-c:a","copy"]);
    } else {
        command.args(["-ac","2","-ar","44100","-c:a","aac","-b:a"])
            .arg(format!("{}k",variant.bitrate()));
    }
    command.args(["-threads","1","-movflags","+faststart","-fs","33488896"])
        .arg(&output).kill_on_drop(true)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child, Err(_) => return plain(StatusCode::SERVICE_UNAVAILABLE,"ffmpeg is required for audio quality variants"),
    };
    match tokio::time::timeout(std::time::Duration::from_secs(45),child.wait()).await {
        Ok(Ok(status)) if status.success() => {},
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return plain(StatusCode::BAD_GATEWAY,"audio encoding failed or timed out");
        }
    }
    let size = tokio::fs::metadata(&output).await.map(|m|m.len()).unwrap_or(0);
    // ffmpeg's size cap may stop a long input early. Never advertise that
    // truncated output as a complete song; leave margin for its final MP4 atoms.
    if size == 0 || size >= 33_480_000 { return plain(StatusCode::PAYLOAD_TOO_LARGE,"encoded audio exceeds size limit"); }
    let bytes = match tokio::fs::read(&output).await {
        Ok(bytes) => bytes, Err(_) => return plain(StatusCode::BAD_GATEWAY,"encoded audio unavailable"),
    };
    let bytes=Bytes::from(bytes);
    {
        let mut cache=variant_cache().lock().unwrap();
        cache.retain(|(key,_,_)|key!=&cache_key);
        while cache.len()>=8 || cache.iter().map(|(_,_,v)|v.len()).sum::<usize>()+bytes.len()>64*1024*1024 {if cache.pop_front().is_none(){break;}}
        cache.push_back((cache_key,std::time::Instant::now(),bytes.clone()));
    }
    variant_response(bytes)
}

fn plain(status: StatusCode, msg: &str) -> Response {
    Response::builder().status(status).body(Body::from(msg.to_string())).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn url_resolution_coalesces_duplicates_but_runs_different_songs_in_parallel() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Commands { calls: AtomicUsize, barrier: Option<tokio::sync::Barrier> }
        #[async_trait::async_trait]
        impl glyph_server_core::CommandRunner for Commands {
            async fn run(&self, _: &str, _: &[String], _: u64) -> Result<Vec<u8>, glyph_server_core::CommandError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(barrier) = &self.barrier { barrier.wait().await; }
                else { tokio::time::sleep(std::time::Duration::from_millis(30)).await; }
                Ok(b"https://test.googlevideo.com/audio\n".to_vec())
            }
        }
        let settings = Settings::resolve(Default::default(), None, "test-nonexistent.toml".into());
        let base = build_server(&settings, LogBuffer::new()).services();
        let mut config = base.config.clone(); config.ytdlp_path = Some("fake-yt-dlp".into());
        let commands = Arc::new(Commands { calls: AtomicUsize::new(0), barrier: None });
        let mut svc = Services { http: base.http.clone(), cache: Arc::new(LruDiskCache::new(16, None)),
            limiter: base.limiter.clone(), rt: base.rt.clone(), config, cmds: commands.clone() };
        let (a,b) = tokio::join!(audio_url(&svc, glyph_protocol::ProviderId::YouTube, "sametest001"),
            audio_url(&svc, glyph_protocol::ProviderId::YouTube, "sametest001"));
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(commands.calls.load(Ordering::SeqCst), 1);
        let commands = Arc::new(Commands { calls: AtomicUsize::new(0), barrier: Some(tokio::sync::Barrier::new(2)) });
        svc.cmds = commands.clone();
        let (a,b) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(audio_url(&svc, glyph_protocol::ProviderId::YouTube, "parallel001"),
                audio_url(&svc, glyph_protocol::ProviderId::YouTube, "parallel002"))
        }).await.expect("different videos must not share one global extraction lock");
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(commands.calls.load(Ordering::SeqCst), 2);
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(HeaderName::try_from(*k).unwrap(), HeaderValue::try_from(*v).unwrap());
        }
        h
    }

    #[tokio::test]
    async fn companion_never_exposes_public_audio_routes() {
        struct Restore(Option<String>);
        impl Drop for Restore { fn drop(&mut self) { match &self.0 { Some(v) => std::env::set_var("YTDLP_SERVICE_TOKEN",v), None => std::env::remove_var("YTDLP_SERVICE_TOKEN") } } }
        let _restore = Restore(std::env::var("YTDLP_SERVICE_TOKEN").ok());
        let token = "test-companion-secret-with-at-least-32-characters";
        std::env::set_var("YTDLP_SERVICE_TOKEN",token);
        let settings = Settings::resolve(Default::default(), None, "test-nonexistent.toml".into());
        let state = AppState { server: build_server(&settings,LogBuffer::new()),trust_proxy:false,tls:false };
        // Companion mode must retain URL metadata between range requests.
        // The old NoCache implementation discarded this immediately.
        let cache = state.server.services().cache.clone();
        cache.put("ytaudio:v1:abcdefghijk", b"resolved-url".to_vec(), 60).await;
        assert_eq!(cache.get("ytaudio:v1:abcdefghijk").await, Some(b"resolved-url".to_vec()));
        let peer = "127.0.0.1:5000".parse().unwrap();
        for path in ["/v1/audio/youtube/abcdefghijk", "/v1/download/jamendo/42", "/v1/search?q=test"] {
            let request = axum::http::Request::builder().uri(path).body(Body::empty()).unwrap();
            assert_eq!(handle(State(state.clone()),ConnectInfo(peer),request).await.status(),StatusCode::NOT_FOUND,"{path}");
        }
        let request = axum::http::Request::builder().uri("/v1/health").body(Body::empty()).unwrap();
        assert_eq!(handle(State(state.clone()),ConnectInfo(peer),request).await.status(),StatusCode::OK);
        let request = axum::http::Request::builder().uri("/internal/ytdlp").method("POST").body(Body::from("{}" )).unwrap();
        assert_eq!(handle(State(state.clone()),ConnectInfo(peer),request).await.status(),StatusCode::UNAUTHORIZED);
        let request = axum::http::Request::builder().uri("/internal/ytdlp").method("POST").header("authorization",format!("Bearer {token}")).body(Body::from("{}" )).unwrap();
        assert_eq!(handle(State(state.clone()),ConnectInfo(peer),request).await.status(),StatusCode::BAD_REQUEST);

        // `glyph-server companion off`: 403 for the Worker, `on` restores it.
        let flag = std::env::temp_dir().join(format!("glyph-companion-off-{}", std::process::id()));
        let flag = COMPANION_OFF.get_or_init(|| flag).clone();
        std::fs::write(&flag, b"off\n").unwrap();
        let authed = || axum::http::Request::builder().uri("/internal/ytdlp").method("POST").header("authorization",format!("Bearer {token}")).body(Body::from("{}" )).unwrap();
        assert_eq!(handle(State(state.clone()),ConnectInfo(peer),authed()).await.status(),StatusCode::FORBIDDEN);
        std::fs::remove_file(&flag).unwrap();
        assert_eq!(handle(State(state),ConnectInfo(peer),authed()).await.status(),StatusCode::BAD_REQUEST);
    }

    #[test]
    fn companion_switch_lives_next_to_the_cache() {
        let mut s = Settings::resolve(Default::default(), None, "conf/server.toml".into());
        s.cache_dir = Some("/cache".into());
        assert_eq!(companion_off_flag(&s), std::path::PathBuf::from("/cache").join("companion.off"));
        s.cache_dir = None;
        assert_eq!(companion_off_flag(&s), std::path::PathBuf::from("conf").join("companion.off"));
    }

    #[test]
    fn media_paths_are_parsed_strictly() {
        use glyph_protocol::ProviderId;
        assert_eq!(audio_target("/v1/audio/youtube/abcdefghijk"), Some((ProviderId::YouTube, "abcdefghijk".into())));
        assert_eq!(audio_target("/v1/download/jamendo/42"), Some((ProviderId::Jamendo, "42".into())));
        for bad in [
            "/v1/audio/youtube",
            "/v1/audio/youtube/../../etc/passwd",
            "/v1/audio/nosuchprovider/1",
            "/v1/audio//1",
            "/v1/audio/youtube/abc%2Fdef",
        ] {
            assert_eq!(audio_target(bad), None, "{bad}");
        }
    }

    #[test]
    fn local_means_loopback_without_proxy_headers() {
        let lo: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        assert_eq!(transport(&headers(&[]), lo, false, false), ("127.0.0.1".into(), false, true));
        assert_eq!(transport(&headers(&[]), remote, false, false), ("203.0.113.7".into(), false, false));
        // A tunnel/proxy on localhost must not make remote callers "local".
        let h = headers(&[("x-forwarded-for", "198.51.100.1"), ("x-forwarded-proto", "https")]);
        let (_, secure, local) = transport(&h, lo, true, false);
        assert!(secure && !local);
        let (ip, _, _) = transport(&h, lo, true, false);
        assert_eq!(ip, "198.51.100.1");
    }

    #[test]
    fn forwarded_headers_are_ignored_without_trust_proxy() {
        let remote: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        let h = headers(&[("x-forwarded-for", "1.1.1.1"), ("x-forwarded-proto", "https")]);
        assert_eq!(transport(&h, remote, false, false), ("203.0.113.7".into(), false, false));
        assert_eq!(transport(&h, remote, true, false), ("203.0.113.7".into(), false, false));
        let invalid = headers(&[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(transport(&invalid, "127.0.0.1:1".parse().unwrap(), true, false).0, "127.0.0.1");
        // direct TLS counts as secure
        assert!(transport(&headers(&[]), remote, false, true).1);
        // cloudflare tunnel
        let cf = headers(&[("cf-connecting-ip", "9.9.9.9"), ("cf-visitor", "{\"scheme\":\"https\"}")]);
        let (ip, secure, local) = transport(&cf, "127.0.0.1:1".parse().unwrap(), true, false);
        assert_eq!((ip.as_str(), secure, local), ("9.9.9.9", true, false));
    }
    #[tokio::test]
    async fn encoded_variant_ranges_use_the_selected_representation() {
        let response=slice_variant(variant_response(Bytes::from_static(b"abcdefghij")),Some("bytes=3-5")).await;
        assert_eq!(response.status(),StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-range"],"bytes 3-5/10");
        assert_eq!(response.headers()["content-length"],"3");
        assert_eq!(axum::body::to_bytes(response.into_body(),100).await.unwrap(),Bytes::from_static(b"def"));
        let response=slice_variant(variant_response(Bytes::from_static(b"abc")),Some("bytes=9-10")).await;
        assert_eq!(response.status(),StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()["content-range"],"bytes */3");
    }

}
