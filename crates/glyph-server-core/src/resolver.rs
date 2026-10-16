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
use crate::traits::Services;
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
    format!("resolve:v1:{ident}:{}", p.join(""))
}

async fn try_provider(svc: &Services, p: ProviderId, want: &Track) -> Option<(Track, PlaybackSource, f32)> {
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
    let q = format!("{} {}", want.artist, want.title);
    let cands = search_provider(svc, p, q.trim()).await.ok()?;
    let (cand, s) = best(want, &cands)?;
    let pb = match &cand.playback {
        Some(pb) => pb.clone(),
        None => providers::playback_for(svc, p, &cand.track.id).await.ok()?,
    };
    let mut m = cand.track.clone();
    merge_into(&mut m, want);
    Some((m, pb, s))
}

async fn try_preview(svc: &Services, want: &Track) -> Option<(Track, PlaybackSource, f32)> {
    if is_skipped(svc, ProviderId::Deezer).await {
        return None;
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
    Some((m, cand.playback.clone()?, s))
}

pub async fn resolve(svc: &Services, req: ResolveRequest) -> ResolveResponse {
    let want = &req.track;
    let prio = effective_priority(&req.priority);
    let key = cache_key(want, &prio);
    if let Some(hit) = cache::get_json::<ResolveResponse>(&*svc.cache, &key).await {
        return hit;
    }

    let mut found = None;
    for p in &prio {
        if let Some(r) = try_provider(svc, *p, want).await {
            found = Some(r);
            break;
        }
    }
    if found.is_none() {
        found = try_preview(svc, want).await;
    }
    let (resp, ttl) = match found {
        Some((m, pb, c)) => (ResolveResponse { matched: Some(m), source: Some(pb), confidence: c }, 30 * DAY),
        None => (ResolveResponse { matched: None, source: None, confidence: 0.0 }, HOUR),
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
        let h = harness(all_secrets());
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
        h.http.on("youtube/v3", 200, r#"{"items":[]}"#);
        let r = block_on(resolve(&h.svc, ResolveRequest { track: want(), priority: vec![] }));
        let m = r.matched.unwrap();
        assert_eq!((m.source, m.id.as_str()), (Audius, "au1"));
        assert_eq!(m.external_ids.get(&Spotify).unwrap(), "sp1");
        assert_eq!(m.isrc.as_deref(), Some("GBDUW1300001"));
        assert!(matches!(r.source, Some(PlaybackSource::Stream { provider: Audius, .. })));
        assert!(r.confidence > 0.9);
        assert_eq!(h.http.call_count("youtube"), 0);
    }

    #[test]
    fn falls_through_priority_to_youtube_and_caches() {
        let h = setup();
        h.http.on("audius.co/v1/tracks/search", 200, r#"{"data":[]}"#);
        h.http.on("jamendo.com", 200, r#"{"results":[]}"#);
        h.http.on("api.soundcloud.com/tracks", 200, "[]");
        h.http.on("oauth/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on(
            "youtube/v3/search",
            200,
            r#"{"items":[
              {"id":{"videoId":"live"},"snippet":{"title":"Daft Punk - Get Lucky (Live)","channelTitle":"Daft Punk"}},
              {"id":{"videoId":"topic"},"snippet":{"title":"Get Lucky","channelTitle":"Daft Punk - Topic"}}]}"#,
        );
        h.http.on(
            "youtube/v3/videos",
            200,
            r#"{"items":[{"id":"live","contentDetails":{"duration":"PT9M"},"status":{"embeddable":true}},
                         {"id":"topic","contentDetails":{"duration":"PT6M10S"},"status":{"embeddable":true}}]}"#,
        );
        let r = block_on(resolve(&h.svc, ResolveRequest { track: want(), priority: vec![] }));
        match r.source {
            Some(PlaybackSource::Embed { provider: YouTube, ref embed_id }) => assert_eq!(embed_id, "topic"),
            ref o => panic!("{o:?}"),
        }
        let calls = h.http.calls.lock().unwrap().len();
        let r2 = block_on(resolve(&h.svc, ResolveRequest { track: want(), priority: vec![] }));
        assert_eq!(r2, r);
        assert_eq!(h.http.calls.lock().unwrap().len(), calls, "second resolve must hit the cache");
    }

    #[test]
    fn respects_custom_priority() {
        let h = setup();
        h.http.on("audius.co/v1/tracks/search", 200, r#"{"data":[{"id":"au1","title":"Get Lucky","duration":369,"user":{"name":"Daft Punk"}}]}"#);
        h.http.on("youtube/v3/search", 200, r#"{"items":[{"id":{"videoId":"yt1"},"snippet":{"title":"Get Lucky","channelTitle":"Daft Punk"}}]}"#);
        h.http.on("youtube/v3/videos", 200, r#"{"items":[{"id":"yt1","contentDetails":{"duration":"PT6M9S"},"status":{"embeddable":true}}]}"#);
        let r = block_on(resolve(&h.svc, ResolveRequest { track: want(), priority: vec![YouTube, Audius] }));
        assert_eq!(r.matched.unwrap().source, YouTube);
    }

    #[test]
    fn known_external_id_skips_search() {
        let h = setup();
        let mut w = want();
        w.external_ids.insert(Audius, "known".into());
        let r = block_on(resolve(&h.svc, ResolveRequest { track: w, priority: vec![] }));
        let m = r.matched.unwrap();
        assert_eq!((m.source, m.id.as_str(), r.confidence), (Audius, "known", 1.0));
        assert!(h.http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn preview_fallback_then_nothing() {
        let h = setup();
        h.http.on("audius.co", 200, r#"{"data":[]}"#);
        h.http.on("jamendo.com", 200, r#"{"results":[]}"#);
        h.http.on("api.soundcloud.com/tracks", 200, "[]");
        h.http.on("youtube/v3/search", 200, r#"{"items":[]}"#);
        h.http.on("track/isrc:GBDUW1300001", 200,
            r#"{"id":1,"title":"Get Lucky","duration":369,"preview":"https://cdn/prev.mp3","artist":{"name":"Daft Punk"}}"#);
        let r = block_on(resolve(&h.svc, ResolveRequest { track: want(), priority: vec![] }));
        assert!(matches!(r.source, Some(PlaybackSource::Preview { provider: Deezer, .. })));

        let h = setup();
        h.http.on("audius.co", 200, r#"{"data":[]}"#);
        let r = block_on(resolve(&h.svc, ResolveRequest { track: want(), priority: vec![] }));
        assert!(r.matched.is_none() && r.source.is_none());
        assert_eq!(r.confidence, 0.0);
    }
}
