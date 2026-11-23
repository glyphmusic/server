//! Server-side music taste & intent engine (default-on, with strict client-side
//! local-only opt-out):
//! - `POST /v1/taste/tags`: shared 30-day cache of canonical genres, moods, eras,
//!   and related artists (Last.fm `artist.getTopTags`/`artist.getSimilar` + Deezer
//!   genres/related + built-in cold-start fallback).
//! - `POST /v1/taste/collab`: hub-demoted cosine/PMI item-item collaborative
//!   filtering with session-transition edges, negative contrasting, multi-seed
//!   consensus, and genre-bridged related-artist blending.
//! - `POST /v1/taste/recommend`: unified server-first recommendation & Daily Mix
//!   engine for Home, Radio/Autoplay, and Discovery. Infers real-time user
//!   intention (Discovery, Comfort, DeepFocus, ArtistDive, VibeReset with
//!   skip-streak pivot, HighEnergy, LateNightWindDown), retrieves multi-channel
//!   cached candidates, ranks with multi-objective scoring + MMR diversity
//!   re-ranking, and builds deterministic Daily Mixes.
//! - `GET /v1/taste/onboarding`: genre-diverse starter artists and songs.

use crate::artist::ArtistResponse;
use crate::cache::{self, DAY, HOUR};
use crate::error::ApiErr;
use crate::merge::{normalize_artist, normalize_title, plain_words};
use crate::providers::{build_url, deezer, fetch_json, lastfm};
use crate::traits::{with_timeout, HttpRequest, RateDecision, Services};
use glyph_protocol::taste::{
    builtin_artist_genres, classify_tags_full, genre_default_moods, genre_similarity,
    mood_energy_valence, mood_similarity, ArtistTags, CollabArtistRec, DailyMix, InferredIntent,
    ListeningMode, OnboardingArtist, OnboardingCatalogResponse, RecommendedTrack,
    SessionEventContext, SessionEventOutcome, TasteCollabRequest, TasteCollabResponse,
    TastePurpose, TasteRecommendRequest, TasteRecommendResponse, TasteTagsRequest,
    TasteTagsResponse,
};
use glyph_protocol::{SimilarResponse, Track};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

const TAG_FOUND_TTL_S: u64 = 30 * DAY;
const TAG_MISS_TTL_S: u64 = 12 * HOUR;
const TOP_TRACKS_TTL_S: u64 = 7 * DAY;
const TOP_TRACKS_MISS_TTL_S: u64 = 2 * HOUR;
const COOCCUR_TTL_S: u64 = 90 * DAY;
const ONBOARDING_TTL_S: u64 = 6 * HOUR;
const CONTRIB_COOLDOWN_S: u64 = HOUR;

const MAX_TAG_BATCH: usize = 30;
const MAX_POSITIVE_SIGNALS: usize = 40;
const MAX_NEGATIVE_SIGNALS: usize = 40;
const MAX_AVOIDED_TAGS: usize = 30;
const MAX_CONTRIB_ARTISTS: usize = 10;
const MAX_SESSION_PAIRS: usize = 16;
const MAX_SUPPORTERS: usize = 250;
const MAX_NEIGHBORS: usize = 48;
const DEFAULT_COLLAB_LIMIT: usize = 15;
const MAX_COLLAB_LIMIT: usize = 30;
const DEFAULT_REC_HOME_LIMIT: usize = 20;
const DEFAULT_REC_RADIO_LIMIT: usize = 40;
const MAX_REC_LIMIT: usize = 60;

fn canonical_artist_key(name: &str) -> String {
    let n = normalize_artist(name);
    if !n.is_empty() {
        n
    } else {
        plain_words(name)
    }
}

fn canonical_song_key(title: &str, artist: &str) -> String {
    format!("{}|{}", normalize_title(title), canonical_artist_key(artist))
}

fn primary_artist_display(credit: &str) -> String {
    let mut best = credit.trim();
    for sep in [" feat. ", " feat ", " ft. ", " ft ", " featuring ", " x ", " × ", ";"] {
        if let Some(i) = best.to_lowercase().find(sep) {
            best = best[..i].trim();
        }
    }
    if let Some(i) = best.find(',') {
        let rest = best[i..].to_lowercase();
        if !rest.starts_with(", the creator") {
            best = best[..i].trim();
        }
    }
    if best.is_empty() {
        credit.trim().to_string()
    } else {
        best.to_string()
    }
}

fn voter_hash(voter: &str) -> String {
    Sha256::digest(voter.as_bytes())
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn daily_jitter(date_key: &str, salt: &str, key: &str) -> f32 {
    let h = fnv1a(&format!("{date_key}|{salt}|{key}"));
    ((h % 1000) as f32 / 1000.0) * 0.28
}

fn isrc_country(isrc: &str) -> Option<String> {
    let c: String = isrc.trim().chars().take(2).collect();
    (c.len() == 2 && c.chars().all(|c| c.is_ascii_alphabetic())).then(|| c.to_ascii_uppercase())
}

/// Detects canonical version/style tokens from a track title and optional album
/// using strict word boundaries.
fn detect_versions(title: &str, album: Option<&str>) -> Vec<&'static str> {
    let norm = |s: &str| -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push(' ');
        for c in s.to_lowercase().chars() {
            if c.is_alphanumeric() {
                out.push(c);
            } else {
                out.push(' ');
            }
        }
        out.push(' ');
        out
    };
    let t = norm(title);
    let a = album.map(norm).unwrap_or_default();
    let has = |hay: &str, phrase: &str| hay.contains(&format!(" {phrase} "));
    let mut out: Vec<&'static str> = Vec::new();
    let mut push = |style: &'static str| {
        if !out.contains(&style) {
            out.push(style);
        }
    };
    if has(&t, "karaoke") || has(&t, "backing track") { push("karaoke"); }
    if has(&t, "instrumental") { push("instrumental"); }
    if has(&t, "8d") || has(&t, "8d audio") { push("8d"); }
    if has(&t, "nightcore") { push("nightcore"); }
    if has(&t, "sped up") || has(&t, "speed up") || has(&t, "spedups") { push("sped_up"); }
    if has(&t, "slowed") || has(&t, "slowed reverb") || has(&t, "slowed down") { push("slowed"); }
    if has(&t, "cover") || has(&t, "tribute") || has(&t, "remake") { push("cover"); }
    if has(&t, "remix") || has(&t, "club mix") || has(&t, "extended mix") || has(&t, "vip mix") { push("remix"); }
    if has(&t, "acoustic") || has(&t, "unplugged") { push("acoustic"); }
    if has(&t, "live") || has(&t, "live at") || has(&t, "live from") || has(&a, "live at") || has(&a, "live from") || has(&a, "unplugged") {
        push("live");
    }
    if has(&t, "lofi") || has(&t, "lo fi") { push("lofi"); }
    if has(&t, "demo") || has(&t, "demo version") { push("demo"); }
    out
}

const DEFAULT_ODD_VERSIONS: &[&str] = &[
    "karaoke",
    "instrumental",
    "8d",
    "nightcore",
    "sped_up",
    "slowed",
    "cover",
];

fn hour_bucket_index(hour: u8) -> usize {
    match hour % 24 {
        5..=10 => 0,  // Morning
        11..=16 => 1, // Afternoon
        17..=21 => 2, // Evening
        _ => 3,       // Night
    }
}

fn bucket_aligned_moods(hour: u8, is_weekend: bool) -> &'static [&'static str] {
    match (hour_bucket_index(hour), is_weekend) {
        (0, false) => &["Warm", "Focus", "Upbeat", "Chill"],
        (0, true) => &["Warm", "Chill", "Upbeat", "Nostalgic"],
        (1, false) => &["Focus", "Energetic", "Groovy", "Upbeat"],
        (1, true) => &["Upbeat", "Groovy", "Energetic", "Warm"],
        (2, false) => &["Chill", "Warm", "Nostalgic", "Dreamy"],
        (2, true) => &["Groovy", "Energetic", "Upbeat", "Late Night"],
        (3, false) => &["Late Night", "Atmospheric", "Dreamy", "Chill"],
        (3, true) => &["Late Night", "Groovy", "Dark", "Atmospheric"],
        _ => &["Chill", "Warm"],
    }
}

/// Maps Deezer's well-known numeric `genre_id` values to canonical tag strings
/// without extra API round-trips.
fn deezer_genre_name(id: u64) -> Option<&'static str> {
    Some(match id {
        2 => "afrobeats",
        16 => "j-pop",
        52 => "pop",
        84 => "country",
        85 => "alternative",
        98 => "classical",
        106 => "electronic",
        113 => "dance",
        116 => "hip-hop",
        129 => "jazz",
        132 => "pop",
        144 => "reggae",
        152 => "rock",
        153 => "blues",
        165 => "r&b",
        169 => "soul",
        173 => "soundtrack",
        197 => "latin",
        464 => "metal",
        466 => "folk",
        _ => return None,
    })
}

// ------------------------------------------------ Last.fm & Deezer tag fetch

/// Last.fm's XML-to-JSON bridge serializes 1-element lists as a single JSON
/// object and 0-element lists as a whitespace string or empty array.
fn de_one_or_many<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|x| serde_json::from_value(x).ok())
            .collect(),
        serde_json::Value::Object(_) => serde_json::from_value(v).ok().into_iter().collect(),
        _ => Vec::new(),
    })
}

#[derive(Deserialize)]
struct LfmTopTagsResp {
    toptags: Option<LfmTopTags>,
}
#[derive(Deserialize)]
struct LfmTopTags {
    #[serde(default, deserialize_with = "de_one_or_many")]
    tag: Vec<LfmTag>,
}
#[derive(Deserialize)]
struct LfmTag {
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "crate::providers::de_u64_lenient")]
    count: Option<u64>,
}

#[derive(Deserialize)]
struct LfmSimArtistsResp {
    similarartists: Option<LfmSimArtists>,
}
#[derive(Deserialize)]
struct LfmSimArtists {
    #[serde(default, deserialize_with = "de_one_or_many")]
    artist: Vec<LfmSimArtist>,
}
#[derive(Deserialize)]
struct LfmSimArtist {
    #[serde(default)]
    name: String,
}

async fn fetch_lastfm_tags(svc: &Services, artist: &str) -> (Vec<String>, Vec<String>) {
    let Some(key) = svc.config.secrets.lastfm_api_key.as_deref() else {
        return (Vec::new(), Vec::new());
    };
    let tags_url = build_url(
        "https://ws.audioscrobbler.com/2.0/",
        &[
            ("method", "artist.gettoptags"),
            ("artist", artist),
            ("autocorrect", "1"),
            ("api_key", key),
            ("format", "json"),
        ],
    );
    let sim_url = build_url(
        "https://ws.audioscrobbler.com/2.0/",
        &[
            ("method", "artist.getsimilar"),
            ("artist", artist),
            ("autocorrect", "1"),
            ("limit", "12"),
            ("api_key", key),
            ("format", "json"),
        ],
    );
    let timeout = svc.config.provider_timeout_ms.min(3_500);
    let (tags_res, sim_res) = futures::join!(
        with_timeout(&*svc.rt, timeout, fetch_json::<LfmTopTagsResp>(svc, HttpRequest::get(tags_url))),
        with_timeout(&*svc.rt, timeout, fetch_json::<LfmSimArtistsResp>(svc, HttpRequest::get(sim_url))),
    );
    let tags: Vec<String> = tags_res
        .and_then(Result::ok)
        .and_then(|r| r.toptags)
        .map(|t| {
            t.tag
                .into_iter()
                .filter(|x| x.count.unwrap_or(10) >= 5 && !x.name.trim().is_empty())
                .take(18)
                .map(|x| x.name)
                .collect()
        })
        .unwrap_or_default();
    let similar: Vec<String> = sim_res
        .and_then(Result::ok)
        .and_then(|r| r.similarartists)
        .map(|s| {
            s.artist
                .into_iter()
                .map(|a| a.name.trim().to_string())
                .filter(|n| !n.is_empty())
                .take(12)
                .collect()
        })
        .unwrap_or_default();
    (tags, similar)
}

#[derive(Deserialize)]
struct DzSearchArtistResp {
    #[serde(default)]
    data: Vec<DzArtistEntry>,
}
#[derive(Deserialize)]
struct DzArtistEntry {
    #[serde(default, deserialize_with = "crate::providers::de_u64_lenient")]
    id: Option<u64>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    nb_fan: Option<u64>,
}
#[derive(Deserialize)]
struct DzAlbumsResp {
    #[serde(default)]
    data: Vec<DzAlbumGenre>,
}
#[derive(Deserialize)]
struct DzAlbumGenre {
    #[serde(default, deserialize_with = "crate::providers::de_u64_lenient")]
    genre_id: Option<u64>,
}
#[derive(Deserialize)]
struct DzRelatedResp {
    #[serde(default)]
    data: Vec<DzRelatedArtist>,
}
#[derive(Deserialize)]
struct DzRelatedArtist {
    #[serde(default)]
    name: String,
}
#[derive(Deserialize)]
struct DzTopTracksResp {
    #[serde(default)]
    data: Vec<deezer::DzTrack>,
}

async fn fetch_deezer_tags(svc: &Services, artist: &str) -> (Vec<String>, Vec<String>) {
    let timeout = svc.config.provider_timeout_ms.min(3_000);
    let search_url = build_url("https://api.deezer.com/search/artist", &[("q", artist), ("limit", "6")]);
    let Some(Ok(found)) = with_timeout(
        &*svc.rt,
        timeout,
        fetch_json::<DzSearchArtistResp>(svc, HttpRequest::get(search_url)),
    )
    .await
    else {
        return (Vec::new(), Vec::new());
    };
    let want = plain_words(artist);
    let best = found
        .data
        .into_iter()
        .filter(|a| plain_words(&a.name) == want)
        .max_by_key(|a| a.nb_fan.unwrap_or(0));
    let Some(id) = best.and_then(|a| a.id) else {
        return (Vec::new(), Vec::new());
    };
    let alb_url = build_url(&format!("https://api.deezer.com/artist/{id}/albums"), &[("limit", "10")]);
    let rel_url = build_url(&format!("https://api.deezer.com/artist/{id}/related"), &[("limit", "12")]);
    let (alb_res, rel_res) = futures::join!(
        with_timeout(&*svc.rt, timeout, fetch_json::<DzAlbumsResp>(svc, HttpRequest::get(alb_url))),
        with_timeout(&*svc.rt, timeout, fetch_json::<DzRelatedResp>(svc, HttpRequest::get(rel_url))),
    );
    let mut tags = Vec::new();
    if let Some(Ok(albums)) = alb_res {
        for a in albums.data {
            if let Some(name) = a.genre_id.and_then(deezer_genre_name) {
                if !tags.iter().any(|x| x == name) {
                    tags.push(name.to_string());
                }
            }
        }
    }
    let similar = rel_res
        .and_then(Result::ok)
        .map(|r| {
            r.data
                .into_iter()
                .map(|a| a.name.trim().to_string())
                .filter(|n| !n.is_empty())
                .take(12)
                .collect()
        })
        .unwrap_or_default();
    (tags, similar)
}

fn fill_builtin_fallback(norm: &str, item: &mut ArtistTags) {
    if item.genres.is_empty() {
        if let Some(gs) = builtin_artist_genres(norm) {
            let raw: Vec<String> = gs.iter().map(|s| (*s).to_string()).collect();
            let (fg, fm, _, _) = classify_tags_full(&raw, &item.artist);
            item.genres = fg;
            if item.moods.is_empty() {
                item.moods = fm;
            }
        }
    }
}

/// Fetches (or returns cached) canonical genres, moods, eras, tags, and similar
/// artists for a single artist name.
pub async fn lookup_artist_tags(svc: &Services, artist: &str) -> ArtistTags {
    let trimmed = artist.trim();
    let norm = canonical_artist_key(trimmed);
    if norm.is_empty() {
        return ArtistTags { artist: trimmed.to_string(), ..Default::default() };
    }
    let key = format!("tastetag:v1:{norm}");
    if let Some(mut hit) = cache::get_json::<ArtistTags>(&*svc.cache, &key).await {
        hit.artist = trimmed.to_string();
        fill_builtin_fallback(&norm, &mut hit);
        return hit;
    }

    let ((lfm_tags, lfm_sim), (dz_tags, dz_sim)) = futures::join!(
        fetch_lastfm_tags(svc, trimmed),
        fetch_deezer_tags(svc, trimmed),
    );
    let mut raw_tags = lfm_tags;
    raw_tags.extend(dz_tags);
    let (genres, moods, eras, tags) = classify_tags_full(&raw_tags, trimmed);

    let mut similar = Vec::new();
    let mut seen_sim: HashSet<String> = HashSet::new();
    seen_sim.insert(norm.clone());
    for name in lfm_sim.into_iter().chain(dz_sim) {
        let sk = canonical_artist_key(&name);
        if !sk.is_empty() && seen_sim.insert(sk) {
            similar.push(name);
            if similar.len() >= 15 {
                break;
            }
        }
    }

    let mut item = ArtistTags {
        artist: trimmed.to_string(),
        genres,
        moods,
        eras,
        tags,
        similar,
    };
    fill_builtin_fallback(&norm, &mut item);
    let ttl = if item.genres.is_empty() && item.tags.is_empty() && item.similar.is_empty() {
        TAG_MISS_TTL_S
    } else {
        TAG_FOUND_TTL_S
    };
    cache::put_json(&*svc.cache, &key, &item, ttl).await;
    item
}

/// Reads cached artist tags (or built-in fallback) without triggering upstream network calls.
async fn cached_or_builtin_tags(svc: &Services, artist: &str) -> Option<ArtistTags> {
    let norm = canonical_artist_key(artist);
    if norm.is_empty() {
        return None;
    }
    let key = format!("tastetag:v1:{norm}");
    if let Some(mut hit) = cache::get_json::<ArtistTags>(&*svc.cache, &key).await {
        fill_builtin_fallback(&norm, &mut hit);
        return Some(hit);
    }
    let gs = builtin_artist_genres(&norm)?;
    let raw: Vec<String> = gs.iter().map(|s| (*s).to_string()).collect();
    let (genres, moods, eras, tags) = classify_tags_full(&raw, artist);
    Some(ArtistTags {
        artist: artist.to_string(),
        genres,
        moods,
        eras,
        tags,
        similar: Vec::new(),
    })
}

/// `POST /v1/taste/tags`: batch genre/mood/era/similar tag lookup.
pub async fn tags_batch(svc: &Services, req: TasteTagsRequest) -> Result<TasteTagsResponse, ApiErr> {
    let mut unique: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in req.artists {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > 120 {
            continue;
        }
        let k = canonical_artist_key(trimmed);
        if !k.is_empty() && seen.insert(k) {
            unique.push(trimmed.to_string());
            if unique.len() >= MAX_TAG_BATCH {
                break;
            }
        }
    }
    let items = futures::future::join_all(unique.iter().map(|a| lookup_artist_tags(svc, a))).await;
    Ok(TasteTagsResponse { items })
}

// ------------------------------------------------ cached artist top & similar helpers

/// Fetches popular cover-rich tracks for an artist from cache (`artist:v1:*` or
/// `tastetop:v1:*`) or Deezer top tracks (cached 7 days).
async fn fetch_artist_top_tracks(svc: &Services, artist: &str, limit: usize) -> Vec<Track> {
    let trimmed = artist.trim();
    let norm = canonical_artist_key(trimmed);
    if norm.is_empty() {
        return Vec::new();
    }
    // Reuse existing cached artist pages (14-day cache) before making any external call.
    let page_key = format!("artist:v1:en:{}", plain_words(trimmed));
    if let Some(page) = cache::get_json::<ArtistResponse>(&*svc.cache, &page_key).await {
        if !page.top.is_empty() {
            return page.top.into_iter().take(limit).collect();
        }
    }
    let top_key = format!("tastetop:v1:{norm}");
    if let Some(cached) = cache::get_json::<SimilarResponse>(&*svc.cache, &top_key).await {
        return cached.tracks.into_iter().take(limit).collect();
    }
    let timeout = svc.config.provider_timeout_ms.min(2_800);
    let search_url = build_url("https://api.deezer.com/search/artist", &[("q", trimmed), ("limit", "5")]);
    let Some(Ok(found)) = with_timeout(
        &*svc.rt,
        timeout,
        fetch_json::<DzSearchArtistResp>(svc, HttpRequest::get(search_url)),
    )
    .await
    else {
        return Vec::new();
    };
    let want = plain_words(trimmed);
    let best = found
        .data
        .into_iter()
        .filter(|a| plain_words(&a.name) == want)
        .max_by_key(|a| a.nb_fan.unwrap_or(0));
    let Some(id) = best.and_then(|a| a.id) else {
        cache::put_json(&*svc.cache, &top_key, &SimilarResponse { tracks: Vec::new() }, TOP_TRACKS_MISS_TTL_S).await;
        return Vec::new();
    };
    let top_url = build_url(&format!("https://api.deezer.com/artist/{id}/top"), &[("limit", "12")]);
    let tracks: Vec<Track> = with_timeout(
        &*svc.rt,
        timeout,
        fetch_json::<DzTopTracksResp>(svc, HttpRequest::get(top_url)),
    )
    .await
    .and_then(Result::ok)
    .map(|r| r.data.into_iter().map(|t| deezer::to_candidate(t).track).collect())
    .unwrap_or_default();
    let ttl = if tracks.is_empty() { TOP_TRACKS_MISS_TTL_S } else { TOP_TRACKS_TTL_S };
    cache::put_json(&*svc.cache, &top_key, &SimilarResponse { tracks: tracks.clone() }, ttl).await;
    tracks.into_iter().take(limit).collect()
}

/// Fetches similar tracks using the same `similar:{artist}:{title}` cache as `/v1/similar`.
async fn fetch_similar_tracks_cached(svc: &Services, artist: &str, title: &str) -> Vec<Track> {
    let key = format!("similar:{}:{}", plain_words(artist), plain_words(title));
    if let Some(hit) = cache::get_json::<SimilarResponse>(&*svc.cache, &key).await {
        return hit.tracks;
    }
    let timeout = svc.config.provider_timeout_ms.min(3_200);
    let tracks = match with_timeout(&*svc.rt, timeout, lastfm::similar(svc, artist, title)).await {
        Some(Ok(t)) => {
            cache::put_json(&*svc.cache, &key, &SimilarResponse { tracks: t.clone() }, DAY).await;
            t
        }
        _ => Vec::new(),
    };
    tracks
}

/// Fetches cached Deezer chart tracks (reusing `/v1/chart` 1-hour cache).
async fn fetch_chart_cached(svc: &Services) -> Vec<Track> {
    const KEY: &str = "chart:v1:deezer";
    if let Some(hit) = cache::get_json::<SimilarResponse>(&*svc.cache, KEY).await {
        return hit.tracks;
    }
    let timeout = svc.config.provider_timeout_ms.min(3_000);
    match with_timeout(&*svc.rt, timeout, deezer::chart(svc)).await {
        Some(Ok(cands)) => {
            let tracks: Vec<Track> = cands.into_iter().map(|c| c.track).collect();
            cache::put_json(&*svc.cache, KEY, &SimilarResponse { tracks: tracks.clone() }, HOUR).await;
            tracks
        }
        _ => Vec::new(),
    }
}

// ------------------------------------------------ collaborative filtering

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NeighborEntry {
    display: String,
    weight: f32,
    co_voters: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    voters: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CoOccurNode {
    display: String,
    /// Distinct hashed voters who contributed to this artist (anti-sybil dedupe).
    #[serde(default)]
    supporters: Vec<String>,
    /// Total distinct voters observed (may exceed `supporters.len()` once capped).
    #[serde(default)]
    total_voters: u32,
    /// Co-occurring normalized artist key -> accumulated co-occurrence weight.
    #[serde(default)]
    neighbors: BTreeMap<String, NeighborEntry>,
    #[serde(default)]
    updated_at_ms: u64,
}

impl CoOccurNode {
    fn trim_neighbors(&mut self) {
        if self.neighbors.len() <= MAX_NEIGHBORS {
            return;
        }
        let mut entries: Vec<(String, NeighborEntry)> =
            std::mem::take(&mut self.neighbors).into_iter().collect();
        entries.sort_by(|a, b| {
            b.1.weight
                .total_cmp(&a.1.weight)
                .then_with(|| b.1.co_voters.cmp(&a.1.co_voters))
                .then_with(|| a.0.cmp(&b.0))
        });
        entries.truncate(MAX_NEIGHBORS);
        self.neighbors = entries.into_iter().collect();
    }
}

fn cooccur_key(norm_artist: &str) -> String {
    format!("cf:v1:artist:{norm_artist}")
}

/// `POST /v1/taste/collab`: hub-demoted item-item collaborative filtering.
///
/// Optionally ingests the caller's top anonymized artist pairs AND sequential
/// session transition pairs into the shared co-occurrence graph (deduplicated
/// per hashed voter + rate limited), then scores candidates using:
/// - Hub-demoted Cosine/PMI lift (`weight / (sqrt(V_seed) * V_cand^0.55)`)
/// - Multi-seed consensus boost (candidates bridged by multiple seeds rank higher)
/// - Negative contrasting (penalizes candidates that strongly co-occur with hated artists)
/// - Soft genre/mood similarity & avoided-tag suppression
/// - Genre-bridged related-artist cold-start blending
pub async fn collab(
    svc: &Services,
    voter: Option<&str>,
    req: TasteCollabRequest,
) -> Result<TasteCollabResponse, ApiErr> {
    // Sanitize & deduplicate positive signals (blending long-term + recent phase).
    let mut pos_map: HashMap<String, (String, f32)> = HashMap::new();
    for sig in req.positive.into_iter().take(MAX_POSITIVE_SIGNALS) {
        let name = sig.artist.trim();
        if name.is_empty() || name.len() > 120 || !sig.weight.is_finite() {
            continue;
        }
        let base_w = sig.weight.clamp(0.0, 1.0);
        let rec_w = sig
            .recent_weight
            .filter(|r| r.is_finite())
            .map(|r| r.clamp(0.0, 1.0))
            .unwrap_or(0.0);
        let w = (base_w * 0.78 + rec_w * 0.30).clamp(0.0, 1.0);
        if w <= 0.01 {
            continue;
        }
        let norm = canonical_artist_key(name);
        if norm.is_empty() {
            continue;
        }
        pos_map
            .entry(norm)
            .and_modify(|e| {
                if w > e.1 {
                    e.1 = w;
                    e.0 = name.to_string();
                }
            })
            .or_insert_with(|| (name.to_string(), w));
    }

    if pos_map.is_empty() {
        return Ok(TasteCollabResponse::default());
    }

    let mut positive: Vec<(String, String, f32)> = pos_map
        .into_iter()
        .map(|(norm, (display, w))| (norm, display, w))
        .collect();
    positive.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

    let mut excluded: HashSet<String> = positive.iter().map(|(k, _, _)| k.clone()).collect();
    let mut negative_keys: Vec<String> = Vec::new();
    for neg in req.negative.into_iter().take(MAX_NEGATIVE_SIGNALS) {
        let k = canonical_artist_key(&neg);
        if !k.is_empty() {
            if excluded.insert(k.clone()) {
                negative_keys.push(k);
            }
        }
    }

    let avoided_tags: HashSet<String> = req
        .avoided_tags
        .into_iter()
        .take(MAX_AVOIDED_TAGS)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let genre_pref: HashMap<String, f32> = req
        .genre_scores
        .into_iter()
        .filter(|s| s.score.is_finite())
        .map(|s| (s.tag, s.score.clamp(-1.0, 1.0)))
        .collect();
    let mood_pref: HashMap<String, f32> = req
        .mood_scores
        .into_iter()
        .filter(|s| s.score.is_finite())
        .map(|s| (s.tag, s.score.clamp(-1.0, 1.0)))
        .collect();

    // Sanitize anonymized session transition pairs.
    let mut session_edge_pairs: HashMap<(String, String), (String, String)> = HashMap::new();
    for (a_raw, b_raw) in req.session_pairs.into_iter().take(MAX_SESSION_PAIRS) {
        let ka = canonical_artist_key(&a_raw);
        let kb = canonical_artist_key(&b_raw);
        if !ka.is_empty() && !kb.is_empty() && ka != kb {
            session_edge_pairs.entry((ka.clone(), kb.clone())).or_insert((a_raw.trim().to_string(), b_raw.trim().to_string()));
            session_edge_pairs.entry((kb, ka)).or_insert((b_raw.trim().to_string(), a_raw.trim().to_string()));
        }
    }

    let limit = req.limit.unwrap_or(DEFAULT_COLLAB_LIMIT).clamp(1, MAX_COLLAB_LIMIT);

    // 1. Optional contribution into item-item co-occurrence graph (static + session transitions).
    if req.contribute {
        if let Some(voter_raw) = voter.filter(|v| !v.is_empty()) {
            let strong: Vec<&(String, String, f32)> = positive
                .iter()
                .filter(|(_, _, w)| *w >= 0.30)
                .take(MAX_CONTRIB_ARTISTS)
                .collect();
            if strong.len() >= 2 {
                let vh = voter_hash(voter_raw);
                let allowed = svc
                    .limiter
                    .check_often(&format!("cf-contrib:{vh}"), 1, CONTRIB_COOLDOWN_S)
                    .await
                    == RateDecision::Allowed;
                if allowed {
                    let now_ms = svc.rt.now_ms();
                    let nodes = futures::future::join_all(strong.iter().map(|(k, _, _)| {
                        let key = cooccur_key(k);
                        async move { cache::get_json::<CoOccurNode>(&*svc.cache, &key).await }
                    }))
                    .await;
                    let mut writes = Vec::new();
                    for (idx, (k_a, disp_a, w_a)) in strong.iter().enumerate() {
                        let mut node = nodes[idx].clone().unwrap_or_else(|| CoOccurNode {
                            display: disp_a.clone(),
                            ..Default::default()
                        });
                        if node.display.is_empty() {
                            node.display = disp_a.clone();
                        }
                        let mut node_changed = false;
                        if !node.supporters.iter().any(|s| s == &vh) {
                            if node.supporters.len() >= MAX_SUPPORTERS {
                                node.supporters.remove(0);
                            }
                            node.supporters.push(vh.clone());
                            node.total_voters = node.total_voters.saturating_add(1);
                            node_changed = true;
                        }
                        for (k_b, disp_b, w_b) in &strong {
                            if k_a == k_b {
                                continue;
                            }
                            // Sequential same-session pairs represent tighter vibe coherence (1.35x).
                            let session_mult = if session_edge_pairs.contains_key(&(k_a.clone(), k_b.clone())) {
                                1.35
                            } else {
                                1.0
                            };
                            let pair_w = (w_a.min(*w_b) * session_mult).min(1.4);
                            let entry = node.neighbors.entry(k_b.clone()).or_insert_with(|| NeighborEntry {
                                display: disp_b.clone(),
                                weight: 0.0,
                                co_voters: 0,
                                voters: Vec::new(),
                            });
                            if entry.display.is_empty() {
                                entry.display = disp_b.clone();
                            }
                            if entry.voters.iter().any(|s| s == &vh) {
                                continue;
                            }
                            if entry.voters.len() >= MAX_SUPPORTERS {
                                entry.voters.remove(0);
                            }
                            entry.voters.push(vh.clone());
                            entry.weight += pair_w;
                            entry.co_voters = entry.co_voters.saturating_add(1);
                            node_changed = true;
                        }
                        if node_changed {
                            node.updated_at_ms = now_ms;
                            node.trim_neighbors();
                            writes.push((cooccur_key(k_a), node));
                        }
                    }
                    futures::future::join_all(writes.into_iter().map(|(k, n)| async move {
                        cache::put_json(&*svc.cache, &k, &n, COOCCUR_TTL_S).await
                    }))
                    .await;
                }
            }
        }
    }

    // 2. Query item-item co-occurrence nodes for top positive seeds & top negative contrast seeds.
    let query_seeds: Vec<&(String, String, f32)> = positive.iter().take(15).collect();
    let neg_contrast_seeds: Vec<&String> = negative_keys.iter().take(4).collect();
    let (seed_nodes, neg_nodes) = futures::join!(
        futures::future::join_all(query_seeds.iter().map(|(k, _, _)| {
            let key = cooccur_key(k);
            async move { cache::get_json::<CoOccurNode>(&*svc.cache, &key).await }
        })),
        futures::future::join_all(neg_contrast_seeds.iter().map(|k| {
            let key = cooccur_key(k);
            async move { cache::get_json::<CoOccurNode>(&*svc.cache, &key).await }
        })),
    );

    // Collect negative-contrast neighborhoods (candidates strongly associated with disliked artists).
    let mut neg_assoc: HashMap<String, f32> = HashMap::new();
    for node in neg_nodes.into_iter().flatten() {
        let deg = (node.total_voters.max(1) as f32).sqrt();
        for (cand_k, edge) in &node.neighbors {
            if edge.weight > 0.0 {
                *neg_assoc.entry(cand_k.clone()).or_default() += (edge.weight / deg).min(1.5);
            }
        }
    }

    struct RawEdge {
        display: String,
        raw_score: f32,
        matched_seeds: u32,
        because: Vec<(String, f32)>,
        collaborative: bool,
    }

    let mut cands: HashMap<String, RawEdge> = HashMap::new();
    let mut voter_set: HashSet<String> = HashSet::new();
    let mut max_total_voters: u32 = 0;
    let mut collab_hits = 0usize;

    for (idx, (_, seed_display, seed_w)) in query_seeds.iter().enumerate() {
        let Some(node) = &seed_nodes[idx] else { continue };
        max_total_voters = max_total_voters.max(node.total_voters);
        for v in &node.supporters {
            voter_set.insert(v.clone());
        }
        let deg_norm = (node.total_voters.max(1) as f32).sqrt();
        for (cand_k, edge) in &node.neighbors {
            if excluded.contains(cand_k) || edge.weight <= 0.0 {
                continue;
            }
            let co_bonus = (1.0 + edge.co_voters as f32).ln();
            let contrib = seed_w * (edge.weight / deg_norm) * (0.60 + 0.40 * co_bonus);
            collab_hits += 1;
            let acc = cands.entry(cand_k.clone()).or_insert_with(|| RawEdge {
                display: edge.display.clone(),
                raw_score: 0.0,
                matched_seeds: 0,
                because: Vec::new(),
                collaborative: true,
            });
            acc.raw_score += contrib;
            acc.matched_seeds += 1;
            acc.collaborative = true;
            acc.because.push((seed_display.clone(), contrib));
        }
    }

    // 2b. Hub-demoted Cosine/PMI popularity normalization on top CF candidates:
    // prevents mega-popular mainstream artists from swamping genuine niche matches.
    let mut top_cf_keys: Vec<(String, f32)> = cands
        .iter()
        .map(|(k, v)| (k.clone(), v.raw_score))
        .collect();
    top_cf_keys.sort_by(|a, b| b.1.total_cmp(&a.1));
    top_cf_keys.truncate(40);
    let cand_nodes = futures::future::join_all(top_cf_keys.iter().map(|(k, _)| {
        let key = cooccur_key(k);
        async move { cache::get_json::<CoOccurNode>(&*svc.cache, &key).await }
    }))
    .await;
    for ((cand_k, _), cand_node) in top_cf_keys.into_iter().zip(cand_nodes) {
        if let Some(acc) = cands.get_mut(&cand_k) {
            let cand_voters = cand_node.as_ref().map_or(1, |n| n.total_voters.max(1));
            // Popularity damping exponent 0.50 (true cosine denominator on candidate degree).
            let pop_damp = (cand_voters as f32).powf(0.50).max(1.0);
            // Multi-seed consensus multiplier: bridging 2..4 of the user's seeds is strong signal!
            let consensus = 1.0 + 0.28 * (acc.matched_seeds.saturating_sub(1).min(4) as f32);
            acc.raw_score = (acc.raw_score / pop_damp) * consensus;
        }
    }

    // 3. Cold-start / sparse-graph & related-artist bridge blending (cached tags first).
    if cands.len() < limit {
        let seed_tags = futures::future::join_all(
            query_seeds.iter().take(6).map(|(_, disp, _)| lookup_artist_tags(svc, disp)),
        )
        .await;
        for (idx, info) in seed_tags.into_iter().enumerate() {
            let (_, seed_display, seed_w) = query_seeds[idx];
            for (rank, sim_name) in info.similar.into_iter().take(10).enumerate() {
                let cand_k = canonical_artist_key(&sim_name);
                if cand_k.is_empty() || excluded.contains(&cand_k) {
                    continue;
                }
                let contrib = seed_w * (0.36 - rank as f32 * 0.025).max(0.08);
                let acc = cands.entry(cand_k).or_insert_with(|| RawEdge {
                    display: sim_name,
                    raw_score: 0.0,
                    matched_seeds: 0,
                    because: Vec::new(),
                    collaborative: false,
                });
                acc.raw_score += contrib;
                acc.matched_seeds += 1;
                acc.because.push((seed_display.clone(), contrib));
            }
        }
    }

    // 4. Rank, enrich top candidates with cached/builtin tags, apply negative contrasting,
    // soft genre/mood affinity lift, and avoided-tag suppression.
    let mut ranked: Vec<(String, RawEdge)> = cands.into_iter().collect();
    ranked.sort_by(|a, b| b.1.raw_score.total_cmp(&a.1.raw_score).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate((limit * 2).min(44));

    let cand_tags = futures::future::join_all(
        ranked.iter().map(|(_, acc)| cached_or_builtin_tags(svc, &acc.display)),
    )
    .await;

    let mut recs: Vec<CollabArtistRec> = Vec::with_capacity(limit);
    for ((cand_k, mut acc), cached_tags) in ranked.into_iter().zip(cand_tags) {
        let mut score = acc.raw_score;
        // Multi-seed consensus boost for metadata-blended candidates as well.
        if !acc.collaborative && acc.matched_seeds > 1 {
            score *= 1.0 + 0.25 * (acc.matched_seeds.saturating_sub(1).min(3) as f32);
        }
        // Negative contrasting: dampen candidates tightly coupled to disliked artists.
        if let Some(&neg_w) = neg_assoc.get(&cand_k) {
            let damp = (1.0 / (1.0 + neg_w * 0.85)).clamp(0.30, 1.0);
            score *= damp;
        }
        let genres = cached_tags.as_ref().map(|t| t.genres.clone()).unwrap_or_default();
        if let Some(ref ct) = cached_tags {
            if !avoided_tags.is_empty() {
                let bad_hits = ct
                    .genres
                    .iter()
                    .chain(&ct.moods)
                    .filter(|t| avoided_tags.contains(&t.to_lowercase()))
                    .count();
                if bad_hits > 0 {
                    score *= 0.25f32.powi(bad_hits as i32);
                }
            }
            // Soft genre/mood bridge alignment boost.
            if !genre_pref.is_empty() || !mood_pref.is_empty() {
                let tag_aff = soft_tag_vector_affinity(&ct.genres, &ct.moods, &genre_pref, &mood_pref);
                score *= (1.0 + tag_aff * 0.45).clamp(0.30, 1.60);
            }
        }
        if score <= 0.02 {
            continue;
        }
        acc.because.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut because_of = Vec::new();
        for (name, _) in acc.because {
            if !because_of.iter().any(|x: &String| x.eq_ignore_ascii_case(&name)) {
                because_of.push(name);
                if because_of.len() >= 3 {
                    break;
                }
            }
        }
        recs.push(CollabArtistRec {
            artist: acc.display,
            score,
            because_of,
            genres,
            collaborative: acc.collaborative,
        });
    }

    recs.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.artist.cmp(&b.artist)));
    recs.truncate(limit);

    let contributors = (voter_set.len() as u32).max(max_total_voters);
    Ok(TasteCollabResponse {
        recommendations: recs,
        contributors,
        model_ready: collab_hits > 0,
    })
}

/// Soft genre + mood affinity in `-1.0..=1.0` using canonical genre-family & mood similarity,
/// normalized over all candidate tags so unrelated tracks cannot score high from one incidental mood.
fn soft_tag_vector_affinity(
    cand_genres: &[String],
    cand_moods: &[String],
    user_genres: &HashMap<String, f32>,
    user_moods: &HashMap<String, f32>,
) -> f32 {
    let mut sum = 0.0f32;
    let mut total_w = 0.0f32;
    if !user_genres.is_empty() {
        for cg in cand_genres {
            total_w += 1.0;
            let mut best_sim = 0.0f32;
            let mut best_score = 0.0f32;
            for (ug, &us) in user_genres {
                let sim = genre_similarity(cg, ug);
                if sim > best_sim {
                    best_sim = sim;
                    best_score = us;
                }
            }
            if best_sim >= 0.55 {
                sum += best_score * best_sim;
            }
        }
    }
    if !user_moods.is_empty() {
        for cm in cand_moods {
            total_w += 0.45;
            let mut best_sim = 0.0f32;
            let mut best_score = 0.0f32;
            for (um, &us) in user_moods {
                let sim = mood_similarity(cm, um);
                if sim > best_sim {
                    best_sim = sim;
                    best_score = us;
                }
            }
            if best_sim >= 0.55 {
                sum += best_score * best_sim * 0.45;
            }
        }
    }
    if total_w > 0.0 {
        (sum / total_w).clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

// ------------------------------------------------ server-first intent & recommendation engine

struct SessionAnalysis {
    intent: InferredIntent,
    session_genres: HashMap<String, f32>,
    session_moods: HashMap<String, f32>,
    repelled_artists: HashSet<String>,
    repelled_genres: HashSet<String>,
    repelled_moods: HashSet<String>,
    dive_artists: HashSet<String>,
}

/// Infers real-time listening intention (`InferredIntent`) and active session
/// attraction/repulsion vectors from recent session events, search intent,
/// long-term taste, and time-of-day.
fn analyze_session_intent(
    req: &TasteRecommendRequest,
    tags_by_artist: &HashMap<String, ArtistTags>,
    user_genres: &HashMap<String, f32>,
    user_moods: &HashMap<String, f32>,
) -> SessionAnalysis {
    let mut sess_genres: HashMap<String, f32> = HashMap::new();
    let mut sess_moods: HashMap<String, f32> = HashMap::new();
    let mut repelled_artists: HashSet<String> = HashSet::new();
    let mut repelled_genres: HashSet<String> = HashSet::new();
    let mut repelled_moods: HashSet<String> = HashSet::new();
    let mut recent_pos_events: Vec<(String, String)> = Vec::new();
    let mut early_skips_recent = 0u32;
    let mut completions_recent = 0u32;
    let mut replays_or_likes = 0u32;
    let mut library_events = 0u32;
    let mut discovery_events = 0u32;
    let mut total_recent = 0u32;

    for (idx, ev) in req.session.recent_events.iter().take(16).enumerate() {
        let decay = 0.5f32.powf(ev.age_mins as f32 / 45.0).clamp(0.15, 1.0);
        let ak = canonical_artist_key(&ev.artist);
        let tk = plain_words(&ev.title);
        let is_lib = ev.context == SessionEventContext::Library;
        let is_disc = matches!(ev.context, SessionEventContext::Radio | SessionEventContext::Search);
        if ev.age_mins <= 90 {
            total_recent += 1;
            if is_lib {
                library_events += 1;
            }
            if is_disc {
                discovery_events += 1;
            }
        }
        let delta = match ev.outcome {
            SessionEventOutcome::Replayed => {
                replays_or_likes += 1;
                completions_recent += 1;
                if !ak.is_empty() { recent_pos_events.push((ak.clone(), tk)); }
                2.4 * decay
            }
            SessionEventOutcome::Liked => {
                replays_or_likes += 1;
                if !ak.is_empty() { recent_pos_events.push((ak.clone(), tk)); }
                2.2 * decay
            }
            SessionEventOutcome::Queued => {
                if !ak.is_empty() { recent_pos_events.push((ak.clone(), tk)); }
                1.7 * decay
            }
            SessionEventOutcome::Completed => {
                completions_recent += 1;
                if !ak.is_empty() { recent_pos_events.push((ak.clone(), tk)); }
                1.4 * decay
            }
            SessionEventOutcome::Played => {
                if !ak.is_empty() { recent_pos_events.push((ak.clone(), tk)); }
                0.85 * decay
            }
            SessionEventOutcome::SkippedMid => {
                if ev.age_mins <= 30 && !ak.is_empty() {
                    repelled_artists.insert(ak.clone());
                }
                if is_lib { -0.25 * decay } else { -0.55 * decay }
            }
            SessionEventOutcome::SkippedEarly => {
                if idx < 4 && ev.age_mins <= 20 {
                    early_skips_recent += 1;
                }
                if ev.age_mins <= 35 && !ak.is_empty() {
                    repelled_artists.insert(ak.clone());
                }
                if is_lib { -0.55 * decay } else { -1.45 * decay }
            }
            SessionEventOutcome::Unliked => -1.1 * decay,
        };

        if let Some(meta) = tags_by_artist.get(&ak) {
            for (i, g) in meta.genres.iter().enumerate() {
                let w = 1.0 / (1.0 + i as f32 * 0.35);
                *sess_genres.entry(g.clone()).or_default() += delta * w;
                if matches!(ev.outcome, SessionEventOutcome::SkippedEarly) && ev.age_mins <= 25 && i < 2 {
                    repelled_genres.insert(g.clone());
                }
            }
            for (i, m) in meta.moods.iter().enumerate() {
                let w = 1.0 / (1.0 + i as f32 * 0.35);
                *sess_moods.entry(m.clone()).or_default() += delta * w;
                if matches!(ev.outcome, SessionEventOutcome::SkippedEarly) && ev.age_mins <= 25 && i < 2 {
                    repelled_moods.insert(m.clone());
                }
            }
        }
    }

    // Explicit recent search query intent directly steers session genres & moods.
    for g in &req.session.query_genres {
        *sess_genres.entry(g.clone()).or_default() += 1.65;
        repelled_genres.remove(g);
    }
    for m in &req.session.query_moods {
        *sess_moods.entry(m.clone()).or_default() += 1.65;
        repelled_moods.remove(m);
    }

    // Remove any genre/mood from repulsion if the user actively enjoyed it more than they skipped it.
    repelled_genres.retain(|g| sess_genres.get(g).copied().unwrap_or(0.0) <= 0.1);
    repelled_moods.retain(|m| sess_moods.get(m).copied().unwrap_or(0.0) <= 0.1);

    // Detect Artist Dive: >= 3 of last 5 positive plays from an artist across >= 2 distinct tracks.
    let last5: Vec<&(String, String)> = recent_pos_events.iter().take(5).collect();
    let mut artist_counts: HashMap<&String, usize> = HashMap::new();
    let mut artist_titles: HashMap<&String, HashSet<&String>> = HashMap::new();
    for (a, tk) in &last5 {
        *artist_counts.entry(a).or_default() += 1;
        artist_titles.entry(a).or_default().insert(tk);
    }
    let mut top_dive: Vec<(&String, usize)> = artist_counts
        .into_iter()
        .filter(|(a, c)| *c >= 3 && artist_titles.get(a).is_some_and(|s| s.len() >= 2))
        .collect();
    top_dive.sort_by(|a, b| b.1.cmp(&a.1));
    let dive_artists: HashSet<String> = if last5.len() >= 3 && !top_dive.is_empty() {
        top_dive.iter().take(2).map(|(a, _)| (*a).clone()).collect()
    } else {
        HashSet::new()
    };

    // Detect Vibe Reset (consecutive rapid skips).
    let vibe_pivot = req.session.recent_skip_streak >= 2 || early_skips_recent >= 2;

    // Compute active session energy from positive moods (or time-of-day prior).
    let time_moods = bucket_aligned_moods(req.session.hour, req.session.is_weekend);
    let mut e_sum = 0.0f32;
    let mut e_w = 0.0f32;
    for (m, &v) in &sess_moods {
        if v > 0.1 {
            let (e, _) = mood_energy_valence(m);
            e_sum += e * v;
            e_w += v;
        }
    }
    let prior_e: f32 = time_moods
        .iter()
        .map(|m| mood_energy_valence(m).0)
        .sum::<f32>()
        / (time_moods.len().max(1) as f32);
    let mut energy = if e_w > 0.0 {
        (e_sum / e_w) * 0.78 + prior_e * 0.22
    } else {
        prior_e
    };

    // If vibe_pivot is triggered, deliberately shift target energy away from the skipped cluster.
    if vibe_pivot && !repelled_moods.is_empty() {
        let rep_e: f32 = repelled_moods
            .iter()
            .map(|m| mood_energy_valence(m).0)
            .sum::<f32>()
            / repelled_moods.len() as f32;
        energy = if rep_e >= 0.55 {
            (energy * 0.4 + 0.30).clamp(0.18, 0.55)
        } else {
            (energy * 0.4 + 0.68).clamp(0.50, 0.85)
        };
    }

    let focus_cues = ["Focus"];
    let focus_genres_set = ["Lo-Fi", "Ambient", "Classical", "Instrumental", "Soundtrack"];
    let has_focus_vibe = sess_moods
        .iter()
        .any(|(m, &v)| v > 0.6 && focus_cues.contains(&m.as_str()))
        || sess_genres
            .iter()
            .any(|(g, &v)| v > 0.6 && focus_genres_set.contains(&g.as_str()));

    let distinct_recent = recent_pos_events.iter().take(6).map(|(a, _)| a).collect::<HashSet<_>>().len();
    let mode = if vibe_pivot {
        ListeningMode::VibeReset
    } else if !dive_artists.is_empty() && early_skips_recent == 0 {
        ListeningMode::ArtistDive
    } else if replays_or_likes >= 2 {
        ListeningMode::Comfort
    } else if has_focus_vibe && completions_recent >= 2 && early_skips_recent == 0 {
        ListeningMode::DeepFocus
    } else if total_recent >= 3 && library_events * 10 >= total_recent * 7 && early_skips_recent == 0 {
        ListeningMode::Comfort
    } else if req.purpose == TastePurpose::Discovery
        || (total_recent >= 2 && discovery_events * 2 >= total_recent)
        || (distinct_recent >= 4 && req.variety_preference >= 0.55)
    {
        ListeningMode::Discovery
    } else if energy >= 0.72 && e_w >= 1.0 {
        ListeningMode::HighEnergy
    } else if (req.session.hour >= 22 || req.session.hour <= 4) && energy <= 0.46 {
        ListeningMode::LateNightWindDown
    } else {
        ListeningMode::Balanced
    };

    // Calibrate exploration vs comfort balance (0 = comfort, 1 = high discovery).
    let base_nov = req.variety_preference.clamp(0.15, 0.85);
    let novelty = match mode {
        ListeningMode::Discovery => (base_nov + 0.25).clamp(0.55, 0.95),
        ListeningMode::Comfort => (base_nov - 0.28).clamp(0.10, 0.45),
        ListeningMode::DeepFocus => (base_nov - 0.15).clamp(0.20, 0.55),
        ListeningMode::ArtistDive => (base_nov - 0.10).clamp(0.25, 0.60),
        ListeningMode::VibeReset => 0.62,
        ListeningMode::HighEnergy | ListeningMode::LateNightWindDown | ListeningMode::Balanced => base_nov,
    };

    // Pick top focus genres & moods (fall back to non-repelled long-term favorites if session is fresh or pivoting).
    let mut focus_g: Vec<(String, f32)> = sess_genres
        .iter()
        .filter(|(g, v)| **v > 0.2 && !repelled_genres.contains(*g))
        .map(|(g, v)| (g.clone(), *v))
        .collect();
    if focus_g.is_empty() || vibe_pivot {
        for (g, &s) in user_genres {
            if s > 0.25 && !repelled_genres.contains(g) && !focus_g.iter().any(|(x, _)| x == g) {
                focus_g.push((g.clone(), s * 0.8));
            }
        }
    }
    focus_g.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let focus_genres: Vec<String> = focus_g.into_iter().take(3).map(|(g, _)| g).collect();

    let mut focus_m: Vec<(String, f32)> = sess_moods
        .iter()
        .filter(|(m, v)| **v > 0.2 && !repelled_moods.contains(*m))
        .map(|(m, v)| (m.clone(), *v))
        .collect();
    if focus_m.is_empty() || vibe_pivot {
        for (m, &s) in user_moods {
            if s > 0.25 && !repelled_moods.contains(m) && !focus_m.iter().any(|(x, _)| x == m) {
                focus_m.push((m.clone(), s * 0.8));
            }
        }
    }
    if focus_m.is_empty() {
        for &m in time_moods.iter().take(2) {
            focus_m.push((m.to_string(), 0.5));
        }
    }
    focus_m.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let focus_moods: Vec<String> = focus_m.into_iter().take(3).map(|(m, _)| m).collect();

    let intent = InferredIntent {
        mode,
        focus_genres,
        focus_moods,
        energy: energy.clamp(0.0, 1.0),
        novelty,
        vibe_pivot,
    };

    SessionAnalysis {
        intent,
        session_genres: sess_genres,
        session_moods: sess_moods,
        repelled_artists,
        repelled_genres,
        repelled_moods,
        dive_artists,
    }
}

struct CandidateTrack {
    track: Track,
    channel_score: f32,
    channels: u8,
    collab_boost: f32,
    because_of: Vec<String>,
    primary_reason: String,
}

fn track_metadata_quality(t: &Track) -> u8 {
    let mut q = 0u8;
    if t.cover_url.as_deref().is_some_and(|c| !c.trim().is_empty()) { q += 3; }
    if t.duration_ms.is_some_and(|d| d > 0) { q += 2; }
    if t.isrc.as_deref().is_some_and(|i| !i.trim().is_empty()) { q += 2; }
    if t.album.as_deref().is_some_and(|a| !a.trim().is_empty()) { q += 1; }
    q
}

fn upsert_candidate(
    pool: &mut HashMap<String, CandidateTrack>,
    track: Track,
    score_add: f32,
    collab_add: f32,
    reason: &str,
    because: Option<&str>,
) {
    let key = canonical_song_key(&track.title, &track.artist);
    if key == "|" {
        return;
    }
    if let Some(existing) = pool.get_mut(&key) {
        // Keep whichever copy has richer metadata (e.g. Deezer track with cover & ISRC beats bare Last.fm stub).
        if track_metadata_quality(&track) > track_metadata_quality(&existing.track) {
            existing.track = track;
        }
        existing.channel_score += score_add;
        existing.channels = existing.channels.saturating_add(1);
        existing.collab_boost = existing.collab_boost.max(collab_add);
        if let Some(b) = because {
            if !existing.because_of.iter().any(|x| x.eq_ignore_ascii_case(b)) && existing.because_of.len() < 3 {
                existing.because_of.push(b.to_string());
            }
        }
        if reason == "collab" || reason == "session_vibe" {
            existing.primary_reason = reason.to_string();
        }
    } else {
        pool.insert(
            key,
            CandidateTrack {
                track,
                channel_score: score_add,
                channels: 1,
                collab_boost: collab_add,
                because_of: because.map(|b| vec![b.to_string()]).unwrap_or_default(),
                primary_reason: reason.to_string(),
            },
        );
    }
}

/// `POST /v1/taste/recommend`: unified server-first music taste & recommendation engine.
pub async fn recommend(
    svc: &Services,
    voter: Option<&str>,
    req: TasteRecommendRequest,
) -> Result<TasteRecommendResponse, ApiErr> {
    let limit = req
        .limit
        .unwrap_or(match req.purpose {
            TastePurpose::Radio => DEFAULT_REC_RADIO_LIMIT,
            TastePurpose::Home | TastePurpose::Discovery => DEFAULT_REC_HOME_LIMIT,
        })
        .clamp(1, MAX_REC_LIMIT);

    // 1. Build signed artist/genre/mood/era maps from request.
    let mut artist_aff: HashMap<String, f32> = HashMap::new();
    let mut artist_display: HashMap<String, String> = HashMap::new();
    for sig in req.positive_artists.iter().take(MAX_POSITIVE_SIGNALS) {
        let k = canonical_artist_key(&sig.artist);
        if k.is_empty() || !sig.weight.is_finite() {
            continue;
        }
        let base = sig.weight.clamp(0.0, 1.0);
        let rec = sig.recent_weight.filter(|r| r.is_finite()).map(|r| r.clamp(0.0, 1.0)).unwrap_or(0.0);
        let combined = (base * 0.72 + rec * 0.28).max(base * 0.85).clamp(0.0, 1.0);
        artist_aff.insert(k.clone(), combined);
        artist_display.insert(k, sig.artist.trim().to_string());
    }
    let mut negative_set: HashSet<String> = HashSet::new();
    for neg in req.negative_artists.iter().take(MAX_NEGATIVE_SIGNALS) {
        let k = canonical_artist_key(neg);
        if !k.is_empty() {
            negative_set.insert(k.clone());
            artist_aff.insert(k, -1.0);
        }
    }
    let user_genres: HashMap<String, f32> = req
        .genre_scores
        .iter()
        .filter(|s| s.score.is_finite())
        .map(|s| (s.tag.clone(), s.score.clamp(-1.0, 1.0)))
        .collect();
    let user_moods: HashMap<String, f32> = req
        .mood_scores
        .iter()
        .filter(|s| s.score.is_finite())
        .map(|s| (s.tag.clone(), s.score.clamp(-1.0, 1.0)))
        .collect();
    let user_eras: HashMap<String, f32> = req
        .era_scores
        .iter()
        .filter(|s| s.score.is_finite())
        .map(|s| (s.tag.clone(), s.score.clamp(-1.0, 1.0)))
        .collect();
    let avoided_tags: HashSet<String> = req
        .avoided_tags
        .iter()
        .take(MAX_AVOIDED_TAGS)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let known_keys: HashSet<String> = req.known_track_keys.iter().cloned().collect();
    let disliked_keys: HashSet<String> = req.disliked_track_keys.iter().cloned().collect();
    let user_region = req.region.as_deref().map(str::to_ascii_uppercase);

    // 2. Bounded artist tag enrichment for top positive + session + seed artists.
    // Cache hits cost 0 external requests; cap live upstream lookups at 4 per request
    // so Workers subrequest budget stays comfortably low.
    let mut tag_targets: Vec<String> = Vec::new();
    let mut seen_targets: HashSet<String> = HashSet::new();
    for ev in req.session.recent_events.iter().take(8) {
        let k = canonical_artist_key(&ev.artist);
        if !k.is_empty() && seen_targets.insert(k) {
            tag_targets.push(primary_artist_display(&ev.artist));
        }
    }
    for s in req.seed_tracks.iter().take(6) {
        let k = canonical_artist_key(&s.artist);
        if !k.is_empty() && seen_targets.insert(k) {
            tag_targets.push(primary_artist_display(&s.artist));
        }
    }
    for a in req.positive_artists.iter().take(12) {
        let k = canonical_artist_key(&a.artist);
        if !k.is_empty() && seen_targets.insert(k) {
            tag_targets.push(a.artist.trim().to_string());
        }
    }
    tag_targets.truncate(16);

    let cached_checks = futures::future::join_all(tag_targets.iter().map(|a| {
        let key = format!("tastetag:v1:{}", canonical_artist_key(a));
        async move { cache::get_json::<ArtistTags>(&*svc.cache, &key).await }
    }))
    .await;

    let mut tags_by_artist: HashMap<String, ArtistTags> = HashMap::new();
    let mut to_fetch_live: Vec<String> = Vec::new();
    for (artist, cached) in tag_targets.into_iter().zip(cached_checks) {
        let k = canonical_artist_key(&artist);
        if let Some(mut hit) = cached {
            hit.artist = artist;
            fill_builtin_fallback(&k, &mut hit);
            tags_by_artist.insert(k, hit);
        } else if to_fetch_live.len() < 4 {
            to_fetch_live.push(artist);
        } else if let Some(builtin) = cached_or_builtin_tags(svc, &artist).await {
            tags_by_artist.insert(k, builtin);
        }
    }

    // Run live tag lookups + collaborative filtering in parallel!
    let collab_req = TasteCollabRequest {
        positive: req.positive_artists.clone(),
        negative: req.negative_artists.clone(),
        avoided_tags: req.avoided_tags.clone(),
        genre_scores: req.genre_scores.clone(),
        mood_scores: req.mood_scores.clone(),
        session_pairs: req.session_pairs.clone(),
        limit: Some(15),
        contribute: req.contribute,
    };
    let (live_tags, collab_res) = futures::join!(
        futures::future::join_all(to_fetch_live.iter().map(|a| lookup_artist_tags(svc, a))),
        collab(svc, voter, collab_req),
    );
    for item in live_tags {
        let k = canonical_artist_key(&item.artist);
        if !k.is_empty() {
            tags_by_artist.insert(k, item);
        }
    }
    let collab_resp = collab_res.unwrap_or_default();

    // 3. Analyze real-time user intention from session + tags + profile.
    let analysis = analyze_session_intent(&req, &tags_by_artist, &user_genres, &user_moods);

    // 4. Multi-channel bounded candidate retrieval (in parallel!).
    // Channel A: Seed similar tracks (prioritizing session / radio seeds; skip repelled seeds on vibe_pivot).
    let mut seed_list: Vec<&glyph_protocol::taste::TasteSeedTrack> = req
        .seed_tracks
        .iter()
        .filter(|s| {
            !s.title.trim().is_empty()
                && !s.artist.trim().is_empty()
                && !(analysis.intent.vibe_pivot
                    && analysis.repelled_artists.contains(&canonical_artist_key(&s.artist)))
        })
        .collect();
    seed_list.sort_by(|a, b| {
        b.is_session
            .cmp(&a.is_session)
            .then_with(|| b.weight.total_cmp(&a.weight))
    });
    let max_sim_seeds = if req.purpose == TastePurpose::Radio { 4 } else { 3 };
    seed_list.truncate(max_sim_seeds);

    // Channel B: Top collaborative & bridge artists -> top cover-rich tracks.
    let mut collab_targets: Vec<(String, f32, Vec<String>)> = collab_resp
        .recommendations
        .iter()
        .filter(|r| !analysis.repelled_artists.contains(&canonical_artist_key(&r.artist)))
        .take(5)
        .map(|r| (r.artist.clone(), r.score.clamp(0.0, 2.5), r.because_of.clone()))
        .collect();

    // For Radio, also pull related artists of the radio seed artists so Radio works
    // seamlessly even when Last.fm API key is missing!
    if req.purpose == TastePurpose::Radio && collab_targets.len() < 4 {
        for s in &seed_list {
            let sk = canonical_artist_key(&s.artist);
            if let Some(meta) = tags_by_artist.get(&sk) {
                for sim in meta.similar.iter().take(3) {
                    let sim_k = canonical_artist_key(sim);
                    if !negative_set.contains(&sim_k)
                        && !collab_targets.iter().any(|(x, _, _)| canonical_artist_key(x) == sim_k)
                    {
                        collab_targets.push((sim.clone(), 0.75, vec![s.artist.clone()]));
                        if collab_targets.len() >= 5 {
                            break;
                        }
                    }
                }
            }
        }
    }

    // Channel C: Anchor / Contrast artists for Home mixes or VibeReset pivot.
    let mut anchor_artists: Vec<String> = Vec::new();
    if req.purpose != TastePurpose::Radio {
        for sig in req.positive_artists.iter().take(10) {
            let k = canonical_artist_key(&sig.artist);
            if k.is_empty() || analysis.repelled_artists.contains(&k) {
                continue;
            }
            if analysis.intent.vibe_pivot {
                // On vibe pivot, deliberately pick an artist whose genres are NOT repelled!
                let is_repelled_genre = tags_by_artist.get(&k).is_some_and(|m| {
                    m.genres.iter().any(|g| analysis.repelled_genres.contains(g))
                });
                if is_repelled_genre {
                    continue;
                }
            }
            if !collab_targets.iter().any(|(x, _, _)| canonical_artist_key(x) == k) {
                anchor_artists.push(sig.artist.clone());
                if anchor_artists.len() >= 3 {
                    break;
                }
            }
        }
    }

    let (sim_results, collab_tracks, anchor_tracks, chart_tracks) = futures::join!(
        futures::future::join_all(
            seed_list
                .iter()
                .map(|s| fetch_similar_tracks_cached(svc, &s.artist, &s.title))
        ),
        futures::future::join_all(
            collab_targets
                .iter()
                .map(|(a, _, _)| fetch_artist_top_tracks(svc, a, 6))
        ),
        futures::future::join_all(
            anchor_artists
                .iter()
                .map(|a| fetch_artist_top_tracks(svc, a, 5))
        ),
        async {
            if req.purpose == TastePurpose::Radio {
                Vec::new()
            } else {
                fetch_chart_cached(svc).await
            }
        },
    );

    let mut pool: HashMap<String, CandidateTrack> = HashMap::new();

    for (idx, list) in sim_results.into_iter().enumerate() {
        let seed = seed_list[idx];
        let reason = if seed.is_session { "session_vibe" } else { "because_of" };
        for (rank, tr) in list.into_iter().take(25).enumerate() {
            let w = (1.05 - rank as f32 * 0.032).max(0.25) * seed.weight.clamp(0.4, 1.3);
            upsert_candidate(&mut pool, tr, w, 0.0, reason, Some(&seed.artist));
        }
    }

    for ((artist, cscore, because), tracks) in collab_targets.into_iter().zip(collab_tracks) {
        let reason_ref = because.first().map(String::as_str).unwrap_or(artist.as_str());
        for (rank, tr) in tracks.into_iter().enumerate() {
            let w = (0.95 - rank as f32 * 0.10).max(0.35);
            upsert_candidate(&mut pool, tr, w, cscore, "collab", Some(reason_ref));
        }
    }

    for (artist, tracks) in anchor_artists.into_iter().zip(anchor_tracks) {
        let reason = if analysis.intent.vibe_pivot { "vibe_reset" } else { "comfort" };
        for (rank, tr) in tracks.into_iter().enumerate() {
            let w = (0.78 - rank as f32 * 0.09).max(0.30);
            upsert_candidate(&mut pool, tr, w, 0.0, reason, Some(&artist));
        }
    }

    // Add chart tracks as a light cold-start/discovery stream (low base weight so taste decides!).
    for (rank, tr) in chart_tracks.into_iter().enumerate() {
        let w = (0.35 - rank as f32 * 0.008).max(0.10);
        upsert_candidate(&mut pool, tr, w, 0.0, "discovery", None);
    }

    // Look up cached/builtin tags for candidate artists (zero external calls).
    let cand_artist_keys: Vec<(String, String)> = pool
        .values()
        .map(|c| (canonical_artist_key(&c.track.artist), primary_artist_display(&c.track.artist)))
        .filter(|(k, _)| !k.is_empty() && !tags_by_artist.contains_key(k))
        .collect::<HashMap<_, _>>()
        .into_iter()
        .collect();
    let extra_cached = futures::future::join_all(
        cand_artist_keys
            .iter()
            .map(|(_, disp)| cached_or_builtin_tags(svc, disp)),
    )
    .await;
    for ((k, _), maybe_tags) in cand_artist_keys.into_iter().zip(extra_cached) {
        if let Some(t) = maybe_tags {
            tags_by_artist.insert(k, t);
        }
    }

    // 5. Multi-objective intent-aware scoring.
    let time_moods = bucket_aligned_moods(req.session.hour, req.session.is_weekend);
    let mut scored: Vec<(CandidateTrack, f32, Vec<String>)> = Vec::with_capacity(pool.len());

    for (song_k, mut cand) in pool {
        if req.hide_explicit && cand.track.explicit {
            continue;
        }
        if disliked_keys.contains(&song_k) {
            continue;
        }
        let ak = canonical_artist_key(&cand.track.artist);
        if ak.is_empty() || negative_set.contains(&ak) {
            continue;
        }
        let is_known = known_keys.contains(&song_k);
        // Home recs & Radio exclude already-played/liked tracks; Daily Mixes use familiar_tracks separately.
        if is_known {
            continue;
        }

        let a_aff = artist_aff.get(&ak).copied().unwrap_or(0.0);
        let meta = tags_by_artist.get(&ak);
        let cand_genres: Vec<String> = meta.map(|m| m.genres.clone()).unwrap_or_default();
        let cand_moods: Vec<String> = meta.map(|m| m.moods.clone()).unwrap_or_default();
        let cand_eras: Vec<String> = meta.map(|m| m.eras.clone()).unwrap_or_default();

        // Avoided tag suppression.
        let avoided_hits = cand_genres
            .iter()
            .chain(&cand_moods)
            .filter(|t| avoided_tags.contains(&t.to_lowercase()))
            .count();

        let long_tag_aff = soft_tag_vector_affinity(&cand_genres, &cand_moods, &user_genres, &user_moods);
        let sess_tag_aff = soft_tag_vector_affinity(
            &cand_genres,
            &cand_moods,
            &analysis.session_genres,
            &analysis.session_moods,
        );

        // Era alignment.
        let era_aff = if !cand_eras.is_empty() && !user_eras.is_empty() {
            let mut sum = 0.0f32;
            let mut cnt = 0.0f32;
            for e in &cand_eras {
                if let Some(&s) = user_eras.get(e) {
                    sum += s;
                    cnt += 1.0;
                }
            }
            if cnt > 0.0 { (sum / cnt).clamp(-1.0, 1.0) } else { 0.0 }
        } else {
            0.0
        };

        // Energy proximity to inferred user intent energy.
        let track_energy = if !cand_moods.is_empty() {
            cand_moods.iter().map(|m| mood_energy_valence(m).0).sum::<f32>() / cand_moods.len() as f32
        } else {
            0.5
        };
        let energy_fit = 1.0 - (track_energy - analysis.intent.energy).abs() * 1.6;

        // Time-of-day mood alignment.
        let time_hits = cand_moods.iter().filter(|m| time_moods.contains(&m.as_str())).count();
        let time_fit = (time_hits as f32 * 0.35).min(0.75);

        // Version/style preference.
        let styles = detect_versions(&cand.track.title, cand.track.album.as_deref());
        let ver_aff = if styles.is_empty() {
            0.0
        } else {
            let sum: f32 = styles
                .iter()
                .map(|s| {
                    req.version_scores.get(*s).copied().unwrap_or_else(|| {
                        if DEFAULT_ODD_VERSIONS.contains(s) { -0.55 } else { 0.0 }
                    })
                })
                .sum();
            (sum / styles.len() as f32).clamp(-1.0, 1.0)
        };

        let mut sc = cand.channel_score;
        // Multi-channel consensus bonus (e.g., both similar-tracks AND collab recommended this!).
        if cand.channels >= 2 {
            sc += 0.45 * (cand.channels.saturating_sub(1).min(3) as f32);
        }
        sc += cand.collab_boost * (0.45 + 0.35 * analysis.intent.novelty);

        // Artist affinity (boosts familiar artists more in Comfort/ArtistDive, dampens over-dominance in Discovery).
        let artist_weight_scale = match analysis.intent.mode {
            ListeningMode::Comfort => 1.20,
            ListeningMode::ArtistDive => 1.15,
            ListeningMode::Discovery => 0.65,
            _ => 0.90,
        };
        sc += if a_aff >= 0.0 {
            a_aff * artist_weight_scale
        } else {
            a_aff * 2.8
        };
        // In ArtistDive mode, lift the focus artist(s) and their directly bridged scene neighbors.
        if !analysis.dive_artists.is_empty() {
            if analysis.dive_artists.contains(&ak) {
                sc += 0.45;
            } else if cand
                .because_of
                .iter()
                .any(|b| analysis.dive_artists.contains(&canonical_artist_key(b)))
            {
                sc += 0.35;
            }
        }

        // Long-term + short-term session intent alignment.
        let sess_weight = match req.purpose {
            TastePurpose::Radio => 1.15,
            TastePurpose::Home => match analysis.intent.mode {
                ListeningMode::DeepFocus | ListeningMode::ArtistDive | ListeningMode::VibeReset => 1.10,
                _ => 0.80,
            },
            TastePurpose::Discovery => 0.65,
        };
        sc += long_tag_aff * 0.95;
        sc += sess_tag_aff * sess_weight;
        sc += era_aff * 0.28;
        sc += energy_fit * 0.40;
        sc += time_fit * 0.32;
        sc += ver_aff * 1.15;

        // Avoided tags penalty.
        if avoided_hits > 0 {
            sc -= avoided_hits as f32 * 1.65;
        }

        // Session repulsion (skip-streak pivot / recently skipped artists & genres).
        if analysis.repelled_artists.contains(&ak) {
            sc -= 2.4;
        }
        if analysis.intent.vibe_pivot {
            let rep_g = cand_genres.iter().any(|g| analysis.repelled_genres.contains(g));
            let rep_m = cand_moods.iter().any(|m| analysis.repelled_moods.contains(m));
            if rep_g { sc -= 1.25; }
            if rep_m { sc -= 0.70; }
            if !rep_g && !rep_m && long_tag_aff > 0.2 {
                sc += 0.55;
                cand.primary_reason = "vibe_reset".to_string();
            }
        }

        // Novelty lift for unknown-artist discovery when novelty preference is high.
        if a_aff == 0.0 && (long_tag_aff > 0.15 || cand.collab_boost > 0.1) {
            sc += 0.55 * analysis.intent.novelty;
        }

        // Metadata completeness + gentle regional lift (never a filter).
        if cand.track.cover_url.as_deref().is_none_or(|c| c.trim().is_empty()) {
            sc -= 0.28;
        }
        if let (Some(want_r), Some(isrc)) = (user_region.as_deref(), cand.track.isrc.as_deref()) {
            if isrc_country(isrc).as_deref() == Some(want_r) {
                sc += 0.18;
            }
        }

        // Small deterministic daily tie-break jitter for Home freshness.
        if !req.date_key.is_empty() && req.purpose != TastePurpose::Radio {
            sc += daily_jitter(&req.date_key, "recs", &song_k) * 0.6;
        }

        // Refine human-meaningful reason key.
        if cand.primary_reason == "discovery" && sess_tag_aff > 0.35 {
            cand.primary_reason = "session_vibe".to_string();
        } else if cand.primary_reason == "because_of" && long_tag_aff > 0.45 && a_aff == 0.0 {
            cand.primary_reason = "genre_bridge".to_string();
        }

        if sc.is_finite() && sc > -0.6 {
            scored.push((cand, sc, cand_genres));
        }
    }

    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.track.id.cmp(&b.0.track.id)));

    // 6. MMR Diversity Re-Ranking (Maximal Marginal Relevance).
    let max_per_artist = if req.purpose == TastePurpose::Radio { 3 } else { 2 };
    let lambda = match analysis.intent.mode {
        ListeningMode::ArtistDive => 0.84,
        ListeningMode::Discovery | ListeningMode::VibeReset => 0.66,
        _ => 0.74,
    };
    let recommendations = mmr_select(&scored, limit, max_per_artist, lambda);

    // 7. Build Server-Side Daily Mixes for Home (combining familiar + fresh candidates).
    let mixes = if req.purpose == TastePurpose::Home {
        let fresh_for_mixes: Vec<Track> = scored.iter().map(|(c, _, _)| c.track.clone()).collect();
        build_server_daily_mixes(
            &req,
            &analysis,
            &tags_by_artist,
            &user_genres,
            &user_moods,
            &collab_resp,
            &fresh_for_mixes,
        )
    } else {
        Vec::new()
    };

    let artist_tags: Vec<ArtistTags> = tags_by_artist
        .into_values()
        .filter(|t| !t.genres.is_empty() || !t.moods.is_empty() || !t.similar.is_empty())
        .take(24)
        .collect();

    Ok(TasteRecommendResponse {
        recommendations,
        mixes,
        collab: collab_resp,
        artist_tags,
        intent: analysis.intent,
        engine: "server".to_string(),
    })
}

/// Maximal Marginal Relevance (MMR) greedy re-ranker: balances raw taste score
/// with diversity across artists and genres, preventing consecutive artist repeats.
fn mmr_select(
    scored: &[(CandidateTrack, f32, Vec<String>)],
    limit: usize,
    max_per_artist: usize,
    lambda: f32,
) -> Vec<RecommendedTrack> {
    if scored.is_empty() || limit == 0 {
        return Vec::new();
    }
    let pool_len = scored.len().min(limit * 4).max(limit.min(scored.len()));
    let slice = &scored[..pool_len];
    let max_s = slice.first().map(|x| x.1).unwrap_or(1.0);
    let min_s = slice.last().map(|x| x.1).unwrap_or(0.0);
    let span = (max_s - min_s).max(0.001);

    let mut used = vec![false; pool_len];
    let mut per_artist: HashMap<String, usize> = HashMap::new();
    let mut picked_artists: Vec<String> = Vec::with_capacity(limit);
    let mut picked_genres: Vec<Vec<String>> = Vec::with_capacity(limit);
    let mut out: Vec<RecommendedTrack> = Vec::with_capacity(limit);

    while out.len() < limit {
        let last_artist = picked_artists.last().map(String::as_str);
        let mut best_idx: Option<usize> = None;
        let mut best_mmr = f32::NEG_INFINITY;

        for (i, (cand, raw_sc, genres)) in slice.iter().enumerate() {
            if used[i] {
                continue;
            }
            let ak = canonical_artist_key(&cand.track.artist);
            let cnt = per_artist.get(&ak).copied().unwrap_or(0);
            if cnt >= max_per_artist {
                continue;
            }
            // Never place the same artist back-to-back if any alternative exists.
            let back_to_back = last_artist == Some(ak.as_str());
            let rel = ((*raw_sc - min_s) / span).clamp(0.0, 1.0);

            let mut max_red: f32 = if back_to_back { 1.0 } else { 0.0 };
            if cnt > 0 {
                max_red = max_red.max(0.72);
            }
            // Check genre redundancy against the last 3 picked tracks for smooth variety.
            for prev_g in picked_genres.iter().rev().take(3) {
                for g1 in genres {
                    for g2 in prev_g {
                        let sim = genre_similarity(g1, g2);
                        if sim > 0.6 {
                            max_red = max_red.max(sim * 0.42);
                        }
                    }
                }
            }

            let mmr = lambda * rel - (1.0 - lambda) * max_red - if back_to_back { 2.0 } else { 0.0 };
            if mmr > best_mmr {
                best_mmr = mmr;
                best_idx = Some(i);
            }
        }

        let Some(idx) = best_idx else { break };
        used[idx] = true;
        let (ref cand, raw_sc, ref genres) = slice[idx];
        let ak = canonical_artist_key(&cand.track.artist);
        *per_artist.entry(ak.clone()).or_default() += 1;
        picked_artists.push(ak);
        picked_genres.push(genres.clone());
        out.push(RecommendedTrack {
            track: cand.track.clone(),
            score: raw_sc,
            reason: cand.primary_reason.clone(),
            because_of: cand.because_of.clone(),
        });
    }
    out
}

fn spread_mix_artists(tracks: Vec<Track>, max_per_artist: usize, limit: usize) -> Vec<Track> {
    let mut by_artist: Vec<(String, VecDeque<Track>)> = Vec::new();
    for t in tracks {
        let a = canonical_artist_key(&t.artist);
        if let Some((_, q)) = by_artist.iter_mut().find(|(k, _)| *k == a) {
            if q.len() < max_per_artist {
                q.push_back(t);
            }
        } else {
            let mut q = VecDeque::new();
            q.push_back(t);
            by_artist.push((a, q));
        }
    }
    let mut out: Vec<Track> = Vec::with_capacity(limit);
    let mut last: Option<String> = None;
    while out.len() < limit {
        let pick = by_artist
            .iter()
            .enumerate()
            .filter(|(_, (a, q))| !q.is_empty() && last.as_deref() != Some(a.as_str()))
            .max_by(|(ia, (_, qa)), (ib, (_, qb))| qa.len().cmp(&qb.len()).then_with(|| ib.cmp(ia)))
            .map(|(i, _)| i);
        let Some(i) = pick else { break };
        if let Some(t) = by_artist[i].1.pop_front() {
            last = Some(by_artist[i].0.clone());
            out.push(t);
        }
    }
    out
}

fn mix_subtitle(tracks: &[Track], lead_tag: Option<&str>) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for t in tracks {
        let disp = primary_artist_display(&t.artist);
        let k = canonical_artist_key(&disp);
        if !k.is_empty() && seen.insert(k) {
            names.push(disp);
            if names.len() >= 3 {
                break;
            }
        }
    }
    let joined = names.join(", ");
    match (lead_tag, joined.is_empty()) {
        (Some(tag), false) => format!("{tag} · {joined}"),
        (Some(tag), true) => tag.to_string(),
        (None, false) => joined,
        (None, true) => String::new(),
    }
}

fn build_server_daily_mixes(
    req: &TasteRecommendRequest,
    analysis: &SessionAnalysis,
    tags_by_artist: &HashMap<String, ArtistTags>,
    user_genres: &HashMap<String, f32>,
    user_moods: &HashMap<String, f32>,
    collab: &TasteCollabResponse,
    fresh_pool: &[Track],
) -> Vec<DailyMix> {
    let negative_set: HashSet<String> = req
        .negative_artists
        .iter()
        .map(|a| canonical_artist_key(a))
        .collect();
    let disliked: HashSet<String> = req.disliked_track_keys.iter().cloned().collect();
    let known: HashSet<String> = req.known_track_keys.iter().cloned().collect();
    let date_key = if req.date_key.is_empty() { "today" } else { req.date_key.as_str() };

    let track_ok = |t: &Track| -> bool {
        let sk = canonical_song_key(&t.title, &t.artist);
        let ak = canonical_artist_key(&t.artist);
        !(req.hide_explicit && t.explicit) && !disliked.contains(&sk) && !negative_set.contains(&ak)
    };

    let mut familiar: Vec<Track> = Vec::new();
    let mut fam_seen: HashSet<String> = HashSet::new();
    for t in &req.familiar_tracks {
        let k = canonical_song_key(&t.title, &t.artist);
        if track_ok(t) && fam_seen.insert(k) {
            familiar.push(t.clone());
        }
    }
    let mut fresh: Vec<Track> = Vec::new();
    let mut fresh_seen: HashSet<String> = HashSet::new();
    for t in fresh_pool {
        let k = canonical_song_key(&t.title, &t.artist);
        if track_ok(t) && !fam_seen.contains(&k) && fresh_seen.insert(k) {
            fresh.push(t.clone());
        }
    }
    if familiar.is_empty() && fresh.is_empty() {
        return Vec::new();
    }

    let genres_of = |t: &Track| -> Vec<String> {
        let ak = canonical_artist_key(&t.artist);
        if let Some(m) = tags_by_artist.get(&ak) {
            if !m.genres.is_empty() {
                return m.genres.clone();
            }
        }
        builtin_artist_genres(&ak)
            .map(|gs| gs.iter().map(|s| (*s).to_string()).collect())
            .unwrap_or_default()
    };
    let moods_of = |t: &Track| -> Vec<String> {
        let ak = canonical_artist_key(&t.artist);
        if let Some(m) = tags_by_artist.get(&ak) {
            if !m.moods.is_empty() {
                return m.moods.clone();
            }
        }
        let mut ms = Vec::new();
        for g in genres_of(t) {
            for &m in genre_default_moods(&g) {
                if !ms.iter().any(|x| x == m) {
                    ms.push(m.to_string());
                }
            }
        }
        ms
    };

    let mut mixes: Vec<DailyMix> = Vec::new();

    // 1. Active Session / Intent Mix (when session has a distinct active mood/mode or vibe pivot).
    let show_intent_mix = analysis.intent.vibe_pivot
        || !matches!(analysis.intent.mode, ListeningMode::Balanced)
        || !analysis.session_genres.is_empty();
    if show_intent_mix {
        let mix_id = format!("daily-intent-{}", analysis.intent.mode.as_str());
        let mut candidates: Vec<(Track, f32)> = familiar
            .iter()
            .chain(fresh.iter())
            .filter(|t| {
                let ak = canonical_artist_key(&t.artist);
                !analysis.repelled_artists.contains(&ak)
            })
            .map(|t| {
                let gs = genres_of(t);
                let ms = moods_of(t);
                let sess_aff = soft_tag_vector_affinity(&gs, &ms, &analysis.session_genres, &analysis.session_moods);
                let focus_hit = gs
                    .iter()
                    .map(|g| {
                        analysis
                            .intent
                            .focus_genres
                            .iter()
                            .map(|fg| genre_similarity(g, fg))
                            .fold(0.0f32, f32::max)
                    })
                    .fold(0.0f32, f32::max);
                let e = if !ms.is_empty() {
                    ms.iter().map(|m| mood_energy_valence(m).0).sum::<f32>() / ms.len() as f32
                } else {
                    0.5
                };
                let e_fit = 1.0 - (e - analysis.intent.energy).abs() * 1.5;
                let sk = canonical_song_key(&t.title, &t.artist);
                let mut sc = sess_aff * 1.2 + focus_hit * 1.1 + e_fit * 0.55 + daily_jitter(date_key, &mix_id, &sk);
                if analysis.intent.vibe_pivot && gs.iter().any(|g| analysis.repelled_genres.contains(g)) {
                    sc -= 1.5;
                }
                (t.clone(), sc)
            })
            .collect();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
        let tracks = spread_mix_artists(candidates.into_iter().map(|(t, _)| t).collect(), 2, 18);
        if tracks.len() >= 3 {
            let lead_genre = analysis.intent.focus_genres.first().cloned();
            let (title, hue) = match analysis.intent.mode {
                ListeningMode::VibeReset => ("Fresh Pivot Mix".to_string(), 168u16),
                ListeningMode::DeepFocus => ("Deep Focus Mix".to_string(), 210u16),
                ListeningMode::ArtistDive => ("In the Zone".to_string(), 292u16),
                ListeningMode::Comfort => ("Comfort Mix".to_string(), 28u16),
                ListeningMode::HighEnergy => ("Energy Boost".to_string(), 12u16),
                ListeningMode::LateNightWindDown => ("Wind Down Mix".to_string(), 248u16),
                ListeningMode::Discovery | ListeningMode::Balanced => (
                    lead_genre
                        .as_deref()
                        .map(|g| format!("{g} Session"))
                        .unwrap_or_else(|| "Right Now Mix".to_string()),
                    328u16,
                ),
            };
            let tags: Vec<String> = analysis
                .intent
                .focus_moods
                .iter()
                .take(2)
                .cloned()
                .collect();
            let subtitle = mix_subtitle(&tracks, lead_genre.as_deref());
            mixes.push(DailyMix {
                id: mix_id,
                kind: "intent".to_string(),
                title,
                subtitle,
                tags,
                accent_hue: hue,
                tracks,
            });
        }
    }

    // 2. Genre / Cluster Daily Mixes (up to 3 top genres with soft genre-family matching).
    let mut top_genres: Vec<(String, f32)> = user_genres
        .iter()
        .filter(|(_, s)| **s > 0.12)
        .map(|(g, s)| (g.clone(), *s))
        .collect();
    top_genres.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut used_globally: HashSet<String> = HashSet::new();
    let mut cluster_idx = 0usize;

    for (genre, _) in top_genres.into_iter().take(6) {
        if cluster_idx >= 3 {
            break;
        }
        let mix_id = format!("daily-genre-{}", cluster_idx + 1);
        let genre_match_score = |t: &Track| -> f32 {
            genres_of(t)
                .iter()
                .map(|g| genre_similarity(g, &genre))
                .fold(0.0f32, f32::max)
        };
        let mut fam_scored: Vec<(Track, f32)> = familiar
            .iter()
            .filter_map(|t| {
                let sk = canonical_song_key(&t.title, &t.artist);
                if used_globally.contains(&sk) {
                    return None;
                }
                let gm = genre_match_score(t);
                (gm >= 0.65).then(|| (t.clone(), gm * 1.4 + daily_jitter(date_key, &mix_id, &sk)))
            })
            .collect();
        fam_scored.sort_by(|a, b| b.1.total_cmp(&a.1));

        let mut fresh_scored: Vec<(Track, f32)> = fresh
            .iter()
            .filter_map(|t| {
                let sk = canonical_song_key(&t.title, &t.artist);
                if used_globally.contains(&sk) {
                    return None;
                }
                let gm = genre_match_score(t);
                (gm >= 0.65).then(|| (t.clone(), gm * 1.35 + daily_jitter(date_key, &mix_id, &sk)))
            })
            .collect();
        fresh_scored.sort_by(|a, b| b.1.total_cmp(&a.1));

        let mut combined: Vec<Track> = Vec::new();
        let mut fi = fam_scored.into_iter().map(|(t, _)| t);
        let mut ni = fresh_scored.into_iter().map(|(t, _)| t);
        for _ in 0..24 {
            if let Some(t) = fi.next() { combined.push(t); }
            if let Some(t) = ni.next() { combined.push(t); }
        }
        let tracks = spread_mix_artists(combined, 3, 20);
        if tracks.len() < 3 {
            continue;
        }
        for t in &tracks {
            used_globally.insert(canonical_song_key(&t.title, &t.artist));
        }
        cluster_idx += 1;
        let subtitle = mix_subtitle(&tracks, Some(&genre));
        let hue = (fnv1a(&genre) % 360) as u16;
        mixes.push(DailyMix {
            id: mix_id,
            kind: "genre".to_string(),
            title: format!("Daily Mix {cluster_idx}"),
            subtitle,
            tags: vec![genre],
            accent_hue: hue,
            tracks,
        });
    }

    // 3. Time-of-Day Mix.
    {
        let bi = hour_bucket_index(req.session.hour);
        let (slug, title, hue) = match bi {
            0 => ("morning", "Morning Mix", 36u16),
            1 => ("afternoon", "Afternoon Mix", 198u16),
            2 => ("evening", "Evening Mix", 278u16),
            _ => ("night", "Late Night Mix", 242u16),
        };
        let mix_id = format!("daily-time-{slug}");
        let aligned = bucket_aligned_moods(req.session.hour, req.session.is_weekend);
        let mut scored: Vec<(Track, f32)> = familiar
            .iter()
            .chain(fresh.iter())
            .map(|t| {
                let ms = moods_of(t);
                let hits = ms.iter().filter(|m| aligned.contains(&m.as_str())).count() as f32;
                let tag_aff = soft_tag_vector_affinity(&genres_of(t), &ms, user_genres, user_moods);
                let sk = canonical_song_key(&t.title, &t.artist);
                (t.clone(), hits * 0.7 + tag_aff * 0.8 + daily_jitter(date_key, &mix_id, &sk))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let tracks = spread_mix_artists(scored.into_iter().map(|(t, _)| t).collect(), 2, 18);
        if tracks.len() >= 3 {
            mixes.push(DailyMix {
                id: mix_id,
                kind: "time".to_string(),
                title: title.to_string(),
                subtitle: mix_subtitle(&tracks, None),
                tags: aligned.iter().take(2).map(|s| (*s).to_string()).collect(),
                accent_hue: hue,
                tracks,
            });
        }
    }

    // 4. Discovery Mix (strictly fresh, guided by collaborative filtering).
    {
        let mix_id = "daily-discovery".to_string();
        let collab_map: HashMap<String, f32> = collab
            .recommendations
            .iter()
            .map(|r| (canonical_artist_key(&r.artist), r.score.clamp(0.0, 2.5)))
            .collect();
        let mut scored: Vec<(Track, f32)> = fresh
            .iter()
            .filter(|t| !known.contains(&canonical_song_key(&t.title, &t.artist)))
            .map(|t| {
                let ak = canonical_artist_key(&t.artist);
                let cb = collab_map.get(&ak).copied().unwrap_or(0.0);
                let tag_aff = soft_tag_vector_affinity(&genres_of(t), &moods_of(t), user_genres, user_moods);
                let sk = canonical_song_key(&t.title, &t.artist);
                (t.clone(), 0.7 + cb * 0.9 + tag_aff * 0.85 + daily_jitter(date_key, &mix_id, &sk))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let tracks = spread_mix_artists(scored.into_iter().map(|(t, _)| t).collect(), 2, 18);
        if tracks.len() >= 3 {
            mixes.push(DailyMix {
                id: mix_id,
                kind: "discovery".to_string(),
                title: "Discovery Mix".to_string(),
                subtitle: mix_subtitle(&tracks, None),
                tags: vec!["Fresh".to_string()],
                accent_hue: 325,
                tracks,
            });
        }
    }

    mixes
}

// ------------------------------------------------ onboarding starter catalog

/// Curated diverse starter artists across major genres so onboarding works
/// immediately even when charts skew heavily toward one genre or when offline.
const STARTER_ARTISTS: &[(&str, &str)] = &[
    ("Tame Impala", "Indie"),
    ("Arctic Monkeys", "Indie Rock"),
    ("Men I Trust", "Dream Pop"),
    ("Daft Punk", "Electronic"),
    ("Fred again..", "Electronic"),
    ("Kaytranada", "House"),
    ("Kendrick Lamar", "Hip-Hop"),
    ("Tyler, The Creator", "Hip-Hop"),
    ("Travis Scott", "Trap"),
    ("Frank Ocean", "R&B"),
    ("SZA", "R&B"),
    ("The Weeknd", "Pop"),
    ("Billie Eilish", "Pop"),
    ("Dua Lipa", "Pop"),
    ("Radiohead", "Alternative"),
    ("Linkin Park", "Rock"),
    ("Metallica", "Metal"),
    ("Deftones", "Metal"),
    ("Miles Davis", "Jazz"),
    ("Nujabes", "Lo-Fi"),
    ("Lana Del Rey", "Dream Pop"),
    ("Bon Iver", "Folk & Acoustic"),
    ("Bad Bunny", "Latin"),
    ("Burna Boy", "Afrobeats"),
    ("Hans Zimmer", "Soundtrack"),
    ("Ludovico Einaudi", "Classical"),
    ("Gorillaz", "Alternative"),
    ("Flume", "Electronic"),
];

#[derive(Deserialize)]
struct DzChartTracks {
    #[serde(default)]
    data: Vec<deezer::DzTrack>,
}

#[derive(Deserialize)]
struct DzChartArtists {
    #[serde(default)]
    data: Vec<DzChartArtist>,
}

#[derive(Deserialize)]
struct DzChartArtist {
    #[serde(default)]
    name: String,
    picture_medium: Option<String>,
    picture_big: Option<String>,
}

/// `GET /v1/taste/onboarding`: genre-diverse starter artists and songs.
pub async fn onboarding_catalog(svc: &Services) -> Result<OnboardingCatalogResponse, ApiErr> {
    const KEY: &str = "taste:onboarding:v1";
    if let Some(hit) = cache::get_json::<OnboardingCatalogResponse>(&*svc.cache, KEY).await {
        return Ok(hit);
    }

    let timeout = svc.config.provider_timeout_ms.min(4_000);
    let (tracks_res, artists_res) = futures::join!(
        with_timeout(
            &*svc.rt,
            timeout,
            fetch_json::<DzChartTracks>(svc, HttpRequest::get("https://api.deezer.com/chart/0/tracks?limit=30"))
        ),
        with_timeout(
            &*svc.rt,
            timeout,
            fetch_json::<DzChartArtists>(svc, HttpRequest::get("https://api.deezer.com/chart/0/artists?limit=24"))
        ),
    );

    let tracks: Vec<Track> = tracks_res
        .and_then(Result::ok)
        .map(|r| r.data.into_iter().map(|t| deezer::to_candidate(t).track).collect())
        .unwrap_or_default();

    let mut artists: Vec<OnboardingArtist> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut chart_photos: HashMap<String, String> = HashMap::new();
    let mut chart_extra: Vec<(String, Option<String>)> = Vec::new();

    if let Some(Ok(chart_a)) = artists_res {
        for a in chart_a.data {
            let name = a.name.trim().to_string();
            let k = canonical_artist_key(&name);
            if k.is_empty() {
                continue;
            }
            let photo = a.picture_big.or(a.picture_medium);
            if let Some(ref p) = photo {
                chart_photos.insert(k.clone(), p.clone());
            }
            chart_extra.push((name, photo));
        }
    }

    for &(name, genre) in STARTER_ARTISTS {
        let k = canonical_artist_key(name);
        if seen.insert(k.clone()) {
            let sample_track = tracks
                .iter()
                .find(|t| canonical_artist_key(&t.artist) == k)
                .cloned();
            let photo = chart_photos
                .get(&k)
                .cloned()
                .or_else(|| sample_track.as_ref().and_then(|t| t.cover_url.clone()));
            artists.push(OnboardingArtist {
                name: name.to_string(),
                genre: genre.to_string(),
                photo,
                sample_track,
            });
        }
    }

    for (name, photo) in chart_extra.into_iter().take(18) {
        let k = canonical_artist_key(&name);
        if !k.is_empty() && seen.insert(k.clone()) {
            let sample_track = tracks
                .iter()
                .find(|t| canonical_artist_key(&t.artist) == k)
                .cloned();
            artists.push(OnboardingArtist {
                name,
                genre: "Trending".to_string(),
                photo: photo.or_else(|| sample_track.as_ref().and_then(|t| t.cover_url.clone())),
                sample_track,
            });
            if artists.len() >= 40 {
                break;
            }
        }
    }

    let mut genres: Vec<String> = Vec::new();
    for a in &artists {
        if !genres.contains(&a.genre) {
            genres.push(a.genre.clone());
        }
    }

    let resp = OnboardingCatalogResponse {
        genres,
        artists,
        tracks: tracks.into_iter().take(24).collect(),
    };
    let ttl = if resp.tracks.is_empty() { 300 } else { ONBOARDING_TTL_S };
    cache::put_json(&*svc.cache, KEY, &resp, ttl).await;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;
    use glyph_protocol::taste::{
        SessionEventSignal, TasteArtistSignal, TasteSeedTrack, TasteSessionContext, TasteTagSignal,
    };
    use glyph_protocol::ProviderId;

    fn sig(artist: &str, weight: f32) -> TasteArtistSignal {
        TasteArtistSignal {
            artist: artist.into(),
            weight,
            recent_weight: None,
        }
    }

    fn mk_track(title: &str, artist: &str) -> Track {
        Track {
            id: format!("{artist}:{title}"),
            source: ProviderId::Deezer,
            title: title.into(),
            artist: artist.into(),
            album: None,
            duration_ms: Some(210_000),
            cover_url: Some("https://img.example/c.jpg".into()),
            isrc: None,
            permalink: None,
            external_ids: Default::default(),
            explicit: false,
        }
    }

    #[test]
    fn tags_batch_merges_lastfm_and_deezer_with_eras_and_caches() {
        let h = harness(all_secrets());
        h.http.on(
            "ws.audioscrobbler.com",
            200,
            r#"{"toptags":{"tag":[{"name":"psychedelic rock","count":100},{"name":"2010s","count":85},{"name":"seen live","count":90},{"name":"dream pop","count":60}]},"similarartists":{"artist":[{"name":"Pond"},{"name":"MGMT"}]}}"#,
        );
        h.http.on(
            "api.deezer.com/search/artist",
            200,
            r#"{"data":[{"id":42,"name":"Tame Impala","nb_fan":3000000}]}"#,
        );
        h.http.on(
            "api.deezer.com/artist/42/albums",
            200,
            r#"{"data":[{"genre_id":85}]}"#,
        );
        h.http.on(
            "api.deezer.com/artist/42/related",
            200,
            r#"{"data":[{"name":"Unknown Mortal Orchestra"}]}"#,
        );

        let resp = block_on(tags_batch(
            &h.svc,
            TasteTagsRequest {
                artists: vec!["Tame Impala".into(), "tame impala".into(), "".into()],
            },
        ))
        .unwrap();
        assert_eq!(resp.items.len(), 1);
        let item = &resp.items[0];
        assert!(item.genres.contains(&"Psychedelic".to_string()));
        assert!(item.genres.contains(&"Dream Pop".to_string()));
        assert!(item.genres.contains(&"Alternative".to_string()));
        assert_eq!(item.eras, vec!["2010s"]);
        assert!(item.similar.contains(&"Pond".to_string()));
        assert!(item.similar.contains(&"Unknown Mortal Orchestra".to_string()));
    }

    #[test]
    fn collaborative_filtering_hub_demotes_and_contrasts_negatives() {
        let h = harness(Default::default());
        // User A listens to Tame Impala, Men I Trust, and Crumb.
        let _ = block_on(collab(
            &h.svc,
            Some("acc:user_a"),
            TasteCollabRequest {
                positive: vec![sig("Tame Impala", 0.9), sig("Men I Trust", 0.85), sig("Crumb", 0.8)],
                session_pairs: vec![("Tame Impala".into(), "Crumb".into())],
                limit: Some(10),
                contribute: true,
                ..Default::default()
            },
        ))
        .unwrap();

        // User B also listens to Tame Impala, Men I Trust, and Khruangbin.
        let _ = block_on(collab(
            &h.svc,
            Some("acc:user_b"),
            TasteCollabRequest {
                positive: vec![sig("Tame Impala", 1.0), sig("Men I Trust", 0.7), sig("Khruangbin", 0.9)],
                limit: Some(10),
                contribute: true,
                ..Default::default()
            },
        ))
        .unwrap();

        // User C likes Tame Impala & Men I Trust, hates Khruangbin -> should get Crumb!
        let rec = block_on(collab(
            &h.svc,
            Some("acc:user_c"),
            TasteCollabRequest {
                positive: vec![sig("Tame Impala", 0.9), sig("Men I Trust", 0.8)],
                negative: vec!["Khruangbin".into()],
                limit: Some(10),
                contribute: false,
                ..Default::default()
            },
        ))
        .unwrap();

        assert!(rec.model_ready);
        assert!(rec.contributors >= 2);
        assert_eq!(rec.recommendations.len(), 1);
        assert_eq!(rec.recommendations[0].artist, "Crumb");
        assert!(rec.recommendations[0].collaborative);
        assert!(!rec.recommendations[0].because_of.is_empty());
    }

    #[test]
    fn collaborative_filtering_dedupes_same_voter_against_stuffing() {
        let h = harness(Default::default());
        let req = TasteCollabRequest {
            positive: vec![sig("Artist A", 1.0), sig("Artist B", 1.0)],
            contribute: true,
            ..Default::default()
        };
        let _ = block_on(collab(&h.svc, Some("acc:spammer"), req.clone())).unwrap();
        h.rt.advance(2 * HOUR * 1000);
        let _ = block_on(collab(&h.svc, Some("acc:spammer"), req)).unwrap();

        let node: CoOccurNode =
            block_on(cache::get_json(&*h.svc.cache, &cooccur_key("artist a"))).unwrap();
        assert_eq!(node.total_voters, 1);
        assert_eq!(node.neighbors["artist b"].co_voters, 1);

        h.rt.advance(2 * HOUR * 1000);
        let evolved = TasteCollabRequest {
            positive: vec![sig("Artist A", 1.0), sig("Artist B", 1.0), sig("Artist C", 0.9)],
            contribute: true,
            ..Default::default()
        };
        let _ = block_on(collab(&h.svc, Some("acc:spammer"), evolved)).unwrap();
        let node_after: CoOccurNode =
            block_on(cache::get_json(&*h.svc.cache, &cooccur_key("artist a"))).unwrap();
        assert_eq!(node_after.total_voters, 1);
        assert_eq!(node_after.neighbors["artist b"].co_voters, 1);
        assert_eq!(node_after.neighbors["artist c"].co_voters, 1);
    }

    #[test]
    fn tags_batch_parses_single_object_lastfm_payloads() {
        let h = harness(all_secrets());
        h.http.on(
            "ws.audioscrobbler.com",
            200,
            r#"{"toptags":{"tag":{"name":"shoegaze","count":"50"}},"similarartists":{"artist":{"name":"Slowdive"}}}"#,
        );
        let resp = block_on(tags_batch(
            &h.svc,
            TasteTagsRequest {
                artists: vec!["My Bloody Valentine".into()],
            },
        ))
        .unwrap();
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].genres, vec!["Shoegaze"]);
        assert_eq!(resp.items[0].similar, vec!["Slowdive"]);
    }

    #[test]
    fn server_recommend_infers_vibe_reset_pivots_away_from_skipped_genre_and_builds_mixes() {
        let h = harness(Default::default());
        // Pre-populate cached artist top tracks so the test exercises full server ranking & mixes offline.
        block_on(cache::put_json(
            &*h.svc.cache,
            "tastetop:v1:men i trust",
            &SimilarResponse {
                tracks: vec![
                    mk_track("Show Me How", "Men I Trust"),
                    mk_track("Lauren", "Men I Trust"),
                    mk_track("Numb", "Men I Trust"),
                ],
            },
            DAY,
        ));
        block_on(cache::put_json(
            &*h.svc.cache,
            "tastetop:v1:beach house",
            &SimilarResponse {
                tracks: vec![
                    mk_track("Space Song", "Beach House"),
                    mk_track("Myth", "Beach House"),
                ],
            },
            DAY,
        ));
        block_on(cache::put_json(
            &*h.svc.cache,
            "tastetop:v1:metallica",
            &SimilarResponse {
                tracks: vec![
                    mk_track("Enter Sandman", "Metallica"),
                    mk_track("Master of Puppets", "Metallica"),
                ],
            },
            DAY,
        ));
        block_on(cache::put_json(
            &*h.svc.cache,
            "tastetop:v1:crumb",
            &SimilarResponse {
                tracks: vec![
                    mk_track("Locket", "Crumb"),
                    mk_track("Ghostride", "Crumb"),
                ],
            },
            DAY,
        ));

        // Seed community CF: Men I Trust -> Beach House & Crumb.
        let _ = block_on(collab(
            &h.svc,
            Some("acc:community1"),
            TasteCollabRequest {
                positive: vec![sig("Men I Trust", 0.9), sig("Beach House", 0.9), sig("Crumb", 0.85)],
                contribute: true,
                ..Default::default()
            },
        ))
        .unwrap();

        // User likes both Metallica & Men I Trust long-term, BUT just skipped 2 Metallica songs in a row!
        let req = TasteRecommendRequest {
            purpose: TastePurpose::Home,
            positive_artists: vec![sig("Metallica", 0.95), sig("Men I Trust", 0.85)],
            seed_tracks: vec![
                TasteSeedTrack { title: "Enter Sandman".into(), artist: "Metallica".into(), album: None, weight: 1.0, is_session: true },
                TasteSeedTrack { title: "Show Me How".into(), artist: "Men I Trust".into(), album: None, weight: 0.9, is_session: false },
            ],
            familiar_tracks: vec![
                mk_track("Show Me How", "Men I Trust"),
                mk_track("Let It Happen", "Tame Impala"),
                mk_track("Enter Sandman", "Metallica"),
            ],
            known_track_keys: vec!["show me how|men i trust".into()],
            genre_scores: vec![
                TasteTagSignal { tag: "Metal".into(), score: 0.9 },
                TasteTagSignal { tag: "Dream Pop".into(), score: 0.85 },
            ],
            mood_scores: vec![
                TasteTagSignal { tag: "Intense".into(), score: 0.8 },
                TasteTagSignal { tag: "Dreamy".into(), score: 0.85 },
            ],
            session: TasteSessionContext {
                recent_events: vec![
                    SessionEventSignal {
                        title: "Enter Sandman".into(),
                        artist: "Metallica".into(),
                        album: None,
                        outcome: SessionEventOutcome::SkippedEarly,
                        context: SessionEventContext::Radio,
                        age_mins: 1,
                    },
                    SessionEventSignal {
                        title: "Master of Puppets".into(),
                        artist: "Metallica".into(),
                        album: None,
                        outcome: SessionEventOutcome::SkippedEarly,
                        context: SessionEventContext::Radio,
                        age_mins: 2,
                    },
                ],
                recent_skip_streak: 2,
                hour: 23,
                is_weekend: false,
                ..Default::default()
            },
            variety_preference: 0.6,
            date_key: "2026-11-23".into(),
            limit: Some(10),
            contribute: false,
            ..Default::default()
        };

        let resp = block_on(recommend(&h.svc, Some("acc:tester"), req)).unwrap();
        assert_eq!(resp.engine, "server");
        assert_eq!(resp.intent.mode, ListeningMode::VibeReset);
        assert!(resp.intent.vibe_pivot);
        assert!(!resp.recommendations.is_empty());
        // Top recommendation should pivot to Dream Pop / Psychedelic (Beach House / Crumb / Men I Trust), NOT Metallica!
        assert_ne!(canonical_artist_key(&resp.recommendations[0].track.artist), "metallica");
        // Known track must not appear in fresh recommendations.
        assert!(!resp.recommendations.iter().any(|r| r.track.title == "Show Me How"));
        // Server built Daily Mixes including an intent/pivot mix!
        assert!(resp.mixes.iter().any(|m| m.kind == "intent"));
    }

    #[test]
    fn http_routes_advertise_taste_engine_and_roundtrip_recommend_endpoint() {
        use crate::{Method, Request, Server};
        let h = harness(Default::default());
        let srv = Server::from_arc(h.svc.clone());

        // /v1/health advertises both TASTE and TASTE_ENGINE
        let health_resp = block_on(srv.handle(Request::new(Method::Get, "/v1/health", vec![], vec![], Some("127.0.0.1".into()))));
        assert_eq!(health_resp.status, 200);
        let health: glyph_protocol::HealthResponse = serde_json::from_slice(health_resp.bytes().unwrap()).unwrap();
        assert!(health.supports(glyph_protocol::capability::TASTE));
        assert!(health.supports(glyph_protocol::capability::TASTE_ENGINE));

        // POST /v1/taste/recommend roundtrip
        let body = serde_json::to_vec(&TasteRecommendRequest {
            purpose: TastePurpose::Home,
            positive_artists: vec![sig("Daft Punk", 1.0)],
            date_key: "2026-11-23".into(),
            session: TasteSessionContext { hour: 21, ..Default::default() },
            contribute: false,
            ..Default::default()
        })
        .unwrap();
        let rec_resp = block_on(srv.handle(Request::new(
            Method::Post,
            "/v1/taste/recommend",
            vec![("content-type".into(), "application/json".into())],
            body,
            Some("127.0.0.1".into()),
        )));
        assert_eq!(rec_resp.status, 200);
        let parsed: TasteRecommendResponse = serde_json::from_slice(rec_resp.bytes().unwrap()).unwrap();
        assert_eq!(parsed.engine, "server");

        // Wrong method on /v1/taste/recommend -> 405
        let wrong = block_on(srv.handle(Request::new(Method::Get, "/v1/taste/recommend", vec![], vec![], Some("127.0.0.1".into()))));
        assert_eq!(wrong.status, 405);
    }
}
