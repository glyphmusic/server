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
    config.import_max_pages = s.import_max_pages;
    config.admin_password_hash = s
        .admin_password_hash
        .clone()
        .filter(|h| glyph_server_core::admin::auth::is_valid_hash(h));
    let services = Services {
        http: Arc::new(ReqwestClient::new()),
        cache: Arc::new(LruDiskCache::new(s.cache_entries, s.cache_dir.clone())),
        limiter: Arc::new(MemoryRateLimiter::new(rt.clone())),
        rt,
        cmds: Arc::new(TokioCommands),
        config,
    };
    Server::new(services).with_host(Arc::new(NativeHost {
        config_path: s.config_path.clone(),
        logs,
        started: Instant::now(),
        update_repo: s.update_repo.clone(),
    }))
}

/// Serve until `shutdown` completes.
pub async fn run(s: Settings, logs: Arc<LogBuffer>, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<(), String> {
    if s.admin_password_hash.is_some() && !glyph_server_core::admin::auth::is_valid_hash(s.admin_password_hash.as_deref().unwrap_or("")) {
        tracing::warn!("admin.password_hash is not a valid Argon2id hash: the admin API stays OFF (run `glyph-server setup`)");
    }
    let server = build_server(&s, logs);
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
    if trust_proxy {
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
    let body = match axum::body::to_bytes(req.into_body(), 128 * 1024).await {
        Ok(b) => b.to_vec(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    if path_only == "/internal/ytdlp" || path_only.starts_with("/internal/audio/") {
        return companion(&st.server, &path_only, &headers, method, &body).await;
    }
    // Audio/cover streaming happens here, not in the core router (streamed bytes,
    // byte ranges and provider-side redirects).
    if path_only.starts_with("/v1/audio/") || path_only.starts_with("/v1/download/") {
        return media(&st.server, &path_only, &headers, method, Some(ip)).await;
    }
    // A companion must not expose the ordinary unauthenticated search routes as a bypass.
    if std::env::var("YTDLP_SERVICE_TOKEN").ok().is_some_and(|t| t.len() >= 32) && path_only != "/v1/health" {
        return plain(StatusCode::NOT_FOUND, "not found");
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
    SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
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
    // Seeking makes many requests; audio gets the same limit with room to spare.
    let key = format!("media:ip:{}", ip.as_deref().unwrap_or("unknown"));
    if let glyph_server_core::RateDecision::Limited { retry_after_s } = svc
        .limiter
        .check(&key, svc.config.rate_limit_per_min.max(1).saturating_mul(20), 60)
        .await
    {
        let mut out = plain(StatusCode::TOO_MANY_REQUESTS, "too many audio requests");
        if let Ok(v) = HeaderValue::from_str(&retry_after_s.to_string()) {
            out.headers_mut().insert("retry-after", v);
        }
        return out;
    }
    let is_download = path.starts_with("/v1/download/");
    let Some((provider, id)) = audio_target(path) else {
        return plain(StatusCode::BAD_REQUEST, "invalid audio path");
    };
    let url = if is_download {
        match media::download_url(&svc, provider, &id).await {
            Ok(Some(u)) => u,
            _ => return plain(StatusCode::FORBIDDEN, "download not allowed for this track"),
        }
    } else {
        match media::audio_url(&svc, provider, &id).await {
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
    match glyph_server_core::media::audio_url(svc, glyph_protocol::ProviderId::YouTube, id).await {
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
    let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut out = Response::builder()
        .status(if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .header("accept-ranges", "bytes");
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
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)));
    let Ok(_permit) = slots.clone().try_acquire_owned() else { return plain(StatusCode::TOO_MANY_REQUESTS, "busy, retry later") };
    let svc = server.services();
    if let Some(id) = path.strip_prefix("/internal/audio/") {
        if method != Method::Get || !glyph_server_core::media::valid_video_id(id) { return plain(StatusCode::BAD_REQUEST, "invalid video request"); }
        let url = match glyph_server_core::media::audio_url(&svc, glyph_protocol::ProviderId::YouTube, id).await {
            Ok(u) => u, Err(_) => return plain(StatusCode::BAD_GATEWAY, "audio lookup failed"),
        };
        let mut permit = Some(_permit);
        let out = fetch_upstream_audio(&url, None, &mut permit).await;
        return retry_stale_youtube(&svc, id, None, out, &mut permit).await;
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

fn plain(status: StatusCode, msg: &str) -> Response {
    Response::builder().status(status).body(Body::from(msg.to_string())).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(HeaderName::try_from(*k).unwrap(), HeaderValue::try_from(*v).unwrap());
        }
        h
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
        // direct TLS counts as secure
        assert!(transport(&headers(&[]), remote, false, true).1);
        // cloudflare tunnel
        let cf = headers(&[("cf-connecting-ip", "9.9.9.9"), ("cf-visitor", "{\"scheme\":\"https\"}")]);
        let (ip, secure, local) = transport(&cf, "127.0.0.1:1".parse().unwrap(), true, false);
        assert_eq!((ip.as_str(), secure, local), ("9.9.9.9", true, false));
    }
}
