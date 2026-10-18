//! Unified search: all providers in parallel (4 s timeout each), results
//! streamed as NDJSON `SearchEvent`s as soon as they arrive.

use crate::cache::{self, MINUTE};
use crate::merge::{plain_words, Merger};
use crate::providers::{self, Candidate, ProviderError};
use crate::traits::{boxed_fut, boxed_stream, with_timeout, BoxStream, Services};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use glyph_protocol::{ProviderId, SearchEvent};
use std::sync::Arc;

/// Providers with errors or exhausted quota are skipped for 10 minutes.
pub const SKIP_TTL_S: u64 = 10 * MINUTE;

pub fn encode_event(ev: &SearchEvent) -> Vec<u8> {
    let mut v = serde_json::to_vec(ev).unwrap_or_else(|_| b"{\"event\":\"done\"}".to_vec());
    v.push(b'\n');
    v
}

fn skip_key(id: ProviderId) -> String {
    format!("skip:{}", id.badge())
}

pub async fn is_skipped(svc: &Services, id: ProviderId) -> bool {
    svc.cache.get(&skip_key(id)).await.is_some()
}

pub async fn mark_skipped(svc: &Services, id: ProviderId) {
    svc.cache.put(&skip_key(id), vec![1], SKIP_TTL_S).await;
}

/// Cache key for a text query, tolerant to case and punctuation.
pub fn query_key(q: &str) -> String {
    plain_words(q)
}

/// Search one provider with cache, cool-off and timeout.
pub async fn search_provider(svc: &Services, id: ProviderId, q: &str) -> Result<Vec<Candidate>, ProviderError> {
    search_provider_page(svc, id, q, 0).await
}

/// [`search_provider`] for result page `page` (0 = first).
pub async fn search_provider_page(svc: &Services, id: ProviderId, q: &str, page: usize) -> Result<Vec<Candidate>, ProviderError> {
    if !svc.config.provider_enabled(id) {
        return Err(ProviderError::NotConfigured);
    }
    if is_skipped(svc, id).await {
        return Err(ProviderError::Skipped);
    }
    let ckey = if page == 0 { format!("search:{}:{}", id.badge(), query_key(q)) } else { format!("search:{}:{}:p{page}", id.badge(), query_key(q)) };
    if let Some(hit) = cache::get_json::<Vec<Candidate>>(&*svc.cache, &ckey).await {
        return Ok(hit);
    }
    let timeout = if id == ProviderId::YouTube { svc.config.provider_timeout_ms.max(22_000) } else { svc.config.provider_timeout_ms };
    let res = match with_timeout(&*svc.rt, timeout, providers::search(svc, id, q, page)).await {
        Some(r) => r,
        None => Err(ProviderError::Timeout),
    };
    match &res {
        Ok(v) => cache::put_json(&*svc.cache, &ckey, v, providers::search_ttl(id)).await,
        Err(_) => mark_skipped(svc, id).await,
    }
    res
}

/// Stream of events: one `Results` per provider that answers (merged with what
/// was already sent), `ProviderError` for failures, then `Done`.
pub fn search_stream(svc: Arc<Services>, q: String) -> BoxStream<SearchEvent> {
    search_stream_page(svc, q, 0)
}

/// [`search_stream`] for result page `page`; later pages ask only the
/// providers that can page.
pub fn search_stream_page(svc: Arc<Services>, q: String, page: usize) -> BoxStream<SearchEvent> {
    let futs = FuturesUnordered::new();
    for id in svc.config.search_providers().into_iter().filter(|id| page == 0 || providers::pages(*id)) {
        let svc = svc.clone();
        let q = q.clone();
        futs.push(boxed_fut(async move {
            let r = search_provider_page(&svc, id, &q, page).await;
            (id, r)
        }));
    }
    let state = (futs, Merger::default(), false);
    let s = futures::stream::unfold(state, |(mut futs, mut merger, done)| async move {
        if done {
            return None;
        }
        loop {
            match futs.next().await {
                None => return Some((SearchEvent::Done, (futs, merger, true))),
                Some((provider, Ok(cands))) => {
                    let tracks = merger.absorb(cands.into_iter().map(|c| c.track).collect());
                    if tracks.is_empty() {
                        continue;
                    }
                    return Some((SearchEvent::Results { provider, tracks }, (futs, merger, false)));
                }
                Some((provider, Err(e))) => {
                    return Some((
                        SearchEvent::ProviderError { provider, message: e.to_string() },
                        (futs, merger, false),
                    ));
                }
            }
        }
    });
    boxed_stream(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;
    use std::sync::atomic::Ordering;

    fn setup() -> Harness {
        let mut s = all_secrets();
        s.soundcloud_client_id = None; // not configured -> not queried
        let h = harness_with_ytdlp(s);
        h.http.on(
            "audius.co/v1/tracks/search",
            200,
            r#"{"data":[{"id":"a1","title":"Song","duration":200,"user":{"name":"Artist"}}]}"#,
        );
        h.http.on(
            "api.deezer.com/search",
            200,
            r#"{"data":[{"id":9,"title":"Song","duration":201,"preview":"https://p","artist":{"name":"Artist"}}]}"#,
        );
        h.http.on("jamendo.com", 500, "boom");
        h.http.on("advancedsearch.php", 200, r#"{"response":{"docs":[]}}"#);
        // yt-dlp is configured but the binary is gone.
        *h.cmds.missing.lock().unwrap() = Some("ytsearch".into());
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on("v1/search", 200, r#"{"tracks":{"items":[]}}"#);
        h
    }

    #[test]
    fn ndjson_line_format() {
        let b = encode_event(&SearchEvent::Done);
        assert_eq!(b, b"{\"event\":\"done\"}\n");
        let line = encode_event(&SearchEvent::ProviderError { provider: ProviderId::YouTube, message: "x".into() });
        assert_eq!(line.iter().filter(|c| **c == b'\n').count(), 1);
        let back: SearchEvent = serde_json::from_slice(&line).unwrap();
        assert!(matches!(back, SearchEvent::ProviderError { provider: ProviderId::YouTube, .. }));
    }

    #[test]
    fn stream_merges_reports_errors_and_ends_with_done() {
        let h = setup();
        let events: Vec<SearchEvent> = block_on(search_stream(h.svc.clone(), "song".into()).collect());
        assert!(matches!(events.last(), Some(SearchEvent::Done)));
        let errors: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SearchEvent::ProviderError { provider, message } => Some((*provider, message.clone())),
                _ => None,
            })
            .collect();
        assert!(errors.iter().any(|(p, _)| *p == ProviderId::Jamendo));
        assert!(errors.iter().any(|(p, m)| *p == ProviderId::YouTube && m.contains("not configured")));
        // Audius and Deezer return the same song: one merged track, ids joined.
        let all: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SearchEvent::Results { tracks, .. } => Some(tracks.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        let last = all.iter().rev().find(|t| t.title == "Song").unwrap();
        // First-seen copy keeps its identity; the other provider is folded into external_ids.
        let mut srcs = last.sources();
        srcs.sort();
        assert_eq!(srcs, vec![ProviderId::Audius, ProviderId::Deezer]);
        // No key material in any event.
        let dump = serde_json::to_string(&events).unwrap();
        for secret in ["YT_SECRET_KEY", "JAM_SECRET", "SP_SECRET", "LF_SECRET"] {
            assert!(!dump.contains(secret));
        }
    }

    #[test]
    fn failing_provider_is_skipped_for_ten_minutes() {
        let h = setup();
        let _ = block_on(search_stream(h.svc.clone(), "song".into()).collect::<Vec<_>>());
        assert_eq!(h.http.call_count("jamendo.com"), 1);
        // New query (cache miss) within 10 min: Jamendo is not called again.
        let ev: Vec<_> = block_on(search_stream(h.svc.clone(), "other".into()).collect());
        assert_eq!(h.http.call_count("jamendo.com"), 1);
        assert!(ev.iter().any(|e| matches!(e, SearchEvent::ProviderError { provider: ProviderId::Jamendo, message } if message.contains("skipped"))));
        h.rt.advance(11 * 60_000);
        let _ = block_on(search_stream(h.svc.clone(), "third".into()).collect::<Vec<_>>());
        assert_eq!(h.http.call_count("jamendo.com"), 2);
    }

    #[test]
    fn provider_timeout_is_reported() {
        let h = setup();
        *h.http.hang_on.lock().unwrap() = Some("audius.co".into());
        h.rt.instant_sleep.store(true, Ordering::SeqCst);
        let r = block_on(search_provider(&h.svc, ProviderId::Audius, "song"));
        assert_eq!(r.unwrap_err(), ProviderError::Timeout);
    }

    #[test]
    fn results_are_cached() {
        let h = setup();
        let _ = block_on(search_provider(&h.svc, ProviderId::Audius, "Song"));
        let _ = block_on(search_provider(&h.svc, ProviderId::Audius, "song!"));
        assert_eq!(h.http.call_count("audius.co"), 1);
    }
}
