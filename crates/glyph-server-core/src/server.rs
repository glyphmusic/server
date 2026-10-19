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
use crate::search::{encode_event, search_stream_page};
use crate::traits::{boxed_stream, BoxStream, Method, RateDecision, Services};
use futures::StreamExt;
use glyph_protocol::{
    HealthResponse, ImportRequest, LyricsResponse, ProviderId, ResolveRequest, SimilarResponse,
    INSTALL_ID_HEADER, PROTOCOL_VERSION,
};
use serde::Serialize;
use std::sync::{Arc, RwLock};

pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Largest request body accepted for `path` (sync pushes and cover uploads are bigger).
pub fn max_body_bytes(path: &str) -> usize {
    crate::accounts::max_body_bytes(path).unwrap_or(MAX_BODY_BYTES)
}

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
    /// Trusted host-derived region/country, never parsed from client headers.
    pub approximate_location: Option<String>,
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
            approximate_location: None,
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
    Cover,
    Download,
    YouTubeHint,
}

fn route(path: &str) -> Option<(Route, Method)> {
    Some(match path.trim_end_matches('/') {
        "/v1/health" => (Route::Health, Method::Get),
        "/v1/search" => (Route::Search, Method::Get),
        "/v1/resolve" => (Route::Resolve, Method::Post),
        "/v1/lyrics" => (Route::Lyrics, Method::Get),
        "/v1/similar" => (Route::Similar, Method::Get),
        "/v1/import/spotify" => (Route::Import, Method::Post),
        "/v1/cover" => (Route::Cover, Method::Post),
        "/v1/download" => (Route::Download, Method::Post),
        "/v1/youtube/hint" => (Route::YouTubeHint, Method::Post),
        _ => return None,
    })
}

#[derive(Clone)]
pub struct Server {
    queue: Arc<crate::admission::Queue>,
    /// Current services; replaced (not mutated) when admin changes the config.
    svc: Arc<RwLock<Arc<Services>>>,
    pub(crate) host: Arc<dyn AdminHost>,
    /// Accounts and sync; `None` = those routes are 404.
    pub(crate) accounts: Option<Arc<crate::accounts::Accounts>>,
}

impl Server {
    pub fn new(svc: Services) -> Self {
        Self::from_arc(Arc::new(svc))
    }

    pub fn from_arc(svc: Arc<Services>) -> Self {
        Self { svc: Arc::new(RwLock::new(svc)), host: Arc::new(NoHost), accounts: None, queue: crate::admission::new_queue() }
    }

    pub fn with_host(mut self, host: Arc<dyn AdminHost>) -> Self {
        self.host = host;
        self
    }

    pub fn with_accounts(mut self, accounts: crate::accounts::Accounts) -> Self {
        self.accounts = Some(Arc::new(accounts));
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
            cmds: old.cmds.clone(),
            config,
        });
    }

    pub async fn handle(&self, req: Request) -> Response {
        if req.path.starts_with("/admin/v1") {
            if let Err(response) = self.check_access(&req) { return response; }
            let svc = self.services();
            let ip = req.client_ip.as_deref().unwrap_or("unknown");
            if let RateDecision::Limited { retry_after_s } = svc.limiter.check_admin(&format!("admin-request:{ip}"), 120, 60).await {
                return Response::error(ApiErr::rate_limited(retry_after_s));
            }
            // handle_admin still validates the admin password/session. Its
            // limiter avoids D1 so administrators can raise a full DB's cap.
            return self.handle_admin(&svc, req).await;
        }
        // Includes unknown routes and account endpoints, before authentication
        // storage lookups. Health remains available for liveness probes.
        let member = if req.path != "/v1/health" && req.method != Method::Options {
            match self.admit(&req, false).await {
                Ok(member) => member,
                Err(e) => return Response::error(e),
            }
        } else { false };
        if let Err(response) = self.check_access(&req) { return response; }
        let svc = self.services();
        if req.path == "/v1/access" && req.method == Method::Get {
            return Response::json(200, &serde_json::json!({"ok": true}));
        }
        if let Some(acc) = self.accounts.as_ref().filter(|_| crate::accounts::owns(req.path.trim_end_matches('/'))) {
            return acc.handle(&svc, req).await;
        }
        if crate::site::owns(req.path.trim_end_matches('/')) {
            return crate::site::handle(&req).unwrap_or_else(Response::error);
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
        let permit = if route != Route::Health {
            match crate::traits::with_timeout(&*svc.rt, 15_000, self.queue.enter(member)).await {
                Some(Ok(p)) => Some(p),
                Some(Err(e)) => return Response::error(e),
                None => {
                    let mut e = ApiErr::unavailable("queue wait timed out; retry later");
                    e.body.retry_after_s = Some(2);
                    return Response::error(e);
                }
            }
        } else { None };
        let features = (self.accounts.is_some(), self.accounts.as_ref().is_some_and(|a| a.blobs.is_some()));
        match dispatch(&svc, route, req, features).await {
            Ok(mut r) => {
                if let Body::Stream(stream) = r.body {
                    r.body = Body::Stream(boxed_stream(futures::stream::unfold((stream, permit), |(mut stream, permit)| async move {
                        stream.next().await.map(|chunk| (chunk, (stream, permit)))
                    })));
                }
                r
            },
            Err(e) => Response::error(e),
        }
    }

    /// Shared by metadata and host-owned media routes. No client trust flags.
    pub async fn admit(&self, req: &Request, media: bool) -> Result<bool, ApiErr> {
        let svc = self.services();
        let ip = req.client_ip.as_deref().unwrap_or("unknown");
        let ip_key = format!("ip:{ip}");
        let namespace = if media { "media" } else { "api" };
        let base = svc.config.rate_limit_per_min.max(1).saturating_mul(if media { 20 } else { 1 });
        let has_bearer = req.header("authorization").is_some_and(|h| h.starts_with("Bearer "));
        if self.accounts.is_some() && has_bearer {
            // Fixed IP ceiling: adding or rotating an install ID never raises it.
            check_rules(&svc, &[(format!("{namespace}:{ip_key}"), ip_key.clone(), base.saturating_mul(8))]).await?;
            if let Some(a) = &self.accounts {
                if let Some((user, device)) = a.abuse_identity(&svc, req).await {
                    let u_key = format!("user:{user}");
                    let d_key = format!("device:{device}");
                    let lim = base.saturating_mul(4);
                    check_rules(&svc, &[
                        (format!("{namespace}:{u_key}"), u_key, lim),
                        (format!("{namespace}:{d_key}"), d_key, lim),
                    ]).await?;
                    return Ok(true);
                }
            }
            let mut rules = vec![(format!("{namespace}:guest:{ip}"), ip_key, base)];
            if let Some(id) = req.header(INSTALL_ID_HEADER).filter(|id| !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')) {
                let key = format!("install:{id}");
                rules.push((format!("{namespace}:{key}"), key, base));
            }
            check_rules(&svc, &rules).await?;
            Ok(false)
        } else {
            let mut rules = vec![
                (format!("{namespace}:{ip_key}"), ip_key.clone(), base.saturating_mul(8)),
                (format!("{namespace}:guest:{ip}"), ip_key, base),
            ];
            if let Some(id) = req.header(INSTALL_ID_HEADER).filter(|id| !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')) {
                let key = format!("install:{id}");
                rules.push((format!("{namespace}:{key}"), key, base));
            }
            check_rules(&svc, &rules).await?;
            Ok(false)
        }
    }
}

async fn check_rules(svc: &Services, rules: &[(String, String, u32)]) -> Result<(), ApiErr> {
    if let Err((banned, retry)) = svc.limiter.admit_rules(rules, 60).await {
        let mut e = ApiErr::rate_limited(retry);
        if banned {
            e.body.code = "temporarily_blocked".into();
            e.body.message = "temporary abuse protection; retry later".into();
        }
        return Err(e);
    }
    Ok(())
}

/// Cover art for `track`: its own, else Deezer by ISRC, else a Deezer search hit.
pub async fn find_cover(svc: &Services, track: &glyph_protocol::Track) -> Option<String> {
    if let Some(c) = track.cover_url.clone().filter(|u| !u.is_empty()) {
        return Some(c);
    }
    if let Some(isrc) = track.isrc.as_deref() {
        let c = crate::traits::with_timeout(&*svc.rt, svc.config.provider_timeout_ms, providers::deezer::by_isrc(svc, isrc)).await
            .and_then(Result::ok).flatten().and_then(|c| c.track.cover_url);
        if c.is_some() {
            return c;
        }
    }
    let query = format!("{} {}", track.artist, track.title);
    crate::search::search_provider(svc, ProviderId::Deezer, &query).await.ok()
        .and_then(|hits| hits.into_iter().find(|c| crate::merge::artists_match(&track.artist, &c.track.artist) && crate::merge::normalize_title(&track.title) == crate::merge::normalize_title(&c.track.title)))
        .and_then(|c| c.track.cover_url)
}

async fn dispatch(svc: &Arc<Services>, route: Route, req: Request, (accounts, covers): (bool, bool)) -> Result<Response, ApiErr> {
    {
        match route {
            Route::Health => Ok(Response::json(
                200,
                &HealthResponse {
                    server_auth_required: svc.config.server_bearer_hash.is_some(),
                    server_version: svc.config.server_version.clone(),
                    protocol_version: PROTOCOL_VERSION,
                    providers: svc.config.enabled_providers(),
                    accounts,
                    covers,
                },
            )),
            Route::Search => {
                let q = req.query.required("q")?.to_string();
                // `page` (0-based) loads further results while scrolling.
                let page = req.query.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(0).min(20);
                let stream = search_stream_page(svc.clone(), q, page).map(|e| encode_event(&e));
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
                let wanted = r.track.clone();
                let mut resolved = resolver::resolve(svc, r).await;
                if svc.config.proxy_audio { crate::media::proxy_playback(&mut resolved); }
                // Not part of the cached answer: hints change independently.
                resolved.youtube_hint = crate::hints::lookup(svc, &wanted).await;
                Ok(Response::json(200, &resolved))
            }
            Route::YouTubeHint => {
                let r: glyph_protocol::YouTubeHintRequest = parse_body(&req)?;
                let voter = req
                    .header(INSTALL_ID_HEADER)
                    .filter(|id| !id.is_empty() && id.len() <= 64)
                    .map(|id| format!("id:{id}"))
                    .or_else(|| req.client_ip.as_ref().map(|ip| format!("ip:{ip}")))
                    .ok_or_else(|| ApiErr::bad_request("missing install id"))?;
                Ok(Response::json(200, &crate::hints::report(svc, &voter, r).await?))
            }
            Route::Cover => {
                let track: glyph_protocol::Track = parse_body(&req)?;
                let cover = find_cover(svc, &track).await;
                Ok(Response::json(200, &serde_json::json!({ "url": cover })))
            }
            Route::Download => {
                #[derive(serde::Deserialize)]
                #[serde(untagged)]
                enum Input { Resolve(ResolveRequest), Track(glyph_protocol::Track) }
                let request = match parse_body::<Input>(&req)? {
                    Input::Resolve(r) => r,
                    Input::Track(track) => ResolveRequest { youtube_only: false, track, priority: vec![] },
                };
                if request.track.title.trim().is_empty() || request.track.artist.trim().is_empty() {
                    return Err(ApiErr::bad_request("track.title and track.artist are required"));
                }
                let youtube_only = request.youtube_only;
                let track = request.track.clone();
                let resolved = resolver::resolve(svc, request).await;
                if matches!(resolved.source.as_ref(), Some(glyph_protocol::PlaybackSource::Embed { provider: ProviderId::YouTube, .. } | glyph_protocol::PlaybackSource::Stream { provider: ProviderId::YouTube, .. })) {
                    if let Some(matched) = &resolved.matched {
                        if crate::media::valid_video_id(&matched.id) {
                            return Ok(Response::json(200, &serde_json::json!({"provider":ProviderId::YouTube,
                                "url":crate::media::audio_path(ProviderId::YouTube, &matched.id).replace("/v1/audio/", "/v1/download/")})));
                        }
                    }
                }
                if youtube_only { return Ok(Response::json(200, &serde_json::Value::Null)); }
                for provider in [ProviderId::Jamendo, ProviderId::Audius, ProviderId::Archive] {
                    for t in std::iter::once(&track).chain(resolved.matched.iter()) {
                        let id = if t.source == provider { Some(&t.id) } else { t.external_ids.get(&provider) };
                        if let Some(id) = id {
                            if let Some(Ok(Some(url))) = crate::traits::with_timeout(&*svc.rt, svc.config.provider_timeout_ms, crate::media::download_url(svc, provider, id)).await {
                                let url = if svc.config.proxy_audio { crate::media::audio_path(provider, id).replace("/v1/audio/", "/v1/download/") } else { url };
                                return Ok(Response::json(200, &serde_json::json!({"provider":provider,"url":url})));
                            }
                        }
                    }
                }
                Ok(Response::json(200, &serde_json::Value::Null))
            }
            Route::Lyrics => lyrics(svc, &req).await,
            Route::Similar => {
                let artist = req.query.required("artist")?;
                let title = req.query.required("title")?;
                let key = format!("similar:{}:{}", plain_words(artist), plain_words(title));
                if let Some(hit) = cache::get_json::<SimilarResponse>(&*svc.cache, &key).await {
                    return Ok(Response::json(200, &hit));
                }
                // Radio is a bonus feature: a missing Last.fm key or a short
                // upstream outage answers with an empty list instead of a 5xx,
                // and the client keeps its own fallbacks.
                let (tracks, ttl) = match lastfm::similar(svc, artist, title).await {
                    Ok(t) => (t, DAY),
                    Err(_) => (Vec::new(), SIMILAR_TROUBLE_TTL),
                };
                let resp = SimilarResponse { tracks };
                cache::put_json(&*svc.cache, &key, &resp, ttl).await;
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

/// How long a failed lyrics lookup is remembered. Short: a rate-limited
/// minute must not hide lyrics for an hour.
const LYRICS_TROUBLE_TTL: u64 = 300;

/// How long an empty radio answer is remembered after a provider problem.
const SIMILAR_TROUBLE_TTL: u64 = 600;

/// Tries the companion first (home connection, not rate-limited by LRCLIB),
/// then the local lookup. Upstream trouble yields "no lyrics", never a 5xx.
async fn lyrics(svc: &Arc<Services>, req: &Request) -> Result<Response, ApiErr> {
    {
        let title = req.query.required("title")?;
        let artist = req.query.required("artist")?;
        let album = req.query.optional("album")?;
        let dur = req.query.duration_ms("duration")?;
        let key = format!("lyrics:{}:{}:{}", plain_words(artist), plain_words(title), dur.unwrap_or(0) / 2000);
        let cached: Option<Option<LyricsResponse>> = cache::get_json(&*svc.cache, &key).await;
        let (found, ttl) = match cached {
            Some(v) => (v, 0),
            None => {
                let query = crate::traits::LyricsQuery {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.map(str::to_string),
                    duration_ms: dur,
                };
                match svc.cmds.lyrics(&query).await {
                    Some(crate::traits::LyricsLookup::Found(l)) => (Some(l), 7 * DAY),
                    // The companion answered: no lyrics for this song.
                    Some(crate::traits::LyricsLookup::None) => (None, HOUR),
                    None => match lrclib::lyrics(svc, title, artist, album, dur).await {
                        Ok(Some(l)) => (Some(l), 7 * DAY),
                        // Misses are cached shortly so repeated plays do not hammer LRCLIB.
                        Ok(None) => (None, HOUR),
                        // Rate limits and outages are expected for data-centre
                        // addresses; stay quiet and retry soon.
                        Err(_) => (None, LYRICS_TROUBLE_TTL),
                    },
                }
            }
        };
        if ttl > 0 {
            cache::put_json(&*svc.cache, &key, &found, ttl).await;
        }
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

        // A key alone is not enough any more: YouTube search runs through yt-dlp.
        let h = harness(all_secrets());
        let r = block_on(server(&h).handle(get("/v1/health/")));
        let hr: HealthResponse = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert!(!hr.providers.contains(&ProviderId::YouTube) && hr.providers.contains(&ProviderId::LastFm));

        let h = harness_with_ytdlp(all_secrets());
        let r = block_on(server(&h).handle(get("/v1/health")));
        let hr: HealthResponse = serde_json::from_slice(r.bytes().unwrap()).unwrap();
        assert!(hr.providers.contains(&ProviderId::YouTube));
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
        // Rotating an untrusted install ID does not bypass the guest IP quota.
        assert_eq!(block_on(s.handle(with_id("other"))).status, 429);
        // Health is never rate limited.
        for _ in 0..20 {
            assert_eq!(block_on(s.handle(get("/v1/health"))).status, 200);
        }
        // Window passes -> allowed again.
        h.rt.advance(61_000);
        assert_eq!(block_on(s.handle(with_id("abc"))).status, 400);
    }

    #[test]
    fn repeated_abuse_bans_ip_across_media_and_metadata_then_expires() {
        let h = harness(Default::default());
        let s = server(&h);
        s.update_config(|c| c.rate_limit_per_min = 1);
        let req = Request::new(Method::Get, "/v1/lyrics", vec![], vec![], Some("abusive".into()));
        block_on(async {
            assert!(!s.admit(&req, false).await.unwrap());
            for _ in 0..12 { assert!(s.admit(&req, false).await.is_err()); }
            let e = s.admit(&req, true).await.unwrap_err();
            assert_eq!(e.body.code, "temporarily_blocked");
            assert_eq!(e.body.retry_after_s, Some(900));
            let other = Request::new(Method::Get, "/v1/lyrics", vec![], vec![], Some("innocent".into()));
            assert!(!s.admit(&other, false).await.unwrap());
            h.rt.advance(900_000);
            assert!(!s.admit(&req, false).await.unwrap());
        });
    }

    #[test]
    fn unconsumed_search_stream_holds_slot_until_disconnect() {
        use futures::FutureExt;
        let h = harness(Default::default());
        let s = server(&h);
        let mut responses: Vec<_> = (0..8).map(|_| block_on(s.handle(get("/v1/search?q=hello")))).collect();
        assert!(responses.iter().all(|r| r.status == 200));
        let mut waiting = Box::pin(s.handle(get("/v1/search?q=hello")));
        assert!(waiting.as_mut().now_or_never().is_none());
        drop(responses.pop());
        assert_eq!(waiting.as_mut().now_or_never().unwrap().status, 200);
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
    fn lyrics_rate_limits_answer_no_lyrics_instead_of_5xx() {
        // LRCLIB blocks data-centre ranges (429/5xx): the client must see a
        // missing-lyrics answer, never a 502/503 that also pollutes logs.
        for status in [429u16, 500, 503] {
            let h = harness(Default::default());
            h.http.on("lrclib.net", status, "{}");
            let s = server(&h);
            let r = block_on(s.handle(get("/v1/lyrics?title=T&artist=A&duration=200")));
            assert_eq!(r.status, 404, "upstream {status} must not surface as 5xx");
        }
    }

    #[test]
    fn lyrics_prefer_the_companion_and_skip_the_local_lookup() {
        let h = harness(Default::default());
        h.http.on("lrclib.net", 429, "{}");
        h.cmds.companion_lyrics(Some(glyph_protocol::LyricsResponse {
            provider: ProviderId::LrcLib,
            synced: vec![glyph_protocol::LyricLine { time_ms: 1_000, text: "from companion".into() }],
            plain: None,
            instrumental: false,
        }));
        let s = server(&h);
        let r = block_on(s.handle(get("/v1/lyrics?title=T&artist=A&duration=200")));
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r)["synced"][0]["text"], "from companion");
        // The rate-limited local provider was never asked.
        assert!(h.http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn lyrics_companion_trouble_falls_back_to_the_local_lookup() {
        let h = harness(Default::default());
        h.http.on("lrclib.net/api/get", 404, "{}");
        h.http.on("lrclib.net/api/search", 429, "{}");
        let s = server(&h);
        // No companion answer and a rate-limited lookup: still not a 5xx.
        let r = block_on(s.handle(get("/v1/lyrics?title=T&artist=A&duration=200")));
        assert_eq!(r.status, 404);
    }

    #[test]
    fn similar_without_key_is_an_empty_list_not_an_error() {
        // Radio is optional: an unconfigured provider and a broken upstream
        // both answer with an empty list so the client never sees a 5xx.
        let h = harness(Default::default());
        let r = block_on(server(&h).handle(get("/v1/similar?artist=a&title=t")));
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r)["tracks"].as_array().unwrap().len(), 0);

        let h = harness(all_secrets());
        h.http.on("audioscrobbler", 500, "LF_SECRET in body?");
        let r = block_on(server(&h).handle(get("/v1/similar?artist=a&title=t")));
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r)["tracks"].as_array().unwrap().len(), 0);
        assert!(!String::from_utf8_lossy(r.bytes().unwrap()).contains("LF_SECRET"));
    }

    #[test]
    fn youtube_download_resolves_metadata_tracks_to_a_proxy_stream() {
        let h = crate::testutil::harness_with_ytdlp(Default::default());
        h.cmds.on("ytsearch", r#"{"id":"abcdefghijk","title":"Get Lucky","channel":"Daft Punk - Topic","duration":369}"#);
        let track = track("dz1", ProviderId::Deezer, "Get Lucky", "Daft Punk", 369);
        for body in [serde_json::to_vec(&track).unwrap(), serde_json::to_vec(&ResolveRequest {
            youtube_only: true, track: track.clone(), priority: vec![ProviderId::YouTube],
        }).unwrap()] {
            let response = block_on(server(&h).handle(Request::new(Method::Post, "/v1/download", vec![], body, None)));
            assert_eq!(response.status, 200);
            let value: serde_json::Value = serde_json::from_slice(response.bytes().unwrap()).unwrap();
            assert_eq!(value["provider"], "youtube");
            assert_eq!(value["url"], "/v1/download/youtube/abcdefghijk");
        }
    }

    #[test]
    fn resolve_endpoint_roundtrip() {
        let h = harness(Default::default());
        h.http.on("audius.co", 200, r#"{"data":[{"id":"au1","title":"Get Lucky","duration":369,"user":{"name":"Daft Punk"}}]}"#);
        let body = serde_json::to_vec(&ResolveRequest { youtube_only: false,
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
