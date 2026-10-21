//! Identical import pages share native provider work, including after a retry.
use super::*;
use glyph_server_core::import::{PageRequest, PageResponse};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, Weak};

type Gate = tokio::sync::Mutex<()>;

pub(super) async fn page(svc: &Services, request: &PageRequest) -> Result<PageResponse, glyph_server_core::ApiErr> {
    request.validate()?;
    let canonical = glyph_server_core::import::canonical_url(&request.url)
        .ok_or_else(|| glyph_server_core::ApiErr::bad_request("unsupported import link"))?;
    // Keep independent installations/configurations out of one another's groups.
    let key = format!("{:p}:{}:{}:{canonical}", svc, request.max_tracks, request.offset);
    static ACTIVE: OnceLock<Mutex<HashMap<String, Weak<Gate>>>> = OnceLock::new();
    let gate = {
        let mut active = ACTIVE.get_or_init(Default::default).lock().unwrap();
        active.retain(|_, gate| gate.strong_count() > 0);
        match active.get(&key).and_then(Weak::upgrade) {
            Some(gate) => gate,
            None => {
                let gate = Arc::new(Gate::new(()));
                active.insert(key, Arc::downgrade(&gate));
                gate
            }
        }
    };
    let _guard = gate.lock().await;
    // A preceding reader publishes to the canonical page cache before releasing.
    glyph_server_core::import::page(svc, request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyph_server_core::traits::{HttpClient, HttpError, HttpRequest, HttpResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Provider(AtomicUsize);
    #[async_trait::async_trait]
    impl HttpClient for Provider {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let body = if request.url.contains("/tracks?") {
                r#"{"data":[{"id":1,"title":"Song"}],"total":1}"#
            } else { r#"{"title":"Playlist","nb_tracks":1}"# };
            Ok(HttpResponse { status: 200, body: body.as_bytes().to_vec() })
        }
    }

    #[tokio::test]
    async fn simultaneous_share_links_read_the_provider_only_once() {
        let rt: Arc<dyn Runtime> = Arc::new(TokioRuntime);
        let provider = Arc::new(Provider(AtomicUsize::new(0)));
        let svc = Services {
            http: provider.clone(),
            cache: Arc::new(glyph_server_core::cache::MemoryCache::new(rt.clone(), 16)),
            limiter: Arc::new(MemoryRateLimiter::new(rt.clone())),
            rt,
            cmds: Arc::new(glyph_server_core::traits::NoCommands),
            config: Config::new(Default::default()),
        };
        let first = PageRequest { url: "https://www.deezer.com/playlist/1?share=first".into(), offset: 0, max_tracks: 5000 };
        let second = PageRequest { url: "https://www.deezer.com/playlist/1?share=second".into(), ..first.clone() };
        let (a, b) = tokio::join!(page(&svc, &first), page(&svc, &second));
        assert_eq!(a.unwrap().list.tracks[0].title, "Song");
        assert_eq!(b.unwrap().list.tracks[0].title, "Song");
        assert_eq!(provider.0.load(Ordering::SeqCst), 2, "one metadata fetch and one track page, shared by both readers");
    }
}
