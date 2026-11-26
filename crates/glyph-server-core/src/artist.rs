//! Artist metadata, service-based verification, and server-side full catalog cache.
//!
//! - **Service-based verification** (no arbitrary fan-count threshold): an artist
//!   is verified when a platform with native verification flags confirms them
//!   (Audius `is_verified`, SoundCloud `badges.verified`) or when at least two
//!   independent official catalog/encyclopedia services confirm an exact-name
//!   artist profile (Deezer official discography, Apple Music/iTunes, Spotify,
//!   Wikipedia music biography).
//! - **Complete server-side catalog caching**: on the first visit to an artist,
//!   the server indexes popular tracks plus tracks from the artist's releases
//!   (bounded per call for Worker subrequest safety), computes `total_songs` and
//!   `total_releases`, and caches the result for 30 days. After 12 hours, the
//!   server performs a lightweight 1-request album-list check and incrementally
//!   fetches tracks only for newly added releases.

use crate::cache::{self, DAY};
use crate::merge::{normalize_title, plain_words};
use crate::providers::{self, build_url, fetch_json};
use crate::traits::{with_timeout, HttpRequest, Services};
use futures::stream::{self, StreamExt};
use glyph_protocol::{ArtistMetaItem, ArtistsMetaRequest, ArtistsMetaResponse, ExplicitRating, Track};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const ARTIST_TTL: u64 = 30 * DAY;
pub const ARTIST_META_TTL: u64 = 14 * DAY;
pub const NOT_FOUND_TTL: u64 = DAY;

/// How often a cached artist checks Deezer for newly released albums/singles.
pub const RELEASE_CHECK_INTERVAL_MS: u64 = 12 * 3600 * 1000;
/// Minimum interval between incremental catalog-indexing passes on huge discographies.
const INDEX_COOLDOWN_MS: u64 = 15_000;

/// Per-request album tracklist indexing cap (Worker stays well inside 50 subrequests).
#[cfg(target_arch = "wasm32")]
const MAX_ALBUMS_PER_CALL: usize = 14;
#[cfg(not(target_arch = "wasm32"))]
const MAX_ALBUMS_PER_CALL: usize = 50;

/// Max album pages fetched per artist (100 releases/page).
#[cfg(target_arch = "wasm32")]
const MAX_ALBUM_PAGES: usize = 2;
#[cfg(not(target_arch = "wasm32"))]
const MAX_ALBUM_PAGES: usize = 4;

/// Max uncached artists resolved in a single `POST /v1/artists/meta` batch.
#[cfg(target_arch = "wasm32")]
const MAX_META_UNCACHED: usize = 10;
#[cfg(not(target_arch = "wasm32"))]
const MAX_META_UNCACHED: usize = 25;

const MAX_META_BATCH: usize = 25;
const VERIFY_TIMEOUT_MS: u64 = 3_500;
const ALBUM_CONCURRENCY: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistResponse {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fans: Option<u64>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub similar: Vec<String>,
    /// Related artists with portrait URL and service-verified badge when known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub similar_artists: Vec<ArtistMetaItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ArtistLink>,
    /// The artist's popular songs first, followed by unique songs from their discography.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top: Vec<Track>,
    /// The discography, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub releases: Vec<ArtistRelease>,
    /// Cached total unique song count across the artist's indexed catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_songs: Option<u32>,
    /// Cached total releases count in the discography.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_releases: Option<u32>,
    /// Whether all known releases have been indexed into this cached record.
    #[serde(default)]
    pub catalog_complete: bool,
}

/// One release of the artist (Deezer album id).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistRelease {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released: Option<String>,
    /// `album`, `ep`, `single`, `compile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_type: Option<String>,
    /// Number of tracks on this release, when indexed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracks: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedArtist {
    checked_at_ms: u64,
    #[serde(default)]
    last_index_ms: u64,
    #[serde(default)]
    deezer_id: Option<u64>,
    #[serde(default)]
    indexed_release_ids: Vec<String>,
    #[serde(default)]
    known_release_ids: Vec<String>,
    response: ArtistResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedArtistMeta {
    name: String,
    #[serde(default)]
    photo: Option<String>,
    verified: bool,
}

const MUSIC_WORDS: &[&str] = &[
    "musician", "singer", "rapper", "band", "duo", "trio", "group", "dj", "producer", "songwriter", "composer",
    "vocalist", "guitarist", "drummer", "pianist", "record", "hip hop", "music", "musiker", "sänger", "sängerin",
    "komponist", "produzent", "рэп", "репер", "певец", "певица", "музыкант", "группа", "исполнитель", "продюсер",
    "композитор", "диджей", "автор песен",
];

pub(crate) fn is_musical(desc: &str) -> bool {
    let d = desc.to_lowercase();
    MUSIC_WORDS.iter().any(|w| d.contains(w))
}

pub(crate) fn names_match(name: &str, title: &str) -> bool {
    let n = plain_words(name);
    let t = plain_words(title.split(" (").next().unwrap_or(title));
    !n.is_empty() && n == t
}

fn meta_key(name: &str) -> String {
    format!("artist-meta:v1:{}", plain_words(name))
}

fn artist_key(lang: &str, name: &str) -> String {
    format!("artist:v3:{}:{}", lang.to_ascii_lowercase(), plain_words(name))
}

/// Service-level verification signals (no fan-count threshold).
#[derive(Default)]
struct VerifySignals {
    audius_verified: Option<bool>,
    soundcloud_verified: Option<bool>,
    deezer_official: Option<bool>,
    itunes_official: Option<bool>,
    spotify_official: Option<bool>,
    wiki_musical: bool,
}

/// Decides artist verification solely from service responses.
/// Returns `None` when upstream calls failed so callers never cache a transient error as `false`.
fn evaluate_verified(s: &VerifySignals) -> Option<bool> {
    if s.audius_verified == Some(true) || s.soundcloud_verified == Some(true) {
        return Some(true);
    }
    let catalog_confirmations = usize::from(s.deezer_official == Some(true))
        + usize::from(s.itunes_official == Some(true))
        + usize::from(s.spotify_official == Some(true))
        + usize::from(s.wiki_musical);
    if catalog_confirmations >= 2 {
        return Some(true);
    }
    let answered = usize::from(s.audius_verified.is_some())
        + usize::from(s.soundcloud_verified.is_some())
        + usize::from(s.deezer_official.is_some())
        + usize::from(s.itunes_official.is_some())
        + usize::from(s.spotify_official.is_some())
        + usize::from(s.wiki_musical);
    if answered >= 2 || (answered >= 1 && s.deezer_official == Some(false) && s.itunes_official != Some(true)) {
        Some(false)
    } else {
        None
    }
}

#[derive(Deserialize)]
struct DzSearchResp {
    #[serde(default)]
    data: Vec<DzArtistItem>,
}

#[derive(Deserialize)]
struct DzArtistItem {
    #[serde(default, deserialize_with = "crate::providers::de_u64_lenient")]
    id: Option<u64>,
    #[serde(default)]
    name: String,
    link: Option<String>,
    picture: Option<String>,
    picture_medium: Option<String>,
    picture_big: Option<String>,
    picture_xl: Option<String>,
    #[serde(default, deserialize_with = "crate::providers::de_u64_lenient")]
    nb_fan: Option<u64>,
    #[serde(default, deserialize_with = "crate::providers::de_u64_lenient")]
    nb_album: Option<u64>,
}

struct DzFound {
    name: String,
    photo: Option<String>,
    fans: Option<u64>,
    nb_album: Option<u64>,
    link: Option<String>,
    id: Option<u64>,
}

async fn lookup_deezer(svc: &Services, name: &str) -> Result<Option<DzFound>, crate::providers::ProviderError> {
    let url = build_url("https://api.deezer.com/search/artist", &[("q", name), ("limit", "10")]);
    let resp: DzSearchResp = fetch_json(svc, HttpRequest::get(url)).await?;
    let want = plain_words(name);
    let best = resp
        .data
        .into_iter()
        .filter(|a| plain_words(&a.name) == want)
        .max_by_key(|a| (a.nb_album.unwrap_or(0) > 0, a.nb_fan.unwrap_or(0)));
    Ok(best.map(|best| {
        let photo = best.picture_xl.or(best.picture_big).or(best.picture_medium).or(best.picture);
        DzFound {
            name: best.name,
            photo,
            fans: best.nb_fan,
            nb_album: best.nb_album,
            link: best.link,
            id: best.id,
        }
    }))
}

#[derive(Deserialize)]
struct DzList<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct DzRelease {
    #[serde(default, deserialize_with = "crate::providers::de_u64_lenient")]
    id: Option<u64>,
    #[serde(default)]
    title: String,
    cover_xl: Option<String>,
    cover_big: Option<String>,
    release_date: Option<String>,
    record_type: Option<String>,
}

#[derive(Deserialize)]
struct DzRelatedArtist {
    #[serde(default)]
    name: String,
    picture_xl: Option<String>,
    picture_big: Option<String>,
    picture_medium: Option<String>,
}

async fn fetch_deezer_albums(svc: &Services, id: u64, max_pages: usize) -> Vec<ArtistRelease> {
    let base = format!("https://api.deezer.com/artist/{id}/albums");
    let mut raw: Vec<ArtistRelease> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for page in 0..max_pages {
        let index = (page * 100).to_string();
        let url = build_url(&base, &[("limit", "100"), ("index", &index)]);
        let Ok(list) = fetch_json::<DzList<DzRelease>>(svc, HttpRequest::get(url)).await else {
            break;
        };
        let count = list.data.len();
        for r in list.data {
            let Some(rid) = r.id.map(|v| v.to_string()) else { continue };
            let title = r.title.trim().to_string();
            if title.is_empty() || !seen_ids.insert(rid.clone()) {
                continue;
            }
            raw.push(ArtistRelease {
                id: rid,
                title,
                cover_url: r.cover_xl.or(r.cover_big),
                released: r.release_date.filter(|d| d.len() == 10 && !d.starts_with("0000")),
                record_type: r.record_type,
                tracks: None,
            });
        }
        if count < 100 || list.next.is_none() {
            break;
        }
    }
    raw.sort_by(|a, b| b.released.cmp(&a.released));
    let mut seen_titles = HashSet::new();
    raw.retain(|r| seen_titles.insert(r.title.to_lowercase()));
    raw
}

async fn fetch_deezer_top(svc: &Services, id: u64) -> Vec<Track> {
    let url = build_url(&format!("https://api.deezer.com/artist/{id}/top"), &[("limit", "50")]);
    fetch_json::<DzList<providers::deezer::DzTrack>>(svc, HttpRequest::get(url))
        .await
        .map(|l| l.data.into_iter().map(|t| providers::deezer::to_candidate(t).track).collect())
        .unwrap_or_default()
}

async fn fetch_deezer_related(svc: &Services, id: u64) -> Vec<(String, Option<String>)> {
    let url = build_url(&format!("https://api.deezer.com/artist/{id}/related"), &[("limit", "20")]);
    fetch_json::<DzList<DzRelatedArtist>>(svc, HttpRequest::get(url))
        .await
        .map(|l| {
            l.data
                .into_iter()
                .filter_map(|a| {
                    let name = a.name.trim().to_string();
                    if name.is_empty() {
                        return None;
                    }
                    let photo = a.picture_xl.or(a.picture_big).or(a.picture_medium);
                    Some((name, photo))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fetches tracks of a single Deezer album/EP/single release.
async fn fetch_release_tracks(svc: &Services, release: &ArtistRelease) -> Option<Vec<Track>> {
    let url = build_url(&format!("https://api.deezer.com/album/{}/tracks", release.id), &[("limit", "100")]);
    let list: DzList<providers::deezer::DzTrack> = fetch_json(svc, HttpRequest::get(url)).await.ok()?;
    let tracks: Vec<Track> = list
        .data
        .into_iter()
        .map(|t| {
            let mut tr = providers::deezer::to_candidate(t).track;
            if tr.album.as_deref().is_none_or(|a| a.trim().is_empty()) {
                tr.album = Some(release.title.clone());
            }
            if tr.cover_url.is_none() {
                tr.cover_url = release.cover_url.clone();
            }
            tr
        })
        .collect();
    Some(tracks)
}

/// Priority when selecting which releases to index first: Albums -> EPs -> Singles -> Compilations.
fn release_priority(record_type: Option<&str>) -> u8 {
    match record_type.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("album") => 0,
        Some("ep") => 1,
        Some("single") => 2,
        _ => 3,
    }
}

/// Merges `incoming` tracks into `target`, deduplicating by ISRC or normalized title
/// while preserving `target`'s existing order (popular tracks stay first) and
/// upgrading explicitness ratings when an explicit edition exists.
fn merge_artist_tracks(target: &mut Vec<Track>, incoming: Vec<Track>) {
    let mut by_isrc: HashMap<String, usize> = HashMap::new();
    let mut by_title: HashMap<String, usize> = HashMap::new();
    for (i, t) in target.iter().enumerate() {
        if let Some(isrc) = t.isrc.as_ref().filter(|s| !s.is_empty()) {
            by_isrc.insert(isrc.to_ascii_uppercase(), i);
        }
        let nt = normalize_title(&t.title);
        if !nt.is_empty() {
            by_title.entry(nt).or_insert(i);
        }
    }
    for tr in incoming {
        let isrc_key = tr.isrc.as_ref().filter(|s| !s.is_empty()).map(|s| s.to_ascii_uppercase());
        let nt = normalize_title(&tr.title);
        let existing_idx = isrc_key
            .as_ref()
            .and_then(|i| by_isrc.get(i).copied())
            .or_else(|| (!nt.is_empty()).then(|| by_title.get(&nt).copied()).flatten());
        if let Some(idx) = existing_idx {
            let cur = &mut target[idx];
            let merged = cur.rating().merge(tr.rating());
            if merged != cur.rating() || (merged == ExplicitRating::Explicit && !cur.explicit) {
                cur.set_rating(merged);
            }
            if cur.album.is_none() {
                cur.album = tr.album;
            }
            if cur.cover_url.is_none() {
                cur.cover_url = tr.cover_url;
            }
            if cur.isrc.is_none() {
                cur.isrc = tr.isrc;
            }
        } else {
            let idx = target.len();
            if let Some(i) = isrc_key {
                by_isrc.insert(i, idx);
            }
            if !nt.is_empty() {
                by_title.insert(nt, idx);
            }
            target.push(tr);
        }
    }
}

/// Fetches tracklists for up to `max_albums` unindexed releases, merges unique songs
/// into `tracks`, updates `release.tracks` counts and `indexed_ids`, and returns
/// `true` when all releases are now indexed.
async fn index_releases(
    svc: &Arc<Services>,
    releases: &mut [ArtistRelease],
    indexed_ids: &mut Vec<String>,
    tracks: &mut Vec<Track>,
    max_albums: usize,
) -> bool {
    let done_set: HashSet<String> = indexed_ids.iter().cloned().collect();
    let mut todo_indices: Vec<usize> = releases
        .iter()
        .enumerate()
        .filter(|(_, r)| !done_set.contains(&r.id))
        .map(|(i, _)| i)
        .collect();
    if todo_indices.is_empty() {
        return true;
    }
    todo_indices.sort_by_key(|&i| (release_priority(releases[i].record_type.as_deref()), i));
    let batch: Vec<(usize, ArtistRelease)> = todo_indices
        .iter()
        .take(max_albums)
        .map(|&i| (i, releases[i].clone()))
        .collect();
    let futs: Vec<_> = batch
        .into_iter()
        .map(|(idx, rel)| {
            let svc = Arc::clone(svc);
            async move {
                let fetched = fetch_release_tracks(&svc, &rel).await;
                (idx, rel.id, fetched)
            }
        })
        .collect();
    let mut results: Vec<_> = stream::iter(futs).buffer_unordered(ALBUM_CONCURRENCY).collect().await;
    // Merge newest releases in deterministic order.
    results.sort_by_key(|(idx, _, _)| *idx);
    let mut fetched_any_failed = false;
    for (idx, rid, res) in results {
        match res {
            Some(album_tracks) => {
                releases[idx].tracks = Some(album_tracks.len() as u32);
                merge_artist_tracks(tracks, album_tracks);
                if !indexed_ids.contains(&rid) {
                    indexed_ids.push(rid);
                }
            }
            None => {
                fetched_any_failed = true;
            }
        }
    }
    !fetched_any_failed && indexed_ids.len() >= releases.len()
}

/// Hydrates `similar_artists` from the lightweight `artist-meta:v1:` cache (0 upstream calls).
async fn hydrate_similar_meta(svc: &Arc<Services>, items: &mut [ArtistMetaItem]) {
    if items.is_empty() {
        return;
    }
    let keys: Vec<(usize, String)> = items
        .iter()
        .enumerate()
        .map(|(i, item)| (i, meta_key(&item.name)))
        .collect();
    let futs: Vec<_> = keys
        .into_iter()
        .map(|(i, k)| {
            let svc = Arc::clone(svc);
            async move { (i, cache::get_json::<CachedArtistMeta>(&*svc.cache, &k).await) }
        })
        .collect();
    let hits: Vec<_> = stream::iter(futs).buffer_unordered(12).collect().await;
    for (i, hit) in hits {
        if let Some(m) = hit {
            items[i].verified = Some(m.verified);
            if items[i].photo.is_none() {
                items[i].photo = m.photo;
            }
        }
    }
}

struct WikiArtist {
    bio: String,
    url: String,
    original_img: Option<String>,
    thumb_img: Option<String>,
}

async fn lookup_wikipedia(svc: &Services, name: &str, lang: &str) -> Option<WikiArtist> {
    let lang = if matches!(lang, "en" | "de" | "ru") { lang } else { "en" };
    let mut langs = vec![lang];
    if lang != "en" {
        langs.push("en");
    }
    for l in langs {
        if let Some(res) = lookup_wikipedia_in(svc, name, l).await {
            return Some(res);
        }
    }
    None
}

async fn lookup_wikipedia_in(svc: &Services, name: &str, lang: &str) -> Option<WikiArtist> {
    let search_url = build_url(
        &format!("https://{lang}.wikipedia.org/w/api.php"),
        &[("action", "query"), ("list", "search"), ("srsearch", name), ("srlimit", "5"), ("format", "json")],
    );
    let found: Value = fetch_json(svc, HttpRequest::get(search_url)).await.ok()?;
    let titles: Vec<String> = found["query"]["search"]
        .as_array()?
        .iter()
        .filter_map(|h| h["title"].as_str().map(str::to_string))
        .collect();

    for title in titles.into_iter().filter(|t| names_match(name, t)) {
        let mut summary_url = match url::Url::parse(&format!("https://{lang}.wikipedia.org/api/rest_v1/page/summary/")) {
            Ok(u) => u,
            Err(_) => continue,
        };
        if let Ok(mut segs) = summary_url.path_segments_mut() {
            segs.pop_if_empty().push(&title.replace(' ', "_"));
        }
        let s: Value = fetch_json(svc, HttpRequest::get(summary_url.as_str())).await.ok()?;
        if s["type"].as_str() == Some("disambiguation") {
            continue;
        }
        if !is_musical(s["description"].as_str().unwrap_or("")) {
            continue;
        }
        let bio = s["extract"].as_str().unwrap_or("").trim().to_string();
        if bio.is_empty() {
            continue;
        }
        let page = s["content_urls"]["desktop"]["page"].as_str().unwrap_or("").to_string();
        let original_img = s["originalimage"]["source"].as_str().map(str::to_string);
        let thumb_img = s["thumbnail"]["source"].as_str().map(str::to_string);
        return Some(WikiArtist { bio, url: page, original_img, thumb_img });
    }
    None
}

/// Lookup full artist metadata and catalog, checking `Services::cache` first and
/// performing server-side incremental new-release updates automatically.
pub async fn lookup(svc: &Arc<Services>, name: &str, lang: &str) -> Result<ArtistResponse, crate::ApiErr> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::ApiErr::bad_request("name is required"));
    }
    let key = artist_key(lang, name);
    let now = svc.rt.now_ms();

    if let Some(mut cached) = cache::get_json::<CachedArtist>(&*svc.cache, &key).await {
        let mut dirty = false;

        // 1. Periodic server-side check for newly released albums/singles (1 lightweight call).
        if let Some(dz_id) = cached.deezer_id {
            if now.saturating_sub(cached.checked_at_ms) >= RELEASE_CHECK_INTERVAL_MS {
                let latest = fetch_deezer_albums(svc, dz_id, 1).await;
                if !latest.is_empty() {
                    let known: HashSet<String> = cached.known_release_ids.iter().cloned().collect();
                    let new_ones: Vec<ArtistRelease> = latest
                        .into_iter()
                        .filter(|r| !known.contains(&r.id))
                        .collect();
                    if !new_ones.is_empty() {
                        for r in &new_ones {
                            cached.known_release_ids.push(r.id.clone());
                        }
                        // Merge new releases in front, sorted newest first.
                        let mut merged_releases = new_ones;
                        merged_releases.extend(cached.response.releases);
                        merged_releases.sort_by(|a, b| b.released.cmp(&a.released));
                        let mut seen_titles = HashSet::new();
                        merged_releases.retain(|r| seen_titles.insert(r.title.to_lowercase()));
                        cached.response.releases = merged_releases;
                        // Refresh top hits so new singles appear at the top while keeping full catalog below.
                        let mut fresh_top = fetch_deezer_top(svc, dz_id).await;
                        if !fresh_top.is_empty() {
                            let existing = std::mem::take(&mut cached.response.top);
                            merge_artist_tracks(&mut fresh_top, existing);
                            cached.response.top = fresh_top;
                        }
                        cached.response.catalog_complete = index_releases(
                            svc,
                            &mut cached.response.releases,
                            &mut cached.indexed_release_ids,
                            &mut cached.response.top,
                            MAX_ALBUMS_PER_CALL,
                        )
                        .await;
                        cached.response.total_songs = Some(cached.response.top.len() as u32);
                        cached.response.total_releases = Some(cached.response.releases.len() as u32);
                        cached.last_index_ms = now;
                    }
                }
                cached.checked_at_ms = now;
                dirty = true;
            } else if !cached.response.catalog_complete
                && now.saturating_sub(cached.last_index_ms) >= INDEX_COOLDOWN_MS
            {
                // 2. Continue indexing remaining releases on large discographies (server-throttled).
                cached.response.catalog_complete = index_releases(
                    svc,
                    &mut cached.response.releases,
                    &mut cached.indexed_release_ids,
                    &mut cached.response.top,
                    MAX_ALBUMS_PER_CALL,
                )
                .await;
                cached.response.total_songs = Some(cached.response.top.len() as u32);
                cached.response.total_releases = Some(cached.response.releases.len() as u32);
                cached.last_index_ms = now;
                dirty = true;
            }
        }

        // Apply latest cached explicit ratings & similar-artist verification badges (0 upstream calls).
        crate::rating::apply_cached_ratings(svc, &mut cached.response.top).await;
        hydrate_similar_meta(svc, &mut cached.response.similar_artists).await;

        if dirty {
            cache::put_json(&*svc.cache, &key, &cached, ARTIST_TTL).await;
        }
        return Ok(cached.response);
    }

    // Cold cache: fetch Deezer profile, Wikipedia bio, and service-level verification in parallel.
    let dz_fut = lookup_deezer(svc, name);
    let wiki_fut = lookup_wikipedia(svc, name, lang);
    let itunes_fut = with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::itunes::lookup_artist(svc, name));
    let audius_fut = with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::audius::lookup_artist(svc, name));
    let sc_fut = async {
        if svc.config.provider_enabled(glyph_protocol::ProviderId::SoundCloud) {
            with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::soundcloud::is_artist_verified(svc, name))
                .await
                .and_then(Result::ok)
        } else {
            None
        }
    };
    let sp_fut = async {
        if svc.config.provider_enabled(glyph_protocol::ProviderId::Spotify) {
            with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::spotify::artist_exists(svc, name))
                .await
                .and_then(Result::ok)
        } else {
            None
        }
    };
    let (dz_res, wiki, itunes_res, audius_res, sc_ver, sp_res) =
        futures::join!(dz_fut, wiki_fut, itunes_fut, audius_fut, sc_fut, sp_fut);

    let dz = dz_res.as_ref().ok().and_then(|o| o.as_ref());
    let itunes = itunes_res.as_ref().and_then(|r| r.as_ref().ok()).and_then(|o| o.as_ref());
    let audius = audius_res.as_ref().and_then(|r| r.as_ref().ok()).and_then(|o| o.as_ref());

    // Fetch Deezer top songs, related artists, and complete release list in parallel.
    let (mut top, related_raw, mut releases) = match dz.and_then(|f| f.id) {
        Some(id) => futures::join!(
            fetch_deezer_top(svc, id),
            fetch_deezer_related(svc, id),
            fetch_deezer_albums(svc, id, MAX_ALBUM_PAGES),
        ),
        None => (Vec::new(), Vec::new(), Vec::new()),
    };

    // Index the artist's discography into `top` so we have real song counts & complete tracks.
    let known_release_ids: Vec<String> = releases.iter().map(|r| r.id.clone()).collect();
    let mut indexed_release_ids: Vec<String> = Vec::new();
    let catalog_complete = if !releases.is_empty() {
        index_releases(svc, &mut releases, &mut indexed_release_ids, &mut top, MAX_ALBUMS_PER_CALL).await
    } else {
        true
    };

    // Apply cached explicit ratings onto the artist's songs.
    crate::rating::apply_cached_ratings(svc, &mut top).await;

    // Service-based verification (zero fan-count threshold).
    let signals = VerifySignals {
        audius_verified: match &audius_res {
            Some(Ok(Some(a))) => Some(a.is_verified),
            Some(Ok(None)) => Some(false),
            _ => None,
        },
        soundcloud_verified: sc_ver,
        deezer_official: match &dz_res {
            Ok(Some(d)) => Some(d.nb_album.unwrap_or(1) > 0 || !top.is_empty() || !releases.is_empty()),
            Ok(None) => Some(false),
            Err(_) => None,
        },
        itunes_official: match &itunes_res {
            Some(Ok(Some(_))) => Some(true),
            Some(Ok(None)) => Some(false),
            _ => None,
        },
        spotify_official: sp_res.as_ref().map(|o| o.is_some()),
        wiki_musical: wiki.is_some(),
    };
    let verified_opt = evaluate_verified(&signals);
    let verified = verified_opt.unwrap_or(false);

    let canonical_name = dz
        .map(|d| d.name.as_str())
        .or_else(|| itunes.map(|a| a.name.as_str()))
        .or_else(|| audius.map(|a| a.name.as_str()))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(name)
        .to_string();

    let dz_photo = dz.and_then(|f| f.photo.clone());
    let audius_photo = audius.and_then(|a| a.photo.clone());
    let fans = dz.and_then(|f| f.fans);
    let dz_link = dz.and_then(|f| f.link.clone());

    let (bio, wiki_url, wiki_orig, wiki_thumb) = match wiki {
        Some(w) => (Some(w.bio), Some(w.url), w.original_img, w.thumb_img),
        None => (None, None, None, None),
    };
    let wiki_img = wiki_orig.or(wiki_thumb);
    let primary_photo = dz_photo.or(audius_photo);
    let (photo, banner) = match (&primary_photo, &wiki_img) {
        (Some(dp), Some(wp)) => (Some(dp.clone()), Some(wp.clone())),
        (Some(dp), None) => (Some(dp.clone()), Some(dp.clone())),
        (None, Some(wp)) => (Some(wp.clone()), Some(wp.clone())),
        (None, None) => (None, None),
    };

    let mut links = Vec::new();
    if let Some(u) = &wiki_url {
        links.push(ArtistLink { label: "Wikipedia".into(), url: u.clone() });
    }
    if let Some(u) = &dz_link {
        links.push(ArtistLink { label: "Deezer".into(), url: u.clone() });
    }
    if let Some(u) = itunes.and_then(|a| a.url.as_ref()) {
        links.push(ArtistLink { label: "Apple Music".into(), url: u.clone() });
    }
    if let Some(u) = sp_res.as_ref().and_then(|o| o.as_ref()).and_then(|(_, _, u)| u.as_ref()) {
        links.push(ArtistLink { label: "Spotify".into(), url: u.clone() });
    }
    if let Some(u) = audius.and_then(|a| a.url.as_ref()) {
        links.push(ArtistLink { label: "Audius".into(), url: u.clone() });
    }
    let url = wiki_url.or(dz_link).or_else(|| links.first().map(|l| l.url.clone()));

    let mut tags: Vec<String> = Vec::new();
    let mut push_tag = |g: &str| {
        let g = g.trim();
        if !g.is_empty() && !tags.iter().any(|x| x.eq_ignore_ascii_case(g)) {
            tags.push(g.to_string());
        }
    };
    if let Some(g) = itunes.and_then(|a| a.genre.as_deref()) {
        push_tag(g);
    }
    if let Some((_, genres, _)) = sp_res.as_ref().and_then(|o| o.as_ref()) {
        for g in genres.iter().take(4) {
            push_tag(g);
        }
    }

    let similar: Vec<String> = related_raw.iter().map(|(n, _)| n.clone()).collect();
    let mut similar_artists: Vec<ArtistMetaItem> = related_raw
        .into_iter()
        .map(|(n, p)| ArtistMetaItem { name: n, photo: p, verified: None })
        .collect();
    hydrate_similar_meta(svc, &mut similar_artists).await;

    let total_songs = (!top.is_empty()).then_some(top.len() as u32);
    let total_releases = (!releases.is_empty()).then_some(releases.len() as u32);
    let is_empty = bio.is_none() && photo.is_none() && fans.is_none() && top.is_empty() && releases.is_empty();
    let ttl = if is_empty { NOT_FOUND_TTL } else { ARTIST_TTL };

    let resp = ArtistResponse {
        name: canonical_name.clone(),
        bio,
        url,
        photo: photo.clone(),
        banner,
        fans,
        verified,
        tags,
        similar,
        similar_artists,
        links,
        top,
        releases,
        total_songs,
        total_releases,
        catalog_complete,
    };

    let cached = CachedArtist {
        checked_at_ms: now,
        last_index_ms: now,
        deezer_id: dz.and_then(|f| f.id),
        indexed_release_ids,
        known_release_ids,
        response: resp.clone(),
    };
    cache::put_json(&*svc.cache, &key, &cached, ttl).await;

    // Also update the lightweight artist-meta cache whenever verification was definitive.
    if let Some(v) = verified_opt {
        let meta = CachedArtistMeta { name: canonical_name, photo, verified: v };
        cache::put_json(&*svc.cache, &meta_key(name), &meta, ARTIST_META_TTL).await;
    }

    Ok(resp)
}

/// Lightweight service-based verification + portrait lookup for a single artist
/// (used by `POST /v1/artists/meta`). Never caches transient errors as `false`.
async fn resolve_meta_uncached(svc: &Services, name: &str) -> ArtistMetaItem {
    let dz_fut = with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, lookup_deezer(svc, name));
    let it_fut = with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::itunes::lookup_artist(svc, name));
    let au_fut = with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::audius::lookup_artist(svc, name));
    let sc_fut = async {
        if svc.config.provider_enabled(glyph_protocol::ProviderId::SoundCloud) {
            with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::soundcloud::is_artist_verified(svc, name))
                .await
                .and_then(Result::ok)
        } else {
            None
        }
    };
    let sp_fut = async {
        if svc.config.provider_enabled(glyph_protocol::ProviderId::Spotify) {
            with_timeout(&*svc.rt, VERIFY_TIMEOUT_MS, providers::spotify::artist_exists(svc, name))
                .await
                .and_then(Result::ok)
        } else {
            None
        }
    };
    let (dz_res, it_res, au_res, sc_ver, sp_res) = futures::join!(dz_fut, it_fut, au_fut, sc_fut, sp_fut);

    let dz = dz_res.as_ref().and_then(|r| r.as_ref().ok()).and_then(|o| o.as_ref());
    let au = au_res.as_ref().and_then(|r| r.as_ref().ok()).and_then(|o| o.as_ref());
    let photo = dz.and_then(|d| d.photo.clone()).or_else(|| au.and_then(|a| a.photo.clone()));

    let signals = VerifySignals {
        audius_verified: match &au_res {
            Some(Ok(Some(a))) => Some(a.is_verified),
            Some(Ok(None)) => Some(false),
            _ => None,
        },
        soundcloud_verified: sc_ver,
        deezer_official: match &dz_res {
            Some(Ok(Some(d))) => Some(d.nb_album.unwrap_or(1) > 0),
            Some(Ok(None)) => Some(false),
            _ => None,
        },
        itunes_official: match &it_res {
            Some(Ok(Some(_))) => Some(true),
            Some(Ok(None)) => Some(false),
            _ => None,
        },
        spotify_official: sp_res.as_ref().map(|o| o.is_some()),
        wiki_musical: false,
    };
    let verified = evaluate_verified(&signals);
    if let Some(v) = verified {
        let meta = CachedArtistMeta { name: name.to_string(), photo: photo.clone(), verified: v };
        cache::put_json(&*svc.cache, &meta_key(name), &meta, ARTIST_META_TTL).await;
    }
    ArtistMetaItem { name: name.to_string(), photo, verified }
}

/// `POST /v1/artists/meta`: batch service-based verification and portrait lookup.
pub async fn batch_meta(svc: &Arc<Services>, req: ArtistsMetaRequest) -> Result<ArtistsMetaResponse, crate::ApiErr> {
    let mut seen = HashSet::new();
    let names: Vec<String> = req
        .names
        .into_iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty() && n.len() <= 160 && seen.insert(plain_words(n)))
        .take(MAX_META_BATCH)
        .collect();
    if names.is_empty() {
        return Ok(ArtistsMetaResponse { items: Vec::new() });
    }

    // 1. Check lightweight artist-meta cache first for all requested names.
    let cache_keys: Vec<(String, String)> = names.iter().map(|n| (n.clone(), meta_key(n))).collect();
    let cache_futs: Vec<_> = cache_keys
        .into_iter()
        .map(|(n, k)| {
            let svc = Arc::clone(svc);
            async move { (n, cache::get_json::<CachedArtistMeta>(&*svc.cache, &k).await) }
        })
        .collect();
    let checked: Vec<_> = stream::iter(cache_futs).buffer_unordered(12).collect().await;

    let mut out: Vec<Option<ArtistMetaItem>> = vec![None; checked.len()];
    let mut todo: Vec<(usize, String)> = Vec::new();
    for (i, (name, hit)) in checked.into_iter().enumerate() {
        if let Some(m) = hit {
            out[i] = Some(ArtistMetaItem { name, photo: m.photo, verified: Some(m.verified) });
        } else if todo.len() < MAX_META_UNCACHED {
            todo.push((i, name));
        } else {
            out[i] = Some(ArtistMetaItem { name, photo: None, verified: None });
        }
    }

    // 2. Resolve remaining uncached names in bounded parallel batches.
    if !todo.is_empty() {
        let futs: Vec<_> = todo
            .into_iter()
            .map(|(i, name)| {
                let svc = Arc::clone(svc);
                async move { (i, resolve_meta_uncached(&svc, &name).await) }
            })
            .collect();
        let resolved: Vec<_> = stream::iter(futs).buffer_unordered(4).collect().await;
        for (i, item) in resolved {
            out[i] = Some(item);
        }
    }

    Ok(ArtistsMetaResponse {
        items: out.into_iter().flatten().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn disambiguation_and_matching() {
        assert!(is_musical("English rock band"));
        assert!(is_musical("deutscher Rapper"));
        assert!(!is_musical("Argentine footballer"));
        assert!(!is_musical("city in Germany"));
        assert!(names_match("Daft Punk", "Daft Punk"));
        assert!(names_match("Muse", "Muse (band)"));
        assert!(!names_match("Muse", "Museum of Modern Art"));
    }

    #[test]
    fn service_verification_needs_no_fan_threshold() {
        // Native Audius verification alone verifies even small artists.
        assert_eq!(
            evaluate_verified(&VerifySignals { audius_verified: Some(true), ..Default::default() }),
            Some(true)
        );
        // Official Deezer + Apple Music catalog confirms artist regardless of fan count.
        assert_eq!(
            evaluate_verified(&VerifySignals {
                deezer_official: Some(true),
                itunes_official: Some(true),
                ..Default::default()
            }),
            Some(true)
        );
        // Single uncorroborated profile is not verified.
        assert_eq!(
            evaluate_verified(&VerifySignals {
                deezer_official: Some(true),
                itunes_official: Some(false),
                audius_verified: Some(false),
                ..Default::default()
            }),
            Some(false)
        );
        // All transient failures yield None (never cached as false).
        assert_eq!(evaluate_verified(&VerifySignals::default()), None);
    }

    #[test]
    fn artist_lookup_merges_catalog_and_incrementally_adds_new_albums() {
        let h = harness(Default::default());
        h.http.on(
            "api.deezer.com/search/artist",
            200,
            r#"{"data":[{"id":1,"name":"Daft Punk","picture_xl":"https://dz/xl.jpg","picture_big":"https://dz/big.jpg","nb_fan":4000000,"nb_album":4,"link":"https://deezer.com/artist/1"}]}"#,
        );
        h.http.on(
            "api.deezer.com/artist/1/top",
            200,
            r#"{"data":[{"id":10,"title":"One More Time","duration":320,"artist":{"name":"Daft Punk"},"album":{"title":"Discovery"}}]}"#,
        );
        h.http.on(
            "api.deezer.com/artist/1/related",
            200,
            r#"{"data":[{"name":"Justice","picture_big":"https://dz/justice.jpg"}]}"#,
        );
        h.http.on(
            "api.deezer.com/artist/1/albums",
            200,
            r#"{"data":[{"id":100,"title":"Discovery","cover_big":"https://dz/disc.jpg","release_date":"2001-03-12","record_type":"album"}]}"#,
        );
        h.http.on(
            "api.deezer.com/album/100/tracks",
            200,
            r#"{"data":[
              {"id":10,"title":"One More Time","duration":320,"artist":{"name":"Daft Punk"}},
              {"id":11,"title":"Aerodynamic","duration":212,"artist":{"name":"Daft Punk"}},
              {"id":12,"title":"Digital Love","duration":301,"artist":{"name":"Daft Punk"}}
            ]}"#,
        );
        h.http.on(
            "en.wikipedia.org/w/api.php",
            200,
            r#"{"query":{"search":[{"title":"Daft Punk"}]}}"#,
        );
        h.http.on(
            "en.wikipedia.org/api/rest_v1/page/summary/Daft_Punk",
            200,
            r#"{"type":"standard","description":"French electronic music duo","extract":"Daft Punk were a French electronic music duo.","content_urls":{"desktop":{"page":"https://en.wikipedia.org/wiki/Daft_Punk"}},"originalimage":{"source":"https://commons/orig.jpg","width":1920,"height":1080}}"#,
        );
        h.http.on(
            "itunes.apple.com/search",
            200,
            r#"{"results":[{"artistName":"Daft Punk","artistLinkUrl":"https://music.apple.com/us/artist/5468295","primaryGenreName":"Electronic"}]}"#,
        );
        h.http.on("audius.co/v1/users/search", 200, r#"{"data":[]}"#);

        let res = block_on(lookup(&h.svc, "Daft Punk", "en")).unwrap();
        assert_eq!(res.name, "Daft Punk");
        assert_eq!(res.bio.as_deref(), Some("Daft Punk were a French electronic music duo."));
        assert_eq!(res.photo.as_deref(), Some("https://dz/xl.jpg"));
        assert_eq!(res.banner.as_deref(), Some("https://commons/orig.jpg"));
        assert_eq!(res.fans, Some(4_000_000));
        assert!(res.verified);
        assert_eq!(res.top.len(), 3);
        assert_eq!(res.total_songs, Some(3));
        assert_eq!(res.total_releases, Some(1));
        assert!(res.catalog_complete);
        assert_eq!(res.releases[0].tracks, Some(3));
        assert_eq!(res.tags, vec!["Electronic"]);
        assert_eq!(res.similar_artists.len(), 1);
        assert_eq!(res.similar_artists[0].photo.as_deref(), Some("https://dz/justice.jpg"));

        // Warm hit makes 0 extra Deezer album calls.
        let cached = block_on(lookup(&h.svc, "Daft Punk", "en")).unwrap();
        assert_eq!(cached, res);
        assert_eq!(h.http.call_count("api.deezer.com/artist/1/albums"), 1);

        // After 12+ hours, a newly released album is detected and incrementally merged!
        h.rt.advance(RELEASE_CHECK_INTERVAL_MS + 1_000);
        h.http.rules.lock().unwrap().retain(|(n, _, _)| n != "api.deezer.com/artist/1/albums");
        h.http.on(
            "api.deezer.com/artist/1/albums",
            200,
            r#"{"data":[
              {"id":200,"title":"Random Access Memories","cover_big":"https://dz/ram.jpg","release_date":"2013-05-17","record_type":"album"},
              {"id":100,"title":"Discovery","cover_big":"https://dz/disc.jpg","release_date":"2001-03-12","record_type":"album"}
            ]}"#,
        );
        h.http.on(
            "api.deezer.com/album/200/tracks",
            200,
            r#"{"data":[{"id":20,"title":"Get Lucky","duration":369,"artist":{"name":"Daft Punk"}}]}"#,
        );
        let refreshed = block_on(lookup(&h.svc, "Daft Punk", "en")).unwrap();
        assert_eq!(refreshed.total_songs, Some(4));
        assert_eq!(refreshed.total_releases, Some(2));
        assert_eq!(refreshed.top.len(), 4);
        assert!(refreshed.top.iter().any(|t| t.title == "Get Lucky"));
        // Old album 100 was NOT fetched a second time!
        assert_eq!(h.http.call_count("api.deezer.com/album/100/tracks"), 1);
        assert_eq!(h.http.call_count("api.deezer.com/album/200/tracks"), 1);
    }

    #[test]
    fn batch_meta_verifies_and_caches_artists() {
        let h = harness(Default::default());
        h.http.on(
            "api.deezer.com/search/artist",
            200,
            r#"{"data":[{"id":5,"name":"Indie Gem","picture_big":"https://dz/indie.jpg","nb_fan":250,"nb_album":2}]}"#,
        );
        h.http.on(
            "itunes.apple.com/search",
            200,
            r#"{"results":[{"artistName":"Indie Gem","artistLinkUrl":"https://music.apple.com/us/artist/99"}]}"#,
        );
        h.http.on("audius.co/v1/users/search", 200, r#"{"data":[]}"#);

        let resp = block_on(batch_meta(&h.svc, ArtistsMetaRequest { names: vec!["Indie Gem".into()] })).unwrap();
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].verified, Some(true));
        assert_eq!(resp.items[0].photo.as_deref(), Some("https://dz/indie.jpg"));

        // Second call hits cache (no extra HTTP requests).
        let resp2 = block_on(batch_meta(&h.svc, ArtistsMetaRequest { names: vec!["Indie Gem".into()] })).unwrap();
        assert_eq!(resp2, resp);
        assert_eq!(h.http.call_count("api.deezer.com/search/artist"), 1);
    }
}
