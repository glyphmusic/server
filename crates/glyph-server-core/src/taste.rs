//! Server-side hybrid taste engine:
//! - `/v1/taste/tags`: shared 30-day cache of canonical genres, moods, tags and
//!   related artists (Last.fm `artist.getTopTags` + Deezer genres/related).
//! - `/v1/taste/collab`: privacy-preserving item-item co-occurrence collaborative
//!   filtering with sybil-resistant hashed voter deduplication and cold-start
//!   related-artist blending.
//! - `/v1/taste/onboarding`: genre-diverse starter artists and songs for
//!   first-run onboarding.

use crate::cache::{self, DAY, HOUR};
use crate::error::ApiErr;
use crate::merge::{normalize_artist, plain_words};
use crate::providers::{build_url, deezer, fetch_json};
use crate::traits::{with_timeout, HttpRequest, RateDecision, Services};
use glyph_protocol::taste::{
    classify_tags, ArtistTags, CollabArtistRec, OnboardingArtist, OnboardingCatalogResponse,
    TasteCollabRequest, TasteCollabResponse, TasteTagsRequest, TasteTagsResponse,
};
use glyph_protocol::Track;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

const TAG_FOUND_TTL_S: u64 = 30 * DAY;
const TAG_MISS_TTL_S: u64 = 12 * HOUR;
const COOCCUR_TTL_S: u64 = 90 * DAY;
const ONBOARDING_TTL_S: u64 = 6 * HOUR;
const CONTRIB_COOLDOWN_S: u64 = HOUR;

const MAX_TAG_BATCH: usize = 30;
const MAX_POSITIVE_SIGNALS: usize = 40;
const MAX_NEGATIVE_SIGNALS: usize = 40;
const MAX_AVOIDED_TAGS: usize = 30;
const MAX_CONTRIB_ARTISTS: usize = 10;
const MAX_SUPPORTERS: usize = 250;
const MAX_NEIGHBORS: usize = 40;
const DEFAULT_COLLAB_LIMIT: usize = 15;
const MAX_COLLAB_LIMIT: usize = 30;

fn canonical_artist_key(name: &str) -> String {
    let n = normalize_artist(name);
    if !n.is_empty() {
        n
    } else {
        plain_words(name)
    }
}

fn voter_hash(voter: &str) -> String {
    Sha256::digest(voter.as_bytes())
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
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
                .take(16)
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

/// Fetches (or returns cached) canonical genres, moods, tags and similar artists
/// for a single artist name.
pub async fn lookup_artist_tags(svc: &Services, artist: &str) -> ArtistTags {
    let trimmed = artist.trim();
    let norm = canonical_artist_key(trimmed);
    if norm.is_empty() {
        return ArtistTags { artist: trimmed.to_string(), ..Default::default() };
    }
    let key = format!("tastetag:v1:{norm}");
    if let Some(mut hit) = cache::get_json::<ArtistTags>(&*svc.cache, &key).await {
        hit.artist = trimmed.to_string();
        return hit;
    }

    let ((lfm_tags, lfm_sim), (dz_tags, dz_sim)) = futures::join!(
        fetch_lastfm_tags(svc, trimmed),
        fetch_deezer_tags(svc, trimmed),
    );
    let mut raw_tags = lfm_tags;
    raw_tags.extend(dz_tags);
    let (genres, moods, tags) = classify_tags(&raw_tags, trimmed);

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

    let item = ArtistTags {
        artist: trimmed.to_string(),
        genres,
        moods,
        tags,
        similar,
    };
    let ttl = if item.genres.is_empty() && item.tags.is_empty() && item.similar.is_empty() {
        TAG_MISS_TTL_S
    } else {
        TAG_FOUND_TTL_S
    };
    cache::put_json(&*svc.cache, &key, &item, ttl).await;
    item
}

/// `POST /v1/taste/tags`: batch genre/mood/similar tag lookup.
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
        let mut entries: Vec<(String, NeighborEntry)> = std::mem::take(&mut self.neighbors).into_iter().collect();
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

/// `POST /v1/taste/collab`: privacy-preserving item-item collaborative filtering.
///
/// Optionally ingests the caller's top anonymized artist pairs into the shared
/// co-occurrence graph (deduplicated per hashed voter + rate limited), then
/// scores candidate artists using lift-damped item-item similarity blended with
/// cached related-artist fallbacks when community data is still sparse.
pub async fn collab(
    svc: &Services,
    voter: Option<&str>,
    req: TasteCollabRequest,
) -> Result<TasteCollabResponse, ApiErr> {
    // Sanitize & deduplicate positive signals.
    let mut pos_map: HashMap<String, (String, f32)> = HashMap::new();
    for sig in req.positive.into_iter().take(MAX_POSITIVE_SIGNALS) {
        let name = sig.artist.trim();
        if name.is_empty() || name.len() > 120 || !sig.weight.is_finite() {
            continue;
        }
        let w = sig.weight.clamp(0.0, 1.0);
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
    for neg in req.negative.into_iter().take(MAX_NEGATIVE_SIGNALS) {
        let k = canonical_artist_key(&neg);
        if !k.is_empty() {
            excluded.insert(k);
        }
    }

    let avoided_tags: HashSet<String> = req
        .avoided_tags
        .into_iter()
        .take(MAX_AVOIDED_TAGS)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let limit = req.limit.unwrap_or(DEFAULT_COLLAB_LIMIT).clamp(1, MAX_COLLAB_LIMIT);

    // 1. Optional contribution into item-item co-occurrence graph.
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
                            let pair_w = w_a.min(*w_b);
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
                    futures::future::join_all(
                        writes
                            .into_iter()
                            .map(|(k, n)| async move { cache::put_json(&*svc.cache, &k, &n, COOCCUR_TTL_S).await }),
                    )
                    .await;
                }
            }
        }
    }

    // 2. Query item-item co-occurrence nodes for top positive seeds.
    let query_seeds: Vec<&(String, String, f32)> = positive.iter().take(15).collect();
    let seed_nodes = futures::future::join_all(query_seeds.iter().map(|(k, _, _)| {
        let key = cooccur_key(k);
        async move { cache::get_json::<CoOccurNode>(&*svc.cache, &key).await }
    }))
    .await;

    struct CandAcc {
        display: String,
        score: f32,
        because: Vec<(String, f32)>,
        collaborative: bool,
    }

    let mut cands: HashMap<String, CandAcc> = HashMap::new();
    let mut voter_set: HashSet<String> = HashSet::new();
    let mut max_total_voters: u32 = 0;
    let mut collab_hits = 0usize;

    for (idx, (_, seed_display, seed_w)) in query_seeds.iter().enumerate() {
        let Some(node) = &seed_nodes[idx] else { continue };
        max_total_voters = max_total_voters.max(node.total_voters);
        for v in &node.supporters {
            voter_set.insert(v.clone());
        }
        // Only treat an edge as meaningful collaborative signal when at least
        // one distinct listener bridges this seed to another artist.
        let deg_norm = (node.total_voters.max(1) as f32).sqrt();
        for (cand_k, edge) in &node.neighbors {
            if excluded.contains(cand_k) || edge.weight <= 0.0 {
                continue;
            }
            // Lift-damped cosine proxy: co-occurring weight normalized by seed popularity.
            let co_bonus = (1.0 + edge.co_voters as f32).ln();
            let contrib = seed_w * (edge.weight / deg_norm) * (0.65 + 0.35 * co_bonus);
            collab_hits += 1;
            let acc = cands.entry(cand_k.clone()).or_insert_with(|| CandAcc {
                display: edge.display.clone(),
                score: 0.0,
                because: Vec::new(),
                collaborative: true,
            });
            acc.score += contrib;
            acc.collaborative = true;
            acc.because.push((seed_display.clone(), contrib));
        }
    }

    // 3. Cold-start / sparse-graph fallback: blend related artists from cached
    // tag metadata for the user's top 5 seeds so fresh servers still deliver
    // strong recommendations.
    if cands.len() < limit {
        let seed_tags = futures::future::join_all(
            query_seeds.iter().take(5).map(|(_, disp, _)| lookup_artist_tags(svc, disp)),
        )
        .await;
        for (idx, info) in seed_tags.into_iter().enumerate() {
            let (_, seed_display, seed_w) = query_seeds[idx];
            for (rank, sim_name) in info.similar.into_iter().take(10).enumerate() {
                let cand_k = canonical_artist_key(&sim_name);
                if cand_k.is_empty() || excluded.contains(&cand_k) {
                    continue;
                }
                let contrib = seed_w * (0.35 - rank as f32 * 0.025).max(0.08);
                let acc = cands.entry(cand_k).or_insert_with(|| CandAcc {
                    display: sim_name,
                    score: 0.0,
                    because: Vec::new(),
                    collaborative: false,
                });
                acc.score += contrib;
                acc.because.push((seed_display.clone(), contrib));
            }
        }
    }

    // 4. Rank, enrich top candidates with cached genres, and suppress avoided tags.
    let mut ranked: Vec<(String, CandAcc)> = cands.into_iter().collect();
    ranked.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate((limit * 2).min(40));

    let cand_tags = futures::future::join_all(ranked.iter().map(|(k, _)| {
        let key = format!("tastetag:v1:{k}");
        async move { cache::get_json::<ArtistTags>(&*svc.cache, &key).await }
    }))
    .await;

    let mut recs: Vec<CollabArtistRec> = Vec::with_capacity(limit);
    for ((_, mut acc), cached_tags) in ranked.into_iter().zip(cand_tags) {
        let genres = cached_tags.as_ref().map(|t| t.genres.clone()).unwrap_or_default();
        if !avoided_tags.is_empty() {
            if let Some(ref ct) = cached_tags {
                let bad_hits = ct
                    .genres
                    .iter()
                    .chain(&ct.moods)
                    .filter(|t| avoided_tags.contains(&t.to_lowercase()))
                    .count();
                if bad_hits > 0 {
                    acc.score *= 0.25f32.powi(bad_hits as i32);
                }
            }
        }
        if acc.score <= 0.02 {
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
            score: acc.score,
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

    // Add curated diverse artists first (paired with chart photo / sample track if available).
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

    // Add up to 12 trending chart artists not already in the curated list.
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
    use glyph_protocol::taste::TasteArtistSignal;

    #[test]
    fn tags_batch_merges_lastfm_and_deezer_and_caches() {
        let h = harness(all_secrets());
        h.http.on(
            "ws.audioscrobbler.com",
            200,
            r#"{"toptags":{"tag":[{"name":"psychedelic rock","count":100},{"name":"seen live","count":90},{"name":"dream pop","count":60}]},"similarartists":{"artist":[{"name":"Pond"},{"name":"MGMT"}]}}"#,
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
        assert!(item.similar.contains(&"Pond".to_string()));
        assert!(item.similar.contains(&"Unknown Mortal Orchestra".to_string()));
    }

    #[test]
    fn collaborative_filtering_learns_cooccurrence_and_respects_negatives() {
        let h = harness(Default::default());
        // User A listens to Tame Impala, Men I Trust, and Crumb.
        let _ = block_on(collab(
            &h.svc,
            Some("acc:user_a"),
            TasteCollabRequest {
                positive: vec![
                    TasteArtistSignal { artist: "Tame Impala".into(), weight: 0.9 },
                    TasteArtistSignal { artist: "Men I Trust".into(), weight: 0.85 },
                    TasteArtistSignal { artist: "Crumb".into(), weight: 0.8 },
                ],
                negative: vec![],
                avoided_tags: vec![],
                limit: Some(10),
                contribute: true,
            },
        ))
        .unwrap();

        // User B also listens to Tame Impala, Men I Trust, and Khruangbin.
        let _ = block_on(collab(
            &h.svc,
            Some("acc:user_b"),
            TasteCollabRequest {
                positive: vec![
                    TasteArtistSignal { artist: "Tame Impala".into(), weight: 1.0 },
                    TasteArtistSignal { artist: "Men I Trust".into(), weight: 0.7 },
                    TasteArtistSignal { artist: "Khruangbin".into(), weight: 0.9 },
                ],
                negative: vec![],
                avoided_tags: vec![],
                limit: Some(10),
                contribute: true,
            },
        ))
        .unwrap();

        // User C likes Tame Impala & Men I Trust, hates Khruangbin -> should get Crumb!
        let rec = block_on(collab(
            &h.svc,
            Some("acc:user_c"),
            TasteCollabRequest {
                positive: vec![
                    TasteArtistSignal { artist: "Tame Impala".into(), weight: 0.9 },
                    TasteArtistSignal { artist: "Men I Trust".into(), weight: 0.8 },
                ],
                negative: vec!["Khruangbin".into()],
                avoided_tags: vec![],
                limit: Some(10),
                contribute: false,
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
            positive: vec![
                TasteArtistSignal { artist: "Artist A".into(), weight: 1.0 },
                TasteArtistSignal { artist: "Artist B".into(), weight: 1.0 },
            ],
            contribute: true,
            ..Default::default()
        };
        let _ = block_on(collab(&h.svc, Some("acc:spammer"), req.clone())).unwrap();
        // Advance past cooldown and retry with same voter.
        h.rt.advance(2 * HOUR * 1000);
        let _ = block_on(collab(&h.svc, Some("acc:spammer"), req)).unwrap();

        let node: CoOccurNode =
            block_on(cache::get_json(&*h.svc.cache, &cooccur_key("artist a"))).unwrap();
        assert_eq!(node.total_voters, 1);
        assert_eq!(node.neighbors["artist b"].co_voters, 1);

        // When the same voter later discovers Artist C, Artist A learns the new A -> C edge
        // without double-counting A.total_voters or the existing A -> B edge.
        h.rt.advance(2 * HOUR * 1000);
        let evolved = TasteCollabRequest {
            positive: vec![
                TasteArtistSignal { artist: "Artist A".into(), weight: 1.0 },
                TasteArtistSignal { artist: "Artist B".into(), weight: 1.0 },
                TasteArtistSignal { artist: "Artist C".into(), weight: 0.9 },
            ],
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
    fn collaborative_filtering_handles_empty_and_non_finite_weights() {
        let h = harness(Default::default());
        let empty = block_on(collab(&h.svc, None, TasteCollabRequest::default())).unwrap();
        assert!(empty.recommendations.is_empty());
        assert!(!empty.model_ready);

        let bad = block_on(collab(
            &h.svc,
            None,
            TasteCollabRequest {
                positive: vec![TasteArtistSignal { artist: "A".into(), weight: f32::NAN }],
                ..Default::default()
            },
        ))
        .unwrap();
        assert!(bad.recommendations.is_empty());
    }

    #[test]
    fn onboarding_catalog_returns_diverse_starters_without_network() {
        let h = harness(Default::default());
        let cat = block_on(onboarding_catalog(&h.svc)).unwrap();
        assert!(cat.artists.len() >= 20);
        assert!(cat.genres.len() >= 8);
    }

    #[test]
    fn http_routes_advertise_capability_and_roundtrip_taste_endpoints() {
        use crate::{Method, Request, Server};
        let h = harness(Default::default());
        let srv = Server::from_arc(h.svc.clone());

        // /v1/health advertises capability::TASTE
        let health_resp = block_on(srv.handle(Request::new(Method::Get, "/v1/health", vec![], vec![], Some("127.0.0.1".into()))));
        assert_eq!(health_resp.status, 200);
        let health: glyph_protocol::HealthResponse = serde_json::from_slice(health_resp.bytes().unwrap()).unwrap();
        assert!(health.supports(glyph_protocol::capability::TASTE));

        // GET /v1/taste/onboarding
        let onb_resp = block_on(srv.handle(Request::new(Method::Get, "/v1/taste/onboarding", vec![], vec![], Some("127.0.0.1".into()))));
        assert_eq!(onb_resp.status, 200);
        let onb: OnboardingCatalogResponse = serde_json::from_slice(onb_resp.bytes().unwrap()).unwrap();
        assert!(!onb.artists.is_empty());

        // Wrong method on /v1/taste/collab -> 405
        let wrong_method = block_on(srv.handle(Request::new(Method::Get, "/v1/taste/collab", vec![], vec![], Some("127.0.0.1".into()))));
        assert_eq!(wrong_method.status, 405);

        // POST /v1/taste/tags with empty list -> 200 empty items
        let tags_resp = block_on(srv.handle(Request::new(
            Method::Post,
            "/v1/taste/tags",
            vec![("content-type".into(), "application/json".into())],
            br#"{"artists":[]}"#.to_vec(),
            Some("127.0.0.1".into()),
        )));
        assert_eq!(tags_resp.status, 200);
        let tags: TasteTagsResponse = serde_json::from_slice(tags_resp.bytes().unwrap()).unwrap();
        assert!(tags.items.is_empty());

        // POST /v1/taste/collab ingest + query over HTTP
        let body_a = serde_json::to_vec(&TasteCollabRequest {
            positive: vec![
                TasteArtistSignal { artist: "Daft Punk".into(), weight: 0.9 },
                TasteArtistSignal { artist: "Justice".into(), weight: 0.85 },
            ],
            contribute: true,
            ..Default::default()
        })
        .unwrap();
        let r1 = block_on(srv.handle(Request::new(
            Method::Post,
            "/v1/taste/collab",
            vec![("content-type".into(), "application/json".into()), ("x-glyph-install".into(), "inst-1".into())],
            body_a,
            Some("10.0.0.1".into()),
        )));
        assert_eq!(r1.status, 200);

        let body_b = serde_json::to_vec(&TasteCollabRequest {
            positive: vec![TasteArtistSignal { artist: "Daft Punk".into(), weight: 1.0 }],
            contribute: false,
            ..Default::default()
        })
        .unwrap();
        let r2 = block_on(srv.handle(Request::new(
            Method::Post,
            "/v1/taste/collab",
            vec![("content-type".into(), "application/json".into()), ("x-glyph-install".into(), "inst-2".into())],
            body_b,
            Some("10.0.0.2".into()),
        )));
        assert_eq!(r2.status, 200);
        let rec: TasteCollabResponse = serde_json::from_slice(r2.bytes().unwrap()).unwrap();
        assert!(rec.model_ready);
        assert!(rec.recommendations.iter().any(|r| r.artist == "Justice"));
    }
}
