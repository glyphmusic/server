//! Mocks for unit tests.

use crate::cache::MemoryCache;
use crate::config::{Config, Secrets};
use crate::ratelimit::MemoryRateLimiter;
use crate::traits::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct MockRuntime {
    now: AtomicU64,
    /// When true, `sleep` completes immediately (to trigger timeouts).
    pub instant_sleep: std::sync::atomic::AtomicBool,
}

impl MockRuntime {
    pub fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Runtime for MockRuntime {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    async fn sleep(&self, _ms: u64) {
        if !self.instant_sleep.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}

/// Routes: first rule whose `needle` is contained in the URL wins.
#[derive(Default)]
pub struct MockHttp {
    pub rules: Mutex<Vec<(String, u16, String)>>,
    pub calls: Mutex<Vec<String>>,
    /// URLs containing this never answer (for timeout tests).
    pub hang_on: Mutex<Option<String>>,
}

impl MockHttp {
    pub fn on(&self, needle: &str, status: u16, body: &str) -> &Self {
        self.rules.lock().unwrap().push((needle.into(), status, body.into()));
        self
    }
    pub fn call_count(&self, needle: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|u| u.contains(needle)).count()
    }
}

#[async_trait::async_trait]
impl HttpClient for MockHttp {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.calls.lock().unwrap().push(req.url.clone());
        let hang = self.hang_on.lock().unwrap().clone();
        if let Some(h) = hang {
            if req.url.contains(&h) {
                std::future::pending::<()>().await;
            }
        }
        let rules = self.rules.lock().unwrap();
        for (needle, status, body) in rules.iter() {
            if req.url.contains(needle.as_str()) {
                return Ok(HttpResponse { status: *status, body: body.clone().into_bytes() });
            }
        }
        Ok(HttpResponse { status: 404, body: b"{}".to_vec() })
    }
}

pub struct Harness {
    pub http: Arc<MockHttp>,
    pub rt: Arc<MockRuntime>,
    pub svc: Arc<Services>,
}

pub fn all_secrets() -> Secrets {
    Secrets {
        youtube_api_key: Some("YT_SECRET_KEY".into()),
        jamendo_client_id: Some("JAM_SECRET".into()),
        soundcloud_client_id: Some("SC_ID".into()),
        soundcloud_client_secret: Some("SC_SECRET".into()),
        spotify_client_id: Some("SP_ID".into()),
        spotify_client_secret: Some("SP_SECRET".into()),
        lastfm_api_key: Some("LF_SECRET".into()),
    }
}

pub fn harness(secrets: Secrets) -> Harness {
    let http = Arc::new(MockHttp::default());
    let rt = Arc::new(MockRuntime::default());
    let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new(rt.clone(), 1000));
    let svc = Arc::new(Services {
        http: http.clone(),
        cache,
        limiter: Arc::new(MemoryRateLimiter::new(rt.clone())),
        rt: rt.clone(),
        config: Config::new(secrets),
    });
    Harness { http, rt, svc }
}

pub fn track(id: &str, source: glyph_protocol::ProviderId, title: &str, artist: &str, secs: u64) -> glyph_protocol::Track {
    glyph_protocol::Track {
        id: id.into(),
        source,
        title: title.into(),
        artist: artist.into(),
        album: None,
        duration_ms: Some(secs * 1000),
        cover_url: None,
        isrc: None,
        permalink: None,
        external_ids: Default::default(),
    }
}
