//! Resolver: find the best legal full-playback source for a track.
//!
//! Order: ISRC first, then "Artist - Title" text search at each full source in
//! priority order (default Audius -> Jamendo -> SoundCloud -> YouTube). Hits are
//! scored by title, artist, duration (+-3 s) and hints such as "Official Audio"
//! or YouTube "Topic" channels. Without a full source a Deezer 30 s preview is
//! returned. Mappings are cached (30 days; misses for 1 hour).

use crate::cache::{self, DAY, HOUR};
use crate::merge::{self, artists_match, durations_match, merge_into, normalize_title, plain_words, word_similarity};
use crate::providers::{self, deezer, Candidate};
use crate::search::{is_skipped, search_provider};
use crate::traits::{with_timeout, Services};
use glyph_protocol::{PlaybackSource, ProviderId, ResolveRequest, ResolveResponse, Track};

pub const DEFAULT_PRIORITY: [ProviderId; 4] =
    [ProviderId::Audius, ProviderId::Jamendo, ProviderId::SoundCloud, ProviderId::YouTube];

/// Minimum score for a text-search hit to count as the same song.
pub const ACCEPT_SCORE: f32 = 0.7;

/// 0.0..=1.0 similarity between the wanted track and a candidate.
pub fn score(want: &Track, cand: &Candidate) -> f32 {
    let c = &cand.track;
    if let (Some(a), Some(b)) = (&want.isrc, &c.isrc) {
        if !a.is_empty() && a.eq_ignore_ascii_case(b) {
            return 1.0;
        }
    }
    let title = {
        let (a, b) = (normalize_title(&want.title), normalize_title(&c.title));
        if a == b && !a.is_empty() {
            1.0
        } else {
            word_similarity(&a, &b)
        }
    };
    let artist = if artists_match(&want.artist, &c.artist) {
        1.0
    } else {
        word_similarity(&plain_words(&want.artist), &plain_words(&c.artist))
    };
    let duration = match (want.duration_ms, c.duration_ms) {
        (Some(a), Some(b)) => {
            if durations_match(Some(a), Some(b)) {
                1.0
            } else if a.abs_diff(b) <= 10_000 {
                0.5
            } else {
                0.0
            }
        }
        _ => 0.5,
    };
    // A wrong-length hit (live/extended/cover) must not pass on text alone.
    let s = 0.45 * title + 0.30 * artist + 0.25 * duration + cand.bonus;
    if title < 0.5 || artist < 0.5 {
        return s.min(0.5);
    }
    if duration == 0.0 {
        return s.min(0.6);
    }
    s.clamp(0.0, 1.0)
}

fn best<'a>(want: &Track, cands: &'a [Candidate]) -> Option<(&'a Candidate, f32)> {
    cands
        .iter()
        .map(|c| (c, score(want, c)))
        .filter(|(_, s)| *s >= ACCEPT_SCORE)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

fn effective_priority(req: &[ProviderId]) -> Vec<ProviderId> {
    let mut out: Vec<ProviderId> = Vec::new();
    for p in req.iter().copied().filter(|p| providers::is_full_source(*p)) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.is_empty() {
        DEFAULT_PRIORITY.to_vec()
    } else {
        out
    }
}

fn cache_key(want: &Track, prio: &[ProviderId]) -> String {
    let ident = match want.isrc.as_deref().filter(|s| !s.is_empty()) {
        Some(i) => format!("isrc:{}", i.to_ascii_lowercase()),
        None => format!(
            "t:{}|{}|{}",
            plain_words(&merge::normalize_artist(&want.artist)),
            normalize_title(&want.title),
            want.duration_ms.map(|d| d / 3000).unwrap_or(0)
        ),
    };
    let p: Vec<&str> = prio.iter().map(|p| p.badge()).collect();
    format!("resolve:v3:{ident}:{}", p.join(""))
}

async fn try_provider(svc: &Services, p: ProviderId, want: &Track, extended: bool) -> Option<(Track, PlaybackSource, f32)> {
    if !svc.config.provider_enabled(p) || is_skipped(svc, p).await {
        return None;
    }
    // Already known at this provider (the track came from it, or carries its id).
    let known = if want.source == p { Some(want.id.clone()) } else { want.external_ids.get(&p).cloned() };
    if let Some(id) = known {
        if let Ok(pb) = providers::playback_for(svc, p, &id).await {
            let mut m = want.clone();
            if want.source != p {
                m.external_ids.remove(&p);
                m.external_ids.insert(want.source, want.id.clone());
                m.source = p;
                m.id = id;
            }
            return Some((m, pb, 1.0));
        }
    }
    // Ask the companion for the song audio rather than choreography, live
    // performances and music videos with a different duration.
    let base = format!("{} {}", want.artist, want.title);
    let mut queries = Vec::new();
    if p == ProviderId::YouTube {
        if let Some(isrc) = want.isrc.as_ref().filter(|s| !s.trim().is_empty()) { queries.push(isrc.clone()); }
        queries.push(format!("{base} audio"));
    }
    queries.push(base.clone());
    if extended && p == ProviderId::YouTube {
        queries.push(format!("{base} official audio"));
        queries.push(format!("{} {}", crate::merge::normalize_artist(&want.artist), normalize_title(&want.title)));
        queries.push(format!("{} - {}", want.artist, want.title));
    }
    let mut seen = std::collections::HashSet::new();
    queries.retain(|q| seen.insert(q.clone()));
    for query in queries {
        let cands = match search_provider(svc, p, query.trim()).await {
            Ok(cands) => cands,
            Err(_) => continue,
        };
        let Some((cand, s)) = best(want, &cands) else { continue; };
        let pb = match &cand.playback {
            Some(pb) => pb.clone(),
            None => providers::playback_for(svc, p, &cand.track.id).await.ok()?,
        };
        let mut m = cand.track.clone();
        merge_into(&mut m, want);
        return Some((m, pb, s));
    }
    None
}

async fn try_preview(svc: &Services, want: &Track) -> Option<(Track, PlaybackSource, f32)> {
    if !svc.config.provider_enabled(ProviderId::Deezer) || is_skipped(svc, ProviderId::Deezer).await {
        return None;
    }
    let known = if want.source == ProviderId::Deezer {
        Some(&want.id)
    } else {
        want.external_ids.get(&ProviderId::Deezer)
    };
    if let Some(id) = known {
        if let Ok(c) = deezer::by_id(svc, id).await {
            if let Some(pb) = c.playback {
                let mut m = c.track;
                merge_into(&mut m, want);
                return Some((m, pb, 1.0));
            }
        }
    }
    if let Some(isrc) = want.isrc.as_deref() {
        if let Ok(Some(c)) = deezer::by_isrc(svc, isrc).await {
            if let Some(pb) = c.playback.clone() {
                let mut m = c.track;
                merge_into(&mut m, want);
                return Some((m, pb, 1.0));
            }
        }
    }
    let q = format!("{} {}", want.artist, want.title);
    let cands = search_provider(svc, ProviderId::Deezer, q.trim()).await.ok()?;
    let with_preview: Vec<Candidate> = cands.into_iter().filter(|c| c.playback.is_some()).collect();
    let (cand, s) = best(want, &with_preview)?;
    let mut m = cand.track.clone();
    merge_into(&mut m, want);
    let fresh = deezer::by_id(svc, &cand.track.id).await.ok()?;
    Some((m, fresh.playback?, s))
}

/// Cache lifetime of an answer: full sources 30 days, previews (signed URLs)
/// 5 minutes, misses 1 hour.
fn ttl(resp: &ResolveResponse) -> u64 {
    match &resp.source {
        Some(PlaybackSource::Preview { .. }) => 5 * cache::MINUTE,
        Some(_) => 30 * DAY,
        None => HOUR,
    }
}

pub async fn resolve(svc: &Services, req: ResolveRequest) -> ResolveResponse {
    let want = &req.track;
    let prio = if req.youtube_only { vec![ProviderId::YouTube] } else { effective_priority(&req.priority) };
    let key = if req.youtube_only { format!("{}:youtube-only", cache_key(want, &prio)) } else { cache_key(want, &prio) };
    // Only full sources are final mappings. A preview or an old miss must
    // retry the higher-priority providers, including on the companion.
    if let Some(hit) = cache::get_json::<ResolveResponse>(&*svc.cache, &key).await {
        if hit.source.as_ref().is_some_and(|source| !matches!(source, PlaybackSource::Preview { .. })) {
            return hit;
        }
    }

    if let Some(mut remote) = svc.cmds.resolve(&req).await {
        // Older companions may ignore the new flag; still enforce it here.
        if req.youtube_only && !matches!(remote.source.as_ref(), Some(PlaybackSource::Stream { provider: ProviderId::YouTube, .. } | PlaybackSource::Embed { provider: ProviderId::YouTube, .. })) {
            remote = ResolveResponse { matched: None, source: None, confidence: 0.0, youtube_hint: None };
        }
        cache::put_json(&*svc.cache, &key, &remote, ttl(&remote)).await;
        return remote;
    }
    let mut found = None;
    for p in &prio {
        if let Some(r) = with_timeout(&*svc.rt, if req.youtube_only { 45_000 } else { 65_000 }, try_provider(svc, *p, want, req.youtube_only)).await.flatten() {
            found = Some(r);
            break;
        }
    }
    if found.is_none() && !req.youtube_only {
        found = with_timeout(&*svc.rt, svc.config.provider_timeout_ms, try_preview(svc, want)).await.flatten();
    }
    let (resp, ttl) = match found {
        Some((m, pb, c)) => {
            let ttl = if matches!(pb, PlaybackSource::Preview { .. }) { 5 * cache::MINUTE } else { 30 * DAY };
            (ResolveResponse { matched: Some(m), source: Some(pb), confidence: c, youtube_hint: None }, ttl)
        },
        None => (ResolveResponse { matched: None, source: None, confidence: 0.0, youtube_hint: None }, HOUR),
    };
    cache::put_json(&*svc.cache, &key, &resp, ttl).await;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;
    use ProviderId::*;

    fn want() -> Track {
        let mut t = track("sp1", Spotify, "Get Lucky", "Daft Punk", 369);
        t.isrc = Some("GBDUW1300001".into());
        t
    }

    fn cand(id: &str, title: &str, artist: &str, secs: u64) -> Candidate {
        Candidate { track: track(id, YouTube, title, artist, secs), bonus: 0.0, playback: None }
    }

    /// Companion that answers every resolve with one fixed match.
    struct Companion(std::sync::Mutex<usize>);
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl crate::traits::CommandRunner for Companion {
        async fn run(&self, _: &str, _: &[String], _: u64) -> Result<Vec<u8>, crate::traits::CommandError> {
            Err(crate::traits::CommandError::Unavailable)
        }
        async fn resolve(&self, req: &ResolveRequest) -> Option<ResolveResponse> {
            *self.0.lock().unwrap() += 1;
            let m = track("yt1", YouTube, &req.track.title, &req.track.artist, 369);
            Some(ResolveResponse { matched: Some(m), source: Some(providers::youtube::playback("yt1")), confidence: 0.9, youtube_hint: None })
        }
    }

    #[test]
    fn misses_are_resolved_on_the_companion_and_cached() {
        let h = harness(all_secrets());
        let companion = std::sync::Arc::new(Companion(Default::default()));
        let svc = Services { http: h.svc.http.clone(), cache: h.svc.cache.clone(), limiter: h.svc.limiter.clone(), rt: h.svc.rt.clone(), cmds: companion.clone(), config: h.svc.config.clone() };
        let req = || ResolveRequest { youtube_only: false, track: want(), priority: vec![] };
        let first = block_on(resolve(&svc, req()));
        assert_eq!(first.matched.unwrap().id, "yt1");
        // No provider was asked here; the second call is a cache hit.
        assert_eq!(h.http.call_count("http"), 0);
        block_on(resolve(&svc, req()));
        assert_eq!(*companion.0.lock().unwrap(), 1);
    }

    #[test]
    fn cached_deezer_preview_is_upgraded_by_the_companion() {
        let h = harness(all_secrets());
        let companion = std::sync::Arc::new(Companion(Default::default()));
        let svc = Services { http: h.svc.http.clone(), cache: h.svc.cache.clone(), limiter: h.svc.limiter.clone(), rt: h.svc.rt.clone(), cmds: companion.clone(), config: h.svc.config.clone() };
        let preview = ResolveResponse { matched: Some(want()), source: Some(PlaybackSource::Preview {
            provider: Deezer, url: "https://cdn/expired.mp3".into(),
        }), confidence: 1.0, youtube_hint: None };
        block_on(cache::put_json(&*svc.cache, &cache_key(&want(), &[YouTube]), &preview, 30 * DAY));
        let result = block_on(resolve(&svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![YouTube] }));
        assert!(matches!(result.source, Some(PlaybackSource::Embed { provider: YouTube, .. })));
        assert_eq!(*companion.0.lock().unwrap(), 1);
    }

    #[test]
    fn cached_miss_does_not_hide_recovered_youtube() {
        let h = setup();
        let miss = ResolveResponse { matched: None, source: None, confidence: 0.0, youtube_hint: None };
        block_on(cache::put_json(&*h.svc.cache, &cache_key(&want(), &[YouTube]), &miss, HOUR));
        h.cmds.on("ytsearch", r#"{"id":"yt1","title":"Get Lucky","channel":"Daft Punk - Topic","duration":369}"#);
        let result = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![YouTube] }));
        assert!(matches!(result.source, Some(PlaybackSource::Embed { provider: YouTube, .. })));
    }

    #[test]
    fn youtube_only_does_not_return_other_sources_or_previews() {
        let h = setup();
        h.http.on("audius.co", 200, r#"{"data":[{"id":"a","title":"Get Lucky","duration":369,"user":{"name":"Daft Punk"}}]}"#);
        h.http.on("api.deezer.com", 200, r#"{"id":1,"title":"Get Lucky","preview":"https://cdn/preview.mp3"}"#);
        let result = block_on(resolve(&h.svc, ResolveRequest { youtube_only: true, track: want(), priority: vec![Audius] }));
        assert!(result.source.is_none());
        assert_eq!(h.http.call_count("http"), 0);
    }

    #[test]
    fn youtube_only_tries_additional_search_variants() {
        let h = setup();
        h.cmds.on("official audio", r#"{"id":"abcdefghijk","title":"Get Lucky","channel":"Daft Punk - Topic","duration":369}"#);
        let result = block_on(resolve(&h.svc, ResolveRequest { youtube_only: true, track: want(), priority: vec![Audius] }));
        assert!(matches!(result.source, Some(PlaybackSource::Embed { provider: YouTube, .. })));
        assert_eq!(h.cmds.call_count("official audio"), 1);
        assert_eq!(h.http.call_count("http"), 0);
    }

    #[test]
    fn scoring_prefers_duration_and_rejects_wrong_song() {
        let w = want();
        let good = score(&w, &cand("a", "Get Lucky", "Daft Punk", 370));
        let live = score(&w, &cand("b", "Get Lucky", "Daft Punk", 480));
        let other = score(&w, &cand("c", "Get Down", "Someone", 369));
        assert!(good > 0.95, "{good}");
        assert!(live < ACCEPT_SCORE, "{live}");
        assert!(other < ACCEPT_SCORE, "{other}");
        let mut c = cand("d", "Totally different", "X", 1);
        c.track.isrc = Some("gbduw1300001".into());
        assert_eq!(score(&w, &c), 1.0);
        let mut off = cand("e", "Get Lucky", "Daft Punk", 369);
        off.bonus = 0.15;
        assert!(score(&w, &off) <= 1.0);
    }

    #[test]
    fn priority_normalisation() {
        assert_eq!(effective_priority(&[]), DEFAULT_PRIORITY.to_vec());
        assert_eq!(effective_priority(&[Spotify, Deezer]), DEFAULT_PRIORITY.to_vec());
        assert_eq!(effective_priority(&[YouTube, Audius, YouTube]), vec![YouTube, Audius]);
    }

    fn setup() -> Harness {
        let h = harness_with_ytdlp(all_secrets());
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h
    }

    #[test]
    fn audius_wins_over_youtube_when_both_match() {
        let h = setup();
        h.http.on(
            "audius.co/v1/tracks/search",
            200,
            r#"{"data":[{"id":"au1","title":"Get Lucky","duration":369,"user":{"name":"Daft Punk"}}]}"#,
        );
        h.cmds.on("ytsearch", "{}");
        let r = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![] }));
        let m = r.matched.unwrap();
        assert_eq!((m.source, m.id.as_str()), (Audius, "au1"));
        assert_eq!(m.external_ids.get(&Spotify).unwrap(), "sp1");
        assert_eq!(m.isrc.as_deref(), Some("GBDUW1300001"));
        assert!(matches!(r.source, Some(PlaybackSource::Stream { provider: Audius, .. })));
        assert!(r.confidence > 0.9);
        assert_eq!(h.cmds.call_count("ytsearch"), 0);
    }

    #[test]
    fn falls_through_priority_to_youtube_and_caches() {
        let h = setup();
        h.http.on("audius.co/v1/tracks/search", 200, r#"{"data":[]}"#);
        h.http.on("jamendo.com", 200, r#"{"results":[]}"#);
        h.http.on("api.soundcloud.com/tracks", 200, "[]");
        h.http.on("oauth/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.cmds.on(
            "ytsearch",
            concat!(
                r#"{"id":"live","title":"Daft Punk - Get Lucky (Live at Home)","channel":"Daft Punk","duration":9.0}"#,
                "\n",
                r#"{"id":"topic","title":"Get Lucky","channel":"Daft Punk - Topic","duration":370.0}"#,
            ),
        );
        let r = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![] }));
        match r.source {
            Some(PlaybackSource::Embed { provider: YouTube, ref embed_id }) => assert_eq!(embed_id, "topic"),
            ref o => panic!("{o:?}"),
        }
        let calls = h.cmds.call_count("ytsearch");
        let r2 = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![] }));
        assert_eq!(r2, r);
        assert_eq!(h.cmds.call_count("ytsearch"), calls, "second resolve must hit the cache");
    }

    #[test]
    fn respects_custom_priority() {
        let h = setup();
        h.http.on("audius.co/v1/tracks/search", 200, r#"{"data":[{"id":"au1","title":"Get Lucky","duration":369,"user":{"name":"Daft Punk"}}]}"#);
        h.cmds.on("ytsearch", r#"{"id":"yt1","title":"Get Lucky","channel":"Daft Punk","duration":369.0}"#);
        let r = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![YouTube, Audius] }));
        assert_eq!(r.matched.unwrap().source, YouTube);
    }

    #[test]
    fn known_external_id_skips_search() {
        let h = setup();
        let mut w = want();
        w.external_ids.insert(Audius, "known".into());
        let r = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: w, priority: vec![] }));
        let m = r.matched.unwrap();
        assert_eq!((m.source, m.id.as_str(), r.confidence), (Audius, "known", 1.0));
        assert!(h.http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cached_preview_refreshes_signature() {
        let h = setup();
        let mut track = want();
        track.source = Deezer;
        track.id = "1".into();
        let stale = ResolveResponse {
            matched: Some(track.clone()),
            source: Some(PlaybackSource::Preview { provider: Deezer, url: "https://cdn/expired.mp3".into() }),
            confidence: 1.0,
            youtube_hint: None,
        };
        block_on(cache::put_json(&*h.svc.cache, &cache_key(&track, &DEFAULT_PRIORITY), &stale, 30 * DAY));
        h.http.on("api.deezer.com/track/1", 200,
            r#"{"id":1,"title":"Get Lucky","preview":"https://cdn/fresh.mp3"}"#);
        let first = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: track.clone(), priority: vec![] }));
        assert!(matches!(first.source, Some(PlaybackSource::Preview { url, .. }) if url == "https://cdn/fresh.mp3"));
        h.http.rules.lock().unwrap().iter_mut().find(|r| r.0 == "api.deezer.com/track/1").unwrap().2 =
            r#"{"id":1,"title":"Get Lucky","preview":"https://cdn/newer.mp3"}"#.into();
        let second = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track, priority: vec![] }));
        assert!(matches!(second.source, Some(PlaybackSource::Preview { url, .. }) if url == "https://cdn/newer.mp3"));
        assert_eq!(h.http.calls.lock().unwrap().iter().filter(|url| url.contains("api.deezer.com/track/1")).count(), 2);
    }

    #[test]
    fn cached_preview_does_not_hide_recovered_youtube() {
        let h = setup();
        let track = want();
        let preview = ResolveResponse { matched: Some(track.clone()),
            source: Some(PlaybackSource::Preview { provider: Deezer, url: "https://cdn/preview.mp3".into() }),
            confidence: 1.0, youtube_hint: None };
        block_on(cache::put_json(&*h.svc.cache, &cache_key(&track, &[YouTube]), &preview, 30 * DAY));
        h.cmds.on("ytsearch", r#"{"id":"yt1","title":"Get Lucky","channel":"Daft Punk - Topic","duration":369.0}"#);
        let result = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track, priority: vec![YouTube] }));
        assert!(matches!(result.source, Some(PlaybackSource::Embed { provider: YouTube, .. })));
        assert_eq!(h.cmds.call_count("ytsearch10:GBDUW1300001"), 1);
    }

    #[test]
    fn youtube_retries_plain_query_when_audio_query_has_no_match() {
        let h = setup();
        h.cmds.on("Get Lucky audio", "{}");
        h.cmds.on("ytsearch10:Daft Punk Get Lucky", r#"{"id":"yt1","title":"Get Lucky","channel":"Daft Punk - Topic","duration":369}"#);
        let mut track = want();
        track.isrc = None;
        let result = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track, priority: vec![YouTube] }));
        assert!(matches!(result.source, Some(PlaybackSource::Embed { provider: YouTube, .. })));
    }

    #[test]
    fn youtube_retries_text_after_isrc_search_fails() {
        let h = setup();
        *h.cmds.failed.lock().unwrap() = Some("ytsearch10:GBDUW1300001".into());
        h.cmds.on("ytsearch10:Daft Punk Get Lucky audio",
            r#"{"id":"yt1","title":"Get Lucky","channel":"Daft Punk - Topic","duration":369}"#);
        let result = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![YouTube] }));
        assert!(matches!(result.source, Some(PlaybackSource::Embed { provider: YouTube, .. })));
        assert_eq!(h.cmds.call_count("ytsearch10:Daft Punk Get Lucky audio"), 1);
    }

    #[test]
    fn preview_search_refreshes_search_url() {
        let h = setup();
        h.http.on("api.deezer.com/search", 200,
            r#"{"data":[{"id":1,"title":"Get Lucky","duration":369,"preview":"https://cdn/stale.mp3","artist":{"name":"Daft Punk"}}]}"#);
        h.http.on("api.deezer.com/track/1", 200,
            r#"{"id":1,"title":"Get Lucky","preview":"https://cdn/fresh.mp3"}"#);
        let mut track = want();
        track.isrc = None;
        let result = block_on(try_preview(&h.svc, &track)).unwrap();
        assert!(matches!(result.1, PlaybackSource::Preview { url, .. } if url == "https://cdn/fresh.mp3"));
    }

    #[test]
    fn preview_fallback_then_nothing() {
        let h = setup();
        h.http.on("audius.co", 200, r#"{"data":[]}"#);
        h.http.on("jamendo.com", 200, r#"{"results":[]}"#);
        h.http.on("api.soundcloud.com/tracks", 200, "[]");
        h.cmds.on("ytsearch", "{}");
        h.http.on("track/isrc:GBDUW1300001", 200,
            r#"{"id":1,"title":"Get Lucky","duration":369,"preview":"https://cdn/prev.mp3","artist":{"name":"Daft Punk"}}"#);
        let r = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![] }));
        assert!(matches!(r.source, Some(PlaybackSource::Preview { provider: Deezer, .. })));

        let h = setup();
        h.http.on("audius.co", 200, r#"{"data":[]}"#);
        let r = block_on(resolve(&h.svc, ResolveRequest { youtube_only: false, track: want(), priority: vec![] }));
        assert!(r.matched.is_none() && r.source.is_none());
        assert_eq!(r.confidence, 0.0);
    }
}
