//! Run the HTTP(S) server.

use crate::host::NativeHost;
use crate::logbuf::LogBuffer;
use crate::platform::{LruDiskCache, ReqwestClient, TokioRuntime};
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
