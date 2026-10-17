//! Request routing. Host adapters (Worker, axum) convert their native request
//! into [`Request`], call [`Server::handle`], and stream the [`Response`] back.

use crate::admin::host::{AdminHost, NoHost};
use crate::cache::{self, DAY, HOUR};
use crate::config::Config;
use crate::error::ApiErr;
use crate::merge::plain_words;
use crate::params::Query;
use crate::providers::{self, lastfm, lrclib};
use crate::resolver;
use crate::search::{encode_event, search_stream};
use crate::traits::{boxed_stream, BoxStream, Method, RateDecision, Services};
use futures::StreamExt;
use glyph_protocol::{
    HealthResponse, ImportRequest, LyricsResponse, ProviderId, ResolveRequest, SimilarResponse,
    INSTALL_ID_HEADER, PROTOCOL_VERSION,
};
use serde::Serialize;
use std::sync::{Arc, RwLock};

pub const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: Query,
    /// Header names lower-case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Peer address (or a trusted proxy header) as determined by the host.
    pub client_ip: Option<String>,
    /// The connection reached us over HTTPS (directly or via a trusted proxy).
    pub secure: bool,
    /// The caller is on this machine (loopback, no proxy headers).
    pub local: bool,
}

impl Request {
    /// Mark transport properties decided by the host adapter.
    pub fn with_transport(mut self, secure: bool, local: bool) -> Self {
        self.secure = secure;
        self.local = local;
        self
    }

    /// Build from a request target like `/v1/search?q=abc`.
    pub fn new(method: Method, target: &str, headers: Vec<(String, String)>, body: Vec<u8>, client_ip: Option<String>) -> Self {
        let (path, q) = target.split_once('?').unwrap_or((target, ""));
        Self {
            method,
            path: path.to_string(),
            query: Query::parse(q),
            headers: headers.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect(),
            body,
            client_ip,
            secure: false,
            local: false,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub enum Body {
    Bytes(Vec<u8>),
    /// NDJSON stream; each item is one complete line including `\n`.
    Stream(BoxStream<Vec<u8>>),
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub(crate) fn json<T: Serialize>(status: u16, value: &T) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self {
            status,
            headers: base_headers("application/json; charset=utf-8"),
            body: Body::Bytes(body),
        }
    }

    pub fn error(e: ApiErr) -> Self {
        let mut r = Self::json(e.status, &e.body);
        if let Some(s) = e.body.retry_after_s {
            r.headers.push(("retry-after".into(), s.to_string()));
        }
        r
    }

    /// Body bytes for non-streaming responses (tests, simple hosts).
    pub fn bytes(&self) -> Option<&[u8]> {
        match &self.body {
            Body::Bytes(b) => Some(b),
            Body::Stream(_) => None,
        }
    }
}

fn base_headers(content_type: &str) -> Vec<(String, String)> {
    vec![
        ("content-type".into(), content_type.into()),
        ("cache-control".into(), "no-store".into()),
        ("x-content-type-options".into(), "nosniff".into()),
        ("x-glyph-protocol".into(), PROTOCOL_VERSION.to_string()),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Route {
    Health,
    Search,
    Resolve,
    Lyrics,
    Similar,
    Import,
}

fn route(path: &str) -> Option<(Route, Method)> {
    Some(match path.trim_end_matches('/') {
        "/v1/health" => (Route::Health, Method::Get),
        "/v1/search" => (Route::Search, Method::Get),
        "/v1/resolve" => (Route::Resolve, Method::Post),
        "/v1/lyrics" => (Route::Lyrics, Method::Get),
        "/v1/similar" => (Route::Similar, Method::Get),
        "/v1/import/spotify" => (Route::Import, Method::Post),
        _ => return None,
    })
}

#[derive(Clone)]
pub struct Server {
    /// Current services; replaced (not mutated) when admin changes the config.
    svc: Arc<RwLock<Arc<Services>>>,
    pub(crate) host: Arc<dyn AdminHost>,
}

impl Server {
    pub fn new(svc: Services) -> Self {
        Self::from_arc(Arc::new(svc))
    }

    pub fn from_arc(svc: Arc<Services>) -> Self {
        Self { svc: Arc::new(RwLock::new(svc)), host: Arc::new(NoHost) }
    }

    pub fn with_host(mut self, host: Arc<dyn AdminHost>) -> Self {
        self.host = host;
        self
    }

    /// Snapshot of the current services.
    pub fn services(&self) -> Arc<Services> {
        self.svc.read().unwrap().clone()
    }

    /// Swap in a modified config (other services are shared).
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut guard = self.svc.write().unwrap();
        let old = guard.clone();
        let mut config = old.config.clone();
        f(&mut config);
        *guard = Arc::new(Services {
            http: old.http.clone(),
            cache: old.cache.clone(),
            limiter: old.limiter.clone(),
            rt: old.rt.clone(),
            config,
        });
    }

    pub async fn handle(&self, req: Request) -> Response {
        let svc = self.services();
        if req.path.starts_with("/admin/v1") {
            return self.handle_admin(&svc, req).await;
        }
        let Some((route, method)) = route(&req.path) else {
            return Response::error(ApiErr::not_found("unknown endpoint"));
        };
        if req.method == Method::Options {
            return Response { status: 204, headers: vec![("allow".into(), "GET, POST, OPTIONS".into())], body: Body::Bytes(vec![]) };
        }
        if req.method != method {
            let mut r = Response::error(ApiErr::new(405, "method_not_allowed", "wrong HTTP method for this endpoint"));
            r.headers.push(("allow".into(), if method == Method::Get { "GET" } else { "POST" }.into()));
            return r;
        }
        if route != Route::Health {
            if let Err(e) = rate_limit(&svc, &req).await {
                return Response::error(e);
            }
        }
        match dispatch(&svc, route, req).await {
            Ok(r) => r,
            Err(e) => Response::error(e),
        }
    }
}

/// Per install id (`limit`/min) and per IP (4x, since NATs share one).
pub(crate) async fn rate_limit(svc: &Arc<Services>, req: &Request) -> Result<(), ApiErr> {
    {
        let limit = svc.config.rate_limit_per_min.max(1);
        let mut checks: Vec<(String, u32)> = Vec::new();
        if let Some(id) = req.header(INSTALL_ID_HEADER) {
            let ok = !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if ok {
                checks.push((format!("id:{id}"), limit));
            }
        }
        let ip = req.client_ip.as_deref().unwrap_or("unknown");
        let ip_limit = if checks.is_empty() { limit } else { limit.saturating_mul(4) };
        checks.push((format!("ip:{ip}"), ip_limit));
        for (key, lim) in checks {
            if let RateDecision::Limited { retry_after_s } = svc.limiter.check(&key, lim, 60).await {
                return Err(ApiErr::rate_limited(retry_after_s));
            }
        }
        Ok(())
    }
}

async fn dispatch(svc: &Arc<Services>, route: Route, req: Request) -> Result<Response, ApiErr> {
    {
        match route {
            Route::Health => Ok(Response::json(
                200,
                &HealthResponse {
                    server_version: svc.config.server_version.clone(),
                    protocol_version: PROTOCOL_VERSION,
                    providers: svc.config.enabled_providers(),
                },
            )),
            Route::Search => {
                let q = req.query.required("q")?.to_string();
                let stream = search_stream(svc.clone(), q).map(|e| encode_event(&e));
                Ok(Response {
                    status: 200,
                    headers: base_headers("application/x-ndjson; charset=utf-8"),
                    body: Body::Stream(boxed_stream(stream)),
                })
            }
            Route::Resolve => {
                let r: ResolveRequest = parse_body(&req)?;
                if r.track.title.trim().is_empty() || r.track.artist.trim().is_empty() {
                    return Err(ApiErr::bad_request("track.title and track.artist are required"));
                }
                Ok(Response::json(200, &resolver::resolve(svc, r).await))
            }
            Route::Lyrics => lyrics(svc, &req).await,
            Route::Similar => {
                let artist = req.query.required("artist")?;
                let title = req.query.required("title")?;
                let key = format!("similar:{}:{}", plain_words(artist), plain_words(title));
                if let Some(hit) = cache::get_json::<SimilarResponse>(&*svc.cache, &key).await {
                    return Ok(Response::json(200, &hit));
                }
                let tracks = lastfm::similar(svc, artist, title).await.map_err(upstream_err)?;
                let resp = SimilarResponse { tracks };
                cache::put_json(&*svc.cache, &key, &resp, DAY).await;
                Ok(Response::json(200, &resp))
            }
            Route::Import => {
                let r: ImportRequest = parse_body(&req)?;
                let key = format!("import:{}", r.url.trim());
                if let Some(hit) = cache::get_json::<glyph_protocol::ImportResponse>(&*svc.cache, &key).await {
                    return Ok(Response::json(200, &hit));
                }
                let resp = crate::import::import(svc, &r.url).await?;
                cache::put_json(&*svc.cache, &key, &resp, HOUR).await;
                Ok(Response::json(200, &resp))
            }
        }
    }
}

async fn lyrics(svc: &Arc<Services>, req: &Request) -> Result<Response, ApiErr> {
    {
        let title = req.query.required("title")?;
        let artist = req.query.required("artist")?;
        let album = req.query.optional("album")?;
        let dur = req.query.duration_ms("duration")?;
        let key = format!("lyrics:{}:{}:{}", plain_words(artist), plain_words(title), dur.unwrap_or(0) / 2000);
        let cached: Option<Option<LyricsResponse>> = cache::get_json(&*svc.cache, &key).await;
        let found = match cached {
            Some(v) => v,
            None => {
                let v = lrclib::lyrics(svc, title, artist, album, dur).await.map_err(upstream_err)?;
                // Misses are cached shortly so repeated plays do not hammer LRCLIB.
                cache::put_json(&*svc.cache, &key, &v, if v.is_some() { 7 * DAY } else { HOUR }).await;
                v
            }
        };
        match found {
            Some(l) => Ok(Response::json(200, &l)),
            None => Err(ApiErr::not_found("no lyrics found")),
        }
    }
}

pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, ApiErr> {
    if req.body.len() > MAX_BODY_BYTES {
        return Err(ApiErr::new(413, "payload_too_large", "request body too large"));
    }
    serde_json::from_slice(&req.body).map_err(|_| ApiErr::bad_request("request body is not valid JSON for this endpoint"))
}

fn upstream_err(e: providers::ProviderError) -> ApiErr {
    use providers::ProviderError::*;
    match e {
        NotConfigured => ApiErr::new(503, "not_configured", "this server instance has no key for that provider"),
        Quota => ApiErr::new(503, "quota_exhausted", "upstream quota exhausted, try later"),
        other => ApiErr::new(502, "upstream_error", format!("upstream failure: {other}")),
    }
}

#[allow(dead_code)]
fn _provider_ids(_: ProviderId) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;
    use glyph_protocol::{ApiError, SearchEvent};

    fn get(target: &str) -> Request {
        Request::new(Method::Get, target, vec![], vec![], Some("1.2.3.4".into()))
    }

    fn server(h: &Harness) -> Server {
        Server::from_arc(h.svc.clone())
    }

    fn json_of(r: &Response) -> serde_json::Value {
        serde_json::from_slice(r.bytes().unwrap()).unwrap()
    }

    #[test]
    fn health_lists_only_configured_providers() {
        let h = harness(Default::default());
        let r = block_on(server(&h).handle(get("/v1/health")));
        assert_eq!(r.status, 200);
        let hr: HealthResponse = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert_eq!(hr.protocol_version, PROTOCOL_VERSION);
        assert!(hr.providers.contains(&ProviderId::Audius));
        assert!(!hr.providers.contains(&ProviderId::YouTube));

        let h = harness(all_secrets());
        let r = block_on(server(&h).handle(get("/v1/health/")));
        let hr: HealthResponse = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert!(hr.providers.contains(&ProviderId::YouTube) && hr.providers.contains(&ProviderId::LastFm));
        let dump = String::from_utf8_lossy(r.bytes().unwrap()).to_string();
        assert!(!dump.contains("YT_SECRET_KEY"));
    }

    #[test]
    fn routing_errors() {
        let h = harness(Default::default());
        let s = server(&h);
        let r = block_on(s.handle(get("/v1/nope")));
        assert_eq!(r.status, 404);
        let r = block_on(s.handle(get("/v1/resolve")));
        assert_eq!(r.status, 405);
        assert!(r.headers.iter().any(|(k, v)| k == "allow" && v == "POST"));
        let r = block_on(s.handle(get("/v1/search")));
        assert_eq!(r.status, 400);
        let e: ApiError = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert_eq!(e.code, "bad_request");
        let r = block_on(s.handle(Request::new(Method::Post, "/v1/resolve", vec![], b"{nope".to_vec(), None)));
        assert_eq!(r.status, 400);
        let r = block_on(s.handle(get("/v1/lyrics?title=x")));
        assert_eq!(r.status, 400);
        let r = block_on(s.handle(get("/v1/lyrics?title=x&artist=y&duration=abc")));
        assert_eq!(r.status, 400);
    }

    #[test]
    fn search_returns_ndjson_stream() {
        let h = harness(Default::default());
        h.http.on("audius.co", 200, r#"{"data":[{"id":"a","title":"S","duration":10,"user":{"name":"A"}}]}"#);
        h.http.on("deezer.com", 200, r#"{"data":[]}"#);
        h.http.on("advancedsearch", 200, r#"{"response":{"docs":[]}}"#);
        let r = block_on(server(&h).handle(get("/v1/search?q=hello%20world")));
        assert_eq!(r.status, 200);
        assert!(r.headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("application/x-ndjson")));
        let Body::Stream(s) = r.body else { panic!() };
        let lines: Vec<Vec<u8>> = block_on(s.collect());
        assert!(lines.iter().all(|l| l.ends_with(b"\n")));
        let evs: Vec<SearchEvent> = lines.iter().map(|l| serde_json::from_slice(l).unwrap()).collect();
        assert!(matches!(evs.last(), Some(SearchEvent::Done)));
        assert!(evs.iter().any(|e| matches!(e, SearchEvent::Results { provider: ProviderId::Audius, .. })));
    }

    #[test]
    fn rate_limit_per_install_id_and_ip() {
        let mut h = harness(Default::default());
        let svc = Arc::get_mut(&mut h.svc).unwrap();
        svc.config.rate_limit_per_min = 2;
        let s = server(&h);
        let with_id = |id: &str| {
            Request::new(Method::Get, "/v1/lyrics", vec![(INSTALL_ID_HEADER.into(), id.into())], vec![], Some("9.9.9.9".into()))
        };
        // Missing params -> 400, but the hit is still counted.
        assert_eq!(block_on(s.handle(with_id("abc"))).status, 400);
        assert_eq!(block_on(s.handle(with_id("abc"))).status, 400);
        let r = block_on(s.handle(with_id("abc")));
        assert_eq!(r.status, 429);
        let v = json_of(&r);
        assert_eq!(v["code"], "rate_limited");
        assert!(v["retry_after_s"].as_u64().unwrap() >= 1);
        assert!(r.headers.iter().any(|(k, _)| k == "retry-after"));
        // Another install id behind the same IP is still fine (IP limit is 4x).
        assert_eq!(block_on(s.handle(with_id("other"))).status, 400);
        // Health is never rate limited.
        for _ in 0..20 {
            assert_eq!(block_on(s.handle(get("/v1/health"))).status, 200);
        }
        // Window passes -> allowed again.
        h.rt.advance(61_000);
        assert_eq!(block_on(s.handle(with_id("abc"))).status, 400);
    }

    #[test]
    fn lyrics_endpoint_and_negative_cache() {
        let h = harness(Default::default());
        h.http.on("lrclib.net/api/get", 404, "{}");
        h.http.on("lrclib.net/api/search", 200, "[]");
        let s = server(&h);
        let r = block_on(s.handle(get("/v1/lyrics?title=T&artist=A&album=B&duration=200")));
        assert_eq!(r.status, 404);
        let n = h.http.calls.lock().unwrap().len();
        let r = block_on(s.handle(get("/v1/lyrics?title=T&artist=A&album=B&duration=200")));
        assert_eq!(r.status, 404);
        assert_eq!(h.http.calls.lock().unwrap().len(), n);
    }

    #[test]
    fn similar_requires_key_and_never_leaks_it() {
        let h = harness(Default::default());
        let r = block_on(server(&h).handle(get("/v1/similar?artist=a&title=t")));
        assert_eq!(r.status, 503);
        assert_eq!(json_of(&r)["code"], "not_configured");

        let h = harness(all_secrets());
        h.http.on("audioscrobbler", 500, "LF_SECRET in body?");
        let r = block_on(server(&h).handle(get("/v1/similar?artist=a&title=t")));
        assert_eq!(r.status, 502);
        assert!(!String::from_utf8_lossy(r.bytes().unwrap()).contains("LF_SECRET"));
    }

    #[test]
    fn resolve_endpoint_roundtrip() {
        let h = harness(Default::default());
        h.http.on("audius.co", 200, r#"{"data":[{"id":"au1","title":"Get Lucky","duration":369,"user":{"name":"Daft Punk"}}]}"#);
        let body = serde_json::to_vec(&ResolveRequest {
            track: track("x", ProviderId::Deezer, "Get Lucky", "Daft Punk", 369),
            priority: vec![],
        })
        .unwrap();
        let r = block_on(server(&h).handle(Request::new(Method::Post, "/v1/resolve", vec![], body, None)));
        assert_eq!(r.status, 200);
        let rr: glyph_protocol::ResolveResponse = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert_eq!(rr.matched.unwrap().source, ProviderId::Audius);
    }
}
