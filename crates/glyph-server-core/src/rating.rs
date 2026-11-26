//! Multi-source explicit rating for songs:
//!
//! 1. Title explicit markers (`(Explicit)` / `[Explicit]`, excluding clean edits)
//! 2. Deezer (`explicit_lyrics` + `explicit_content_lyrics` code, preserving "unknown")
//! 3. Apple Music / iTunes (`trackExplicitness`)
//! 4. LRCLIB lyrics profanity scan (catches explicit songs labels forgot to flag
//!    and confirms clean ratings for unrated uploads with clean lyrics)
//!
//! Answers are cached in `Services::cache` (`rating:v2:...`, 90 days when
//! known, 7 days when unknown) so every app shares the result. Songs no source
//! can rate keep `?` ([`ExplicitRating::Unknown`]).

use crate::cache::{self, DAY, HOUR};
use crate::merge::plain_words;
use crate::providers::{self, ProviderError};
use crate::resolver::{score, ACCEPT_SCORE};
use crate::search::search_provider;
use crate::traits::{with_timeout, LyricsLookup, LyricsQuery, Services};
use futures::stream::{self, StreamExt};
use glyph_protocol::{ExplicitRating, LyricsResponse, ProviderId, Track};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Uncached lookups per search (bounded for Cloudflare Workers subrequest limits).
#[cfg(target_arch = "wasm32")]
const MAX_LOOKUPS: usize = 10;
#[cfg(not(target_arch = "wasm32"))]
const MAX_LOOKUPS: usize = 20;

const PARALLEL: usize = 5;
/// The search only ends after the lookups: all of them together.
const TOTAL_TIMEOUT_MS: u64 = 6_000;
const LYRICS_TIMEOUT_MS: u64 = 2_500;
/// A rating hardly changes; a song no source knows may be added later.
const FOUND_TTL_S: u64 = 90 * DAY;
const MISSING_TTL_S: u64 = 7 * DAY;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RatingEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deezer_id: Option<String>,
    pub rating: ExplicitRating,
}

fn key_for(artist: &str, title: &str) -> String {
    format!("rating:v2:{}", plain_words(&format!("{artist} {title}")))
}

fn key(t: &Track) -> String {
    key_for(&t.artist, &t.title)
}

/// Exact whole words that unambiguously mark explicit lyrics (EN/DE/RU/ES/FR).
/// Mild words (`damn`, `hell`, `crap`) and short substring-prone words (`ass`,
/// `cock`, `dick`) are excluded to prevent false positives.
const EXACT_EXPLICIT: &[&str] = &[
    // English
    "fuck", "fucked", "fucker", "fuckers", "fucking", "fuckin", "fucks", "fucka",
    "motherfucker", "motherfuckers", "motherfucking", "motherfuckin", "motherfucka",
    "muthafucka", "muthafuckas", "muthafuckin", "muthafucking",
    "shit", "shits", "shitty", "shitting", "shittin", "shitted",
    "bullshit", "horseshit", "apeshit", "dipshit", "batshit", "shithead", "shitheads",
    "bitch", "bitches", "bitching", "bitchin", "bitchy", "bitched",
    "cunt", "cunts", "whore", "whores", "slut", "sluts", "slutty",
    "asshole", "assholes", "arsehole", "arseholes",
    "dickhead", "dickheads", "cocksucker", "cocksuckers",
    "pussy", "pussies", "nigga", "niggas", "nigger", "niggers",
    "faggot", "faggots", "twat", "twats", "wanker", "wankers",
    "blowjob", "blowjobs", "handjob", "cumshot", "jizz", "dildo", "dildos",
    "skank", "skanks", "thot", "thots",
    // German
    "fick", "ficken", "fickt", "fickte", "fickst", "gefickt",
    "verfickt", "verfickte", "verfickten", "verfickter", "verficktes",
    "abgefuckt", "abgefuckte", "abgefuckten",
    "hurensohn", "hurensöhne", "hurensohne",
    "fotze", "fotzen", "wichser", "wixer", "wichsen", "gewichst",
    "arschloch", "arschlöcher", "arschlocher",
    "missgeburt", "missgeburten", "nutte", "nutten",
    "schlampe", "schlampen", "hure", "huren",
    "scheiße", "scheisse", "scheiß", "scheiss", "scheißdreck", "scheissdreck",
    "scheißegal", "scheissegal", "beschissen", "beschissene", "beschissenen",
    "bastard", "bastarde", "spast", "spasti", "spasten", "schwanzlutscher", "bumsen",
    // Russian
    "бля", "блядь", "блять", "бляди", "блядство", "блядский", "блядская",
    "сука", "суки", "сучка", "сучки",
    "хуй", "хуя", "хуе", "хуйня", "хуесос", "хуёвый", "хуевый", "хуево", "хуёво",
    "нахуй", "похуй", "охуеть", "ахуеть", "охуенно", "ахуенно",
    "пизда", "пиздец", "пизды", "пизду", "пиздить", "пиздит", "пиздатый",
    "ебать", "ебал", "ебала", "ебаный", "ёбаный", "еблан", "ебанутый",
    "уебан", "уёбок", "уебок", "долбоеб", "долбоёб",
    "заебал", "заебали", "заебись", "выебать", "съебал",
    "пидор", "пидорас", "пидарас", "гандон", "мудак", "мудаки", "шлюха", "шлюхи",
    // Spanish
    "puta", "putas", "puto", "putos", "putazo", "hijueputa",
    "mierda", "mierdas", "joder", "jodido", "jodida", "coño",
    "cabrón", "cabron", "cabrones", "chingar", "chinga", "chingada", "chingado", "chingón",
    "pendejo", "pendejos", "verga", "vergas", "culero", "culeros", "maricón", "maricon",
    // French
    "putain", "merde", "enculé", "encule", "enculer", "salope", "salopes",
    "connard", "connards", "connasse", "connasses",
    "nique", "niquer", "niqué", "pute", "putes", "couilles", "branleur",
];

/// Long unambiguous token prefixes (checked at start of a whole token only).
const EXPLICIT_PREFIXES: &[&str] = &[
    "motherfuck", "muthafuck", "fuck", "bullshit",
    "hurensohn", "arschloch", "verfickt", "scheiß", "scheiss", "beschissen",
    "пиздец", "нахуй", "похуй", "долбоеб", "долбоёб",
];

/// Returns `true` when `text` contains unambiguous explicit profanity.
pub fn lyrics_have_explicit(text: &str) -> bool {
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 3 {
            continue;
        }
        let w = raw.to_lowercase();
        if EXACT_EXPLICIT.contains(&w.as_str()) {
            return true;
        }
        if EXPLICIT_PREFIXES.iter().any(|p| w.starts_with(p)) {
            return true;
        }
    }
    false
}

/// True when the song title itself carries an explicit version tag
/// (`(Explicit)`, `[Explicit]`, `(Explicit Version)`) without clean-edit markers.
pub fn title_marks_explicit(title: &str) -> bool {
    let low = title.to_lowercase();
    if low.contains("non-explicit") || low.contains("non explicit") || low.contains("clean") || low.contains("radio edit") {
        return false;
    }
    let mut depth = 0usize;
    let mut inner = String::new();
    for c in low.chars() {
        match c {
            '(' | '[' => {
                if depth == 0 {
                    inner.clear();
                }
                depth += 1;
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let words: Vec<&str> = inner.split(|ch: char| !ch.is_alphanumeric()).filter(|s| !s.is_empty()).collect();
                    if words.contains(&"explicit") {
                        return true;
                    }
                }
            }
            _ if depth > 0 => inner.push(c),
            _ => {}
        }
    }
    false
}

fn lyrics_text(resp: &LyricsResponse) -> String {
    let mut out = String::new();
    if let Some(p) = &resp.plain {
        out.push_str(p);
        out.push('\n');
    }
    for line in &resp.synced {
        out.push_str(&line.text);
        out.push('\n');
    }
    out
}

/// Evaluates explicitness from already-fetched lyrics (used both by the rating
/// pipeline and by `/v1/lyrics` during playback).
pub fn rate_from_lyrics(resp: &LyricsResponse) -> Option<ExplicitRating> {
    let text = lyrics_text(resp);
    if lyrics_have_explicit(&text) {
        return Some(ExplicitRating::Explicit);
    }
    if resp.instrumental || !text.trim().is_empty() {
        return Some(ExplicitRating::Clean);
    }
    None
}

/// Persists a lyrics-derived rating into the shared cache when `/v1/lyrics`
/// fetches lyrics for playback (never downgrades an existing `Explicit` entry).
pub async fn note_lyrics_rating(svc: &Services, title: &str, artist: &str, resp: &LyricsResponse) {
    let Some(r) = rate_from_lyrics(resp) else { return };
    let k = key_for(artist, title);
    let existing = cache::get_json::<RatingEntry>(&*svc.cache, &k).await;
    let next = match existing {
        Some(mut e) => {
            let merged = e.rating.merge(r);
            if merged == e.rating {
                return;
            }
            e.rating = merged;
            e
        }
        None => RatingEntry { deezer_id: None, rating: r },
    };
    cache::put_json(&*svc.cache, &k, &next, FOUND_TTL_S).await;
}

/// Applies a cached/resolved `RatingEntry` onto `t`. Returns `Some(updated)`
/// only when `t` actually changed (rating or Deezer id).
pub fn apply(t: &Track, entry: &RatingEntry) -> Option<Track> {
    let merged = t.rating().merge(entry.rating);
    let mut out = t.clone();
    if let Some(id) = &entry.deezer_id {
        if out.source != ProviderId::Deezer {
            out.external_ids.entry(ProviderId::Deezer).or_insert_with(|| id.clone());
        }
    }
    if merged != out.rating() || (merged == ExplicitRating::Explicit && !out.explicit) {
        out.set_rating(merged);
    }
    (out != *t).then_some(out)
}

async fn lookup_deezer(svc: &Services, t: &Track) -> Result<Option<(String, ExplicitRating)>, ProviderError> {
    let q = format!("{} {}", t.artist, t.title);
    let cands = match search_provider(svc, ProviderId::Deezer, &q).await {
        Ok(c) => c,
        // Deezer is no search source here: it still rates.
        Err(ProviderError::NotConfigured) => providers::deezer::search(svc, &q).await?,
        Err(e) => return Err(e),
    };
    let best = cands
        .into_iter()
        .map(|c| (score(t, &c), c))
        .filter(|(s, _)| *s >= ACCEPT_SCORE)
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(best.map(|(_, c)| (c.track.id.clone(), c.track.rating())))
}

async fn lookup_lyrics_rating(svc: &Services, t: &Track) -> Result<Option<ExplicitRating>, ()> {
    let lkey = format!(
        "lyrics:{}:{}:{}",
        plain_words(&t.artist),
        plain_words(&t.title),
        t.duration_ms.unwrap_or(0) / 2000
    );
    if let Some(cached) = cache::get_json::<Option<LyricsResponse>>(&*svc.cache, &lkey).await {
        return Ok(cached.as_ref().and_then(rate_from_lyrics));
    }
    let query = LyricsQuery {
        title: t.title.clone(),
        artist: t.artist.clone(),
        album: t.album.clone(),
        duration_ms: t.duration_ms,
    };
    let fetched = match with_timeout(&*svc.rt, LYRICS_TIMEOUT_MS, async {
        match svc.cmds.lyrics(&query).await {
            Some(LyricsLookup::Found(l)) => Ok(Some(l)),
            Some(LyricsLookup::None) => Ok(None),
            None => providers::lrclib::lyrics(svc, &t.title, &t.artist, t.album.as_deref(), t.duration_ms)
                .await
                .map_err(|_| ()),
        }
    })
    .await
    {
        Some(res) => res,
        None => return Err(()),
    };
    match fetched {
        Ok(Some(l)) => {
            cache::put_json(&*svc.cache, &lkey, &Some(l.clone()), 7 * DAY).await;
            Ok(rate_from_lyrics(&l))
        }
        Ok(None) => {
            cache::put_json::<Option<LyricsResponse>>(&*svc.cache, &lkey, &None, HOUR).await;
            Ok(None)
        }
        Err(()) => Err(()),
    }
}

/// Multi-source rating lookup. Returns `None` only when every queried service
/// failed transiently (so nothing is cached).
async fn look_up(svc: &Services, t: &Track) -> Option<RatingEntry> {
    if title_marks_explicit(&t.title) {
        return Some(RatingEntry { deezer_id: None, rating: ExplicitRating::Explicit });
    }

    let need_deezer = !t.sources().contains(&ProviderId::Deezer) || t.explicit_rating == Some(ExplicitRating::Unknown);
    let dz_fut = async {
        if need_deezer {
            lookup_deezer(svc, t).await
        } else {
            Ok(None)
        }
    };
    let itunes_fut = providers::itunes::rate_track(svc, t);
    let (dz_res, it_res) = futures::join!(dz_fut, itunes_fut);

    let mut any_ok = false;
    let mut deezer_id: Option<String> = None;
    let mut rating = ExplicitRating::Unknown;

    if let Ok(hit) = dz_res {
        any_ok = true;
        if let Some((id, r)) = hit {
            deezer_id = Some(id);
            rating = rating.merge(r);
        }
    }
    if let Ok(hit) = it_res {
        any_ok = true;
        if let Some(r) = hit {
            rating = rating.merge(r);
        }
    }

    // Already confirmed Explicit by catalog: skip lyrics fetch.
    if rating == ExplicitRating::Explicit {
        return Some(RatingEntry { deezer_id, rating });
    }

    // Cross-check lyrics via LRCLIB to catch mislabeled clean tracks and rate
    // unrated uploads that have lyrics.
    if let Ok(lr) = lookup_lyrics_rating(svc, t).await {
        any_ok = true;
        if let Some(r) = lr {
            rating = rating.merge(r);
        }
    }

    if !any_ok {
        return None;
    }
    Some(RatingEntry { deezer_id, rating })
}

/// Applies already-cached ratings onto `tracks` without making upstream calls.
pub async fn apply_cached_ratings(svc: &Arc<Services>, tracks: &mut [Track]) {
    if tracks.is_empty() {
        return;
    }
    let keys: Vec<(usize, String)> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.rating() != ExplicitRating::Explicit)
        .map(|(i, t)| (i, key(t)))
        .collect();
    let futs: Vec<_> = keys
        .into_iter()
        .map(|(i, k)| {
            let svc = Arc::clone(svc);
            async move {
                let hit = cache::get_json::<RatingEntry>(&*svc.cache, &k).await;
                (i, hit)
            }
        })
        .collect();
    let results: Vec<_> = stream::iter(futs).buffer_unordered(12).collect().await;
    for (i, hit) in results {
        if let Some(entry) = hit {
            if let Some(updated) = apply(&tracks[i], &entry) {
                tracks[i] = updated;
            }
        }
    }
}

/// Rates tracks in `tracks` using the shared cache + multi-source lookups
/// (Deezer + Apple/iTunes + LRCLIB lyrics). Returns only tracks whose metadata
/// changed (same `source` + `id`, so clients replace them in place).
pub async fn rate_unknown(svc: &Arc<Services>, tracks: &[Track]) -> Vec<Track> {
    let mut rated = Vec::new();
    let mut uncached_unknown: Vec<&Track> = Vec::new();
    let mut uncached_clean: Vec<&Track> = Vec::new();

    // 1. Check cache for every non-explicit track first (free & fast).
    let candidates: Vec<(usize, String)> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.rating() != ExplicitRating::Explicit)
        .map(|(i, t)| (i, key(t)))
        .collect();
    let cache_checks: Vec<_> = candidates
        .into_iter()
        .map(|(i, k)| {
            let svc = Arc::clone(svc);
            async move { (i, cache::get_json::<RatingEntry>(&*svc.cache, &k).await) }
        })
        .collect();
    let checked: Vec<_> = stream::iter(cache_checks).buffer_unordered(12).collect().await;
    for (i, hit) in checked {
        let t = &tracks[i];
        match hit {
            Some(entry) => {
                if let Some(updated) = apply(t, &entry) {
                    rated.push(updated);
                }
            }
            None => {
                if t.explicit_unknown() {
                    uncached_unknown.push(t);
                } else {
                    uncached_clean.push(t);
                }
            }
        }
    }

    // 2. Spend lookup budget on unrated tracks first, then cross-check unverified
    //    catalog-clean tracks (catches mislabeled explicit songs via iTunes/lyrics).
    let mut todo: Vec<Track> = uncached_unknown.into_iter().take(MAX_LOOKUPS).cloned().collect();
    let remaining = MAX_LOOKUPS.saturating_sub(todo.len());
    if remaining > 0 {
        todo.extend(uncached_clean.into_iter().take(remaining).cloned());
    }
    if todo.is_empty() {
        return rated;
    }

    let pending: Vec<_> = todo
        .into_iter()
        .map(|t| {
            let svc = Arc::clone(svc);
            async move {
                let k = key(&t);
                let entry = look_up(&svc, &t).await?;
                let ttl = if entry.rating == ExplicitRating::Unknown { MISSING_TTL_S } else { FOUND_TTL_S };
                cache::put_json(&*svc.cache, &k, &entry, ttl).await;
                apply(&t, &entry)
            }
        })
        .collect();
    let mut lookups = stream::iter(pending).buffer_unordered(PARALLEL);
    let collect = async {
        while let Some(r) = lookups.next().await {
            rated.extend(r);
        }
    };
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
        h.http.on("itunes.apple.com/search", 200, r#"{"results":[]}"#);
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
        h.http.on("itunes.apple.com/search", 200, r#"{"results":[]}"#);
        h.http.on("lrclib.net/api/get", 404, "{}");
        h.http.on("lrclib.net/api/search", 200, "[]");
        let res = block_on(rate_unknown(&h.svc, &[upload("Cat compilation 2024")]));
        // Unknown stays unknown (explicit_rating may be populated as Some(Unknown), never clean/explicit).
        assert!(res.iter().all(|t| t.explicit_unknown()));
        // Already explicit tracks are not looked up again.
        let mut known = upload("Song");
        known.set_rating(ExplicitRating::Explicit);
        assert!(block_on(rate_unknown(&h.svc, &[known])).is_empty());
        assert_eq!(h.http.call_count("api.deezer.com/search"), 1);
    }

    #[test]
    fn lyrics_profanity_upgrades_mislabeled_clean_track() {
        let h = harness(all_secrets());
        h.http.on("itunes.apple.com/search", 200, r#"{"results":[]}"#);
        h.http.on(
            "lrclib.net/api/get",
            200,
            r#"{"instrumental":false,"plainLyrics":"Ich bin ein verdammter Hurensohn im Club"}"#,
        );
        let mut clean_dz = track("9", ProviderId::Deezer, "Mislabeled Song", "Artist", 200);
        clean_dz.set_rating(ExplicitRating::Clean);
        let rated = block_on(rate_unknown(&h.svc, &[clean_dz]));
        assert_eq!(rated.len(), 1);
        assert_eq!(rated[0].rating(), ExplicitRating::Explicit);
        assert!(rated[0].explicit);
    }

    #[test]
    fn profanity_detector_handles_languages_and_avoids_scunthorpe() {
        assert!(lyrics_have_explicit("Don't give a fuck about it"));
        assert!(lyrics_have_explicit("Motherfuckin' legend"));
        assert!(lyrics_have_explicit("Was für ein Arschloch und Hurensohn"));
        assert!(lyrics_have_explicit("Да блять, сука, это пиздец"));
        assert!(lyrics_have_explicit("Hijo de puta mierda"));
        assert!(lyrics_have_explicit("Putain de merde"));
        // Clean words that contain substrings must never trigger.
        assert!(!lyrics_have_explicit("Classic assassin pass the glass grass bass in the cockpit"));
        assert!(!lyrics_have_explicit("Ich schieße ein Tor auf dem Fußballplatz"));
        assert!(!lyrics_have_explicit("Hello sunshine what a lovely day"));
    }

    #[test]
    fn title_explicit_marker_detection() {
        assert!(title_marks_explicit("Stronger (Explicit)"));
        assert!(title_marks_explicit("Stronger [Explicit Version]"));
        assert!(!title_marks_explicit("Stronger (Clean)"));
        assert!(!title_marks_explicit("Stronger (Radio Edit)"));
        assert!(!title_marks_explicit("Stronger (Non-Explicit)"));
    }
}
