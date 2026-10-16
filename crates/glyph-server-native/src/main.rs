mod platform;
mod settings;
mod update;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderName, HeaderValue, Method as HMethod, StatusCode};
use axum::response::Response;
use axum::Router;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use glyph_server_core::ratelimit::MemoryRateLimiter;
use glyph_server_core::{Body as CoreBody, Config, Method, Request, Runtime, Server, Services};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "glyph-server", version, about = "Metadata server of the Glyph music app")]
struct Cli {
    /// Address to listen on (default 0.0.0.0:8787)
    #[arg(long)]
    bind: Option<String>,
    /// Path to server.toml (default: ./server.toml if present)
    #[arg(long, env = "GLYPH_CONFIG")]
    config: Option<PathBuf>,
    /// Check GitHub for a newer release and exit
    #[arg(long)]
    check_update: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (default)
    Serve,
    /// Download, verify (SHA-256) and install the latest release
    SelfUpdate,
}

#[derive(Clone)]
struct AppState {
    server: Server,
    trust_proxy: bool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let file = match settings::load_file(cli.config.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot read config: {e}");
            std::process::exit(2);
        }
    };
    let s = settings::Settings::resolve(file, cli.bind);

    if cli.check_update {
        match update::check(&s.update_repo).await {
            Ok(_) => return,
            Err(e) => {
                eprintln!("update check failed: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(Cmd::SelfUpdate) = cli.command {
        if let Err(e) = update::self_update(&s.update_repo).await {
            eprintln!("self-update failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    let rt: Arc<dyn Runtime> = Arc::new(platform::TokioRuntime);
    let mut config = Config::new(s.secrets.clone());
    config.rate_limit_per_min = s.rate_limit_per_min;
    config.youtube_daily_budget_units = s.youtube_daily_budget_units;
    let enabled = config.enabled_providers();
    let services = Services {
        http: Arc::new(platform::ReqwestClient::new()),
        cache: Arc::new(platform::LruDiskCache::new(s.cache_entries, s.cache_dir.clone())),
        limiter: Arc::new(MemoryRateLimiter::new(rt.clone())),
        rt,
        config,
    };
    let state = AppState { server: Server::new(services), trust_proxy: s.trust_proxy };
    let app = Router::new().fallback(handle).with_state(state);

    let listener = match tokio::net::TcpListener::bind(&s.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind {}: {e}", s.bind);
            std::process::exit(1);
        }
    };
    tracing::info!(
        "glyph-server {} listening on {} (providers: {})",
        env!("CARGO_PKG_VERSION"),
        s.bind,
        enabled.iter().map(|p| p.display_name()).collect::<Vec<_>>().join(", ")
    );
    let serve = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        });
    if let Err(e) = serve.await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

fn client_ip(req: &axum::http::Request<Body>, peer: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(v) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = v.split(',').next() {
                let first = first.trim();
                if !first.is_empty() {
                    return first.to_string();
                }
            }
        }
    }
    peer.ip().to_string()
}

async fn handle(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: axum::http::Request<Body>,
) -> Response {
    let ip = client_ip(&req, peer, st.trust_proxy);
    let method = match *req.method() {
        HMethod::GET | HMethod::HEAD => Method::Get,
        HMethod::POST => Method::Post,
        HMethod::OPTIONS => Method::Options,
        _ => Method::Other,
    };
    let target = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    let body = match axum::body::to_bytes(req.into_body(), 128 * 1024).await {
        Ok(b) => b.to_vec(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let resp = st.server.handle(Request::new(method, &target, headers, body, Some(ip))).await;
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
