//! Explicit rating for YouTube uploads: YouTube does not say whether a song
//! is explicit, so search results no rating source knows are looked up at
//! Deezer ("Artist Title", scored like the resolver). The answer is cached
//! here for every app; songs Deezer does not know keep the "?"
//! ([`glyph_protocol::Track::explicit_unknown`]).

use crate::cache::{self, DAY};
use crate::merge::plain_words;
use crate::providers::{self, ProviderError};
use crate::resolver::{score, ACCEPT_SCORE};
use crate::search::search_provider;
use crate::traits::{with_timeout, Services};
use futures::stream::{self, StreamExt};
use glyph_protocol::{ProviderId, Track};

/// Songs looked up per search, lookups at once.
const MAX_LOOKUPS: usize = 25;
const PARALLEL: usize = 6;
/// The search only ends after the lookups: all of them together.
const TOTAL_TIMEOUT_MS: u64 = 6_000;
/// A rating hardly changes; a song Deezer lacks may be added later.
const FOUND_TTL_S: u64 = 90 * DAY;
const MISSING_TTL_S: u64 = 7 * DAY;

/// Deezer id + explicit, `None` = Deezer does not know the song.
type Hit = Option<(String, bool)>;

fn key(t: &Track) -> String {
    format!("rating:v1:{}", plain_words(&format!("{} {}", t.artist, t.title)))
}

/// `t` with the Deezer match's id and rating, or `None` when there is none.
pub fn apply(t: &Track, hit: &Hit) -> Option<Track> {
    let (id, explicit) = hit.as_ref()?;
    let mut out = t.clone();
    out.external_ids.insert(ProviderId::Deezer, id.clone());
    out.explicit = *explicit;
    Some(out)
}

/// `None` when Deezer could not be asked (nothing is cached then).
async fn look_up(svc: &Services, t: &Track) -> Option<Hit> {
    let q = format!("{} {}", t.artist, t.title);
    let cands = match search_provider(svc, ProviderId::Deezer, &q).await {
        Ok(c) => c,
        // Deezer is no search source here: it still rates.
        Err(ProviderError::NotConfigured) => providers::deezer::search(svc, &q).await.ok()?,
        Err(_) => return None,
    };
    let best = cands
        .into_iter()
        .map(|c| (score(t, &c), c))
        .filter(|(s, _)| *s >= ACCEPT_SCORE)
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Some(best.map(|(_, c)| (c.track.id, c.track.explicit)))
}

async fn rate_one(svc: &Services, t: &Track) -> Option<Track> {
    let k = key(t);
    if let Some(hit) = cache::get_json::<Hit>(&*svc.cache, &k).await {
        return apply(t, &hit);
    }
    let hit = look_up(svc, t).await?;
    cache::put_json(&*svc.cache, &k, &hit, if hit.is_some() { FOUND_TTL_S } else { MISSING_TTL_S }).await;
    apply(t, &hit)
}

/// The unrated YouTube songs of `tracks` that Deezer knows, now with its id
/// and rating (same `source` + `id`, so the app replaces them).
pub async fn rate_unknown(svc: &Services, tracks: &[Track]) -> Vec<Track> {
    let todo: Vec<&Track> = tracks.iter().filter(|t| t.explicit_unknown()).take(MAX_LOOKUPS).collect();
    if todo.is_empty() {
        return Vec::new();
    }
    let mut rated = Vec::new();
    let pending: Vec<_> = todo.into_iter().map(|t| rate_one(svc, t)).collect();
    let mut lookups = stream::iter(pending).buffer_unordered(PARALLEL);
    let collect = async {
        while let Some(r) = lookups.next().await {
            rated.extend(r);
        }
    };
    // What is still open after the time is up stays "?" for this search.
    let _ = with_timeout(&*svc.rt, TOTAL_TIMEOUT_MS, collect).await;
    rated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    fn upload(title: &str) -> Track {
        serde_json::from_value(serde_json::json!({"id": "v1", "source": "youtube", "title": title, "artist": "Artist"})).unwrap()
    }

    #[test]
    fn deezer_rates_youtube_uploads_and_is_cached() {
        let h = harness(all_secrets());
        h.http.on(
            "api.deezer.com/search",
            200,
            r#"{"data":[{"id":9,"title":"Song","duration":201,"explicit_lyrics":true,"artist":{"name":"Artist"}}]}"#,
        );
        let rated = block_on(rate_unknown(&h.svc, &[upload("Song")]));
        assert_eq!(rated.len(), 1);
        assert!(rated[0].explicit && rated[0].id == "v1" && rated[0].external_ids.get(&ProviderId::Deezer).map(String::as_str) == Some("9"));
        let again = block_on(rate_unknown(&h.svc, &[upload("Song")]));
        assert_eq!(again, rated);
        assert_eq!(h.http.call_count("api.deezer.com/search"), 1);
    }

    #[test]
    fn unknown_songs_stay_unrated() {
        let h = harness(all_secrets());
        h.http.on("api.deezer.com/search", 200, r#"{"data":[{"id":9,"title":"Something Else","artist":{"name":"Other"}}]}"#);
        assert!(block_on(rate_unknown(&h.svc, &[upload("Cat compilation 2024")])).is_empty());
        // Rated tracks and other sources are not looked up.
        let mut known = upload("Song");
        known.explicit = true;
        assert!(block_on(rate_unknown(&h.svc, &[known])).is_empty());
        assert_eq!(h.http.call_count("api.deezer.com/search"), 1);
    }
}
