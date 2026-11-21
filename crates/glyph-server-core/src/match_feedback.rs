//! Wrong-song feedback, alternative candidate lookup, and consensus promotion.
//!
//! When a listener clicks "Not the right one" on a song:
//! 1. `/v1/match/alternatives` searches enabled full-playback sources and
//!    returns ranked replacement candidates with 30-second preview URLs.
//! 2. `/v1/match/report` stores the listener's vote (keyed by a hashed voter
//!    identity) without changing the global mapping for everyone else.
//! 3. Only once enough distinct listeners agree on the same replacement
//!    ([`PROMOTE_MIN_VOTES`] and at least [`PROMOTE_MIN_SHARE`] of active votes)
//!    AND the server independently verifies the candidate against its own
//!    provider lookup does the system promote the replacement globally.

use crate::cache::{self, DAY};
use crate::error::ApiErr;
use crate::merge::{artists_match, normalize_artist, normalize_title, plain_words, word_similarity};
use crate::providers::{self, build_url, fetch_json, Candidate, ProviderError};
use crate::resolver;
use crate::search::{is_skipped, search_provider};
use crate::traits::{with_timeout, HttpRequest, RateDecision, Services};
use crate::trust::{self, Claimed, Verified};
use futures::stream::{FuturesUnordered, StreamExt};
use glyph_protocol::{
    MatchAlternative, MatchAlternativesRequest, MatchAlternativesResponse, PlaybackSource,
    ProviderId, Track, WrongMatchReportRequest, WrongMatchReportResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// Minimum distinct voters agreeing on the exact same replacement before the
/// system promotes the change globally for all clients.
pub const PROMOTE_MIN_VOTES: u32 = 3;
/// Minimum share of active votes for this song that must agree on the winner.
pub const PROMOTE_MIN_SHARE: f32 = 0.60;
/// Stored feedback lifetime (votes + promoted override).
const TTL_S: u64 = 90 * DAY;
const VOTE_MAX_AGE_MS: u64 = 90 * DAY * 1000;
const MAX_STORED_VOTES: usize = 100;
const REPORTS_PER_HOUR: u32 = 30;
const MAX_ALTERNATIVES: usize = 15;
const ALT_TIMEOUT_MS: u64 = 6_000;
const DURATION_SLACK_MS: u64 = 35_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct VoteRecord {
    voter: String,
    chosen_provider: ProviderId,
    chosen_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrong_provider: Option<ProviderId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrong_id: Option<String>,
    ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PromotedMatch {
    track: Track,
    promoted_at_ms: u64,
    votes: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct FeedbackEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    promoted: Option<PromotedMatch>,
    #[serde(default)]
    votes: Vec<VoteRecord>,
    #[serde(default)]
    updated_at_ms: u64,
}

/// Canonical cache key for feedback on `track`.
pub fn song_key(t: &Track) -> Option<String> {
    let artist = plain_words(&normalize_artist(&t.artist));
    let title = plain_words(&normalize_title(&t.title));
    (!artist.is_empty() && !title.is_empty()).then(|| format!("matchfb:v1:{artist}:{title}"))
}

fn voter_hash(voter: &str) -> String {
    Sha256::digest(voter.as_bytes())
        .iter()
        .take(10)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Globally promoted replacement track for `t`, if consensus + verification
/// has already confirmed one.
pub async fn lookup_promoted(svc: &Services, t: &Track) -> Option<Track> {
    let key = song_key(t)?;
    cache::get_json::<FeedbackEntry>(&*svc.cache, &key)
        .await?
        .promoted
        .map(|p| p.track)
}

/// Globally promoted YouTube video id for `t`, if consensus promoted a YouTube video.
pub async fn lookup_promoted_youtube(svc: &Services, t: &Track) -> Option<String> {
    let promoted = lookup_promoted(svc, t).await?;
    (promoted.source == ProviderId::YouTube && crate::media::valid_video_id(&promoted.id))
        .then_some(promoted.id)
}

fn candidate_tag(c: &Candidate, current: Option<(ProviderId, &str)>) -> Option<String> {
    if current.is_some_and(|(p, id)| p == c.track.source && id == c.track.id) {
        return Some("current".into());
    }
    if c.track.source == ProviderId::YouTube {
        let lower = c.track.title.to_ascii_lowercase();
        if c.bonus >= 0.14 {
            return Some("topic".into());
        }
        if lower.contains("official audio") || c.bonus > 0.0 {
            return Some("official_audio".into());
        }
    }
    None
}

fn preview_url_for(svc: &Services, c: &Candidate, direct: bool) -> Option<String> {
    if svc.config.proxy_audio && !direct && crate::media::valid_id(&c.track.id) {
        return Some(crate::media::audio_path(c.track.source, &c.track.id));
    }
    match &c.playback {
        Some(PlaybackSource::Preview { url, .. } | PlaybackSource::Stream { url, .. }) => {
            Some(url.clone())
        }
        _ => None,
    }
}

/// Searches enabled full-playback sources for replacement candidates for `req.track`.
pub async fn alternatives(
    svc: &Services,
    req: impl Into<Claimed<MatchAlternativesRequest>>,
    direct: bool,
) -> Result<MatchAlternativesResponse, ApiErr> {
    let claim = req.into();
    let req = claim.get();
    let want = &req.track;
    if want.title.trim().is_empty() || want.artist.trim().is_empty() {
        return Err(ApiErr::bad_request("track.title and track.artist are required"));
    }

    // Identify what is currently mapped for this song (promoted override > YouTube hint > track's own full source).
    let current: Option<(ProviderId, String)> = if let Some(p) = lookup_promoted(svc, want).await {
        Some((p.source, p.id))
    } else if let Some(yt) = crate::hints::lookup(svc, want).await {
        Some((ProviderId::YouTube, yt))
    } else if providers::is_full_source(want.source) {
        Some((want.source, want.id.clone()))
    } else {
        resolver::DEFAULT_PRIORITY
            .iter()
            .copied()
            .find_map(|p| want.external_ids.get(&p).map(|id| (p, id.clone())))
    };

    // Determine provider order: requested full sources first, then defaults + Archive.
    let mut order: Vec<ProviderId> = Vec::new();
    for p in req
        .priority
        .iter()
        .copied()
        .chain(resolver::DEFAULT_PRIORITY.iter().copied())
        .chain(std::iter::once(ProviderId::Archive))
    {
        if providers::is_full_source(p)
            && svc.config.provider_enabled(p)
            && !order.contains(&p)
        {
            order.push(p);
        }
    }

    let custom_query = req
        .query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(200).collect::<String>());
    let base_query = custom_query
        .clone()
        .unwrap_or_else(|| format!("{} {}", want.artist, want.title));

    let has_custom_query = custom_query.is_some();
    let mut tasks = FuturesUnordered::new();
    for p in &order {
        let p = *p;
        let base_q = base_query.clone();
        tasks.push(async move {
            if is_skipped(svc, p).await {
                return Vec::new();
            }
            let mut out = Vec::new();
            if p == ProviderId::YouTube && !has_custom_query {
                let q1 = format!("{base_q} audio");
                let (r1, r2) = futures::join!(
                    with_timeout(&*svc.rt, ALT_TIMEOUT_MS, search_provider(svc, p, &q1)),
                    with_timeout(&*svc.rt, ALT_TIMEOUT_MS, search_provider(svc, p, &base_q)),
                );
                if let Some(Ok(c)) = r1 {
                    out.extend(c);
                }
                if let Some(Ok(c)) = r2 {
                    out.extend(c);
                }
            } else if let Some(Ok(c)) =
                with_timeout(&*svc.rt, ALT_TIMEOUT_MS, search_provider(svc, p, &base_q)).await
            {
                out.extend(c);
            }
            out
        });
    }

    let mut raw: Vec<Candidate> = Vec::new();
    while let Some(batch) = tasks.next().await {
        raw.extend(batch);
    }

    let mut seen: HashSet<(ProviderId, String)> = HashSet::new();
    let cur_ref = current.as_ref().map(|(p, id)| (*p, id.as_str()));
    let mut scored: Vec<(f32, usize, MatchAlternative)> = Vec::new();

    for c in raw {
        if !providers::is_full_source(c.track.source)
            || !glyph_protocol::trust::plausible_track_id(c.track.source, &c.track.id)
        {
            continue;
        }
        if !seen.insert((c.track.source, c.track.id.clone())) {
            continue;
        }
        let conf = resolver::score(want, &c);
        // When the user typed a custom refinement query, keep all plausible hits;
        // otherwise require at least minimal relevance so random noise stays out.
        if !has_custom_query && conf < 0.25 && cur_ref != Some((c.track.source, c.track.id.as_str())) {
            continue;
        }
        let prio_idx = order
            .iter()
            .position(|p| *p == c.track.source)
            .unwrap_or(order.len());
        let tag = candidate_tag(&c, cur_ref);
        let preview_url = preview_url_for(svc, &c, direct);
        scored.push((
            conf,
            prio_idx,
            MatchAlternative {
                track: c.track,
                confidence: conf,
                preview_url,
                tag,
            },
        ));
    }

    // Rank: highest confidence first, with a small preference for Topic / Official
    // Audio and provider priority on near ties.
    scored.sort_by(|a, b| {
        let a_boost = match a.2.tag.as_deref() {
            Some("topic") => 0.04,
            Some("official_audio") => 0.02,
            _ => 0.0,
        };
        let b_boost = match b.2.tag.as_deref() {
            Some("topic") => 0.04,
            Some("official_audio") => 0.02,
            _ => 0.0,
        };
        (b.0 + b_boost)
            .partial_cmp(&(a.0 + a_boost))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });

    let alternatives: Vec<MatchAlternative> = scored
        .into_iter()
        .take(MAX_ALTERNATIVES)
        .map(|(_, _, alt)| alt)
        .collect();

    Ok(MatchAlternativesResponse {
        current_source: current.as_ref().map(|(p, _)| *p),
        current_id: current.map(|(_, id)| id),
        alternatives,
    })
}

/// Plausibility guard for server-verified promotion: looser than strict
/// `hints::video_fits` (so legitimate label uploads or remasters can win by
/// consensus), while rejecting completely unrelated spam/troll videos.
pub fn candidate_plausible(
    want: &Track,
    have_title: &str,
    have_artist_or_channel: &str,
    have_duration_ms: Option<u64>,
) -> bool {
    let want_title = plain_words(&normalize_title(&want.title));
    let got_title = plain_words(&normalize_title(have_title));
    if want_title.is_empty() || got_title.is_empty() {
        return false;
    }
    let title_contains = format!(" {got_title} ").contains(&format!(" {want_title} "))
        || format!(" {want_title} ").contains(&format!(" {got_title} "));
    let title_sim = word_similarity(&want_title, &got_title);
    let title_ok = title_contains || title_sim >= 0.45;
    if !title_ok {
        return false;
    }

    let clean_channel = have_artist_or_channel
        .trim_end_matches(" - Topic")
        .trim_end_matches("VEVO")
        .trim();
    let want_artist = plain_words(&normalize_artist(&want.artist));
    let got_artist = plain_words(&normalize_artist(clean_channel));
    let artist_ok = artists_match(&want.artist, clean_channel)
        || (!want_artist.is_empty()
            && (format!(" {got_title} ").contains(&format!(" {want_artist} "))
                || word_similarity(&want_artist, &got_artist) >= 0.45));

    let duration_ok = match (want.duration_ms, have_duration_ms) {
        (Some(a), Some(b)) => a.abs_diff(b) <= DURATION_SLACK_MS,
        _ => true,
    };

    // Either artist matches with reasonable duration, or exact/strong title match
    // on a label/unofficial channel with matching duration.
    (artist_ok && duration_ok)
        || (title_contains && want.duration_ms.is_some() && have_duration_ms.is_some() && duration_ok)
}

/// Independently verifies `chosen` against the server's own provider lookup
/// before allowing a global promotion. Never trusts client-claimed metadata.
async fn verify_chosen(svc: &Services, want: &Track, chosen: &Track) -> Option<Track> {
    if !providers::is_full_source(chosen.source)
        || !glyph_protocol::trust::plausible_track_id(chosen.source, &chosen.id)
    {
        return None;
    }
    match chosen.source {
        ProviderId::YouTube => verify_youtube(svc, want, &chosen.id).await,
        ProviderId::Audius | ProviderId::Jamendo | ProviderId::SoundCloud | ProviderId::Archive => {
            if !svc.config.provider_enabled(chosen.source) {
                return None;
            }
            // Confirm the provider actually serves playback for this exact id.
            let _pb = with_timeout(
                &*svc.rt,
                svc.config.provider_timeout_ms,
                providers::playback_for(svc, chosen.source, &chosen.id),
            )
            .await?
            .ok()?;
            // Verify metadata from the server's own search rather than trusting the claim.
            let q = format!("{} {}", want.artist, want.title);
            if let Some(Ok(hits)) =
                with_timeout(&*svc.rt, svc.config.provider_timeout_ms, search_provider(svc, chosen.source, &q)).await
            {
                if let Some(hit) = hits.into_iter().find(|c| c.track.id == chosen.id) {
                    if candidate_plausible(want, &hit.track.title, &hit.track.artist, hit.track.duration_ms) {
                        return Some(hit.track);
                    }
                    return None;
                }
            }
            None
        }
        _ => None,
    }
}

async fn verify_youtube(svc: &Services, want: &Track, video_id: &str) -> Option<Track> {
    // 1. YouTube Data API (1 quota unit) if configured.
    if let Some(key) = svc.config.secrets.youtube_api_key.as_deref() {
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            items: Vec<Item>,
        }
        #[derive(Deserialize)]
        struct Item {
            snippet: Snippet,
            #[serde(default, rename = "contentDetails")]
            details: Option<Details>,
        }
        #[derive(Deserialize)]
        struct Snippet {
            #[serde(default)]
            title: String,
            #[serde(default, rename = "channelTitle")]
            channel: String,
        }
        #[derive(Deserialize)]
        struct Details {
            #[serde(default)]
            duration: Option<String>,
        }
        let url = build_url(
            "https://www.googleapis.com/youtube/v3/videos",
            &[("part", "snippet,contentDetails"), ("id", video_id), ("key", key)],
        );
        let r: Result<Resp, ProviderError> = fetch_json(svc, HttpRequest::get(url)).await;
        if let Ok(r) = r {
            let item = r.items.into_iter().next()?;
            let ms = item
                .details
                .and_then(|d| d.duration)
                .and_then(|d| crate::providers::youtube::parse_iso8601_duration(&d));
            if candidate_plausible(want, &item.snippet.title, &item.snippet.channel, ms) {
                let mut t = providers::new_track(
                    video_id,
                    ProviderId::YouTube,
                    &item.snippet.title,
                    item.snippet.channel.trim_end_matches(" - Topic"),
                );
                t.duration_ms = ms;
                t.permalink = Some(format!("https://www.youtube.com/watch?v={video_id}"));
                return Some(t);
            }
            return None;
        }
    }
    // 2. Server yt-dlp metadata lookup if available.
    if let Ok(Some((title, channel, ms))) = crate::providers::youtube::video_info(svc, video_id).await {
        if candidate_plausible(want, &title, &channel, ms) {
            let mut t = providers::new_track(
                video_id,
                ProviderId::YouTube,
                &title,
                channel.trim_end_matches(" - Topic"),
            );
            t.duration_ms = ms;
            t.permalink = Some(format!("https://www.youtube.com/watch?v={video_id}"));
            return Some(t);
        }
        return None;
    }
    // 3. YouTube native web search lookup by video id.
    if let Ok(hits) = crate::providers::youtube::search(svc, video_id).await {
        if let Some(hit) = hits.into_iter().find(|c| c.track.id == video_id) {
            if candidate_plausible(want, &hit.track.title, &hit.track.artist, hit.track.duration_ms) {
                return Some(hit.track);
            }
            return None;
        }
    }
    None
}

/// Records one listener's wrong-song selection.
///
/// The vote is stored persistently, but the global match is NOT changed on a
/// single report. Only when at least [`PROMOTE_MIN_VOTES`] distinct voters
/// agree (with >= [`PROMOTE_MIN_SHARE`] of active votes) AND the server
/// independently verifies the chosen track is it promoted for all clients.
pub async fn report(
    svc: &Services,
    voter: &str,
    req: impl Into<Claimed<WrongMatchReportRequest>>,
) -> Result<WrongMatchReportResponse, ApiErr> {
    let claim = req.into();
    let req = claim.get();
    let key = song_key(&req.track)
        .ok_or_else(|| ApiErr::bad_request("track.title and track.artist are required"))?;
    if !providers::is_full_source(req.chosen.source) {
        return Err(ApiErr::bad_request("chosen.source must be a full-playback provider"));
    }
    if !glyph_protocol::trust::plausible_track_id(req.chosen.source, &req.chosen.id) {
        return Err(ApiErr::bad_request("chosen.id is not valid for chosen.source"));
    }

    let voter = voter_hash(voter);
    if let RateDecision::Limited { retry_after_s } = svc
        .limiter
        .check(&format!("matchfb:{voter}"), REPORTS_PER_HOUR, 3600)
        .await
    {
        return Err(ApiErr::rate_limited(retry_after_s));
    }

    let now = svc.rt.now_ms();
    let mut entry: FeedbackEntry = cache::get_json(&*svc.cache, &key).await.unwrap_or_default();

    // Drop stale votes and replace any earlier vote by this same voter.
    entry
        .votes
        .retain(|v| now.saturating_sub(v.ts_ms) <= VOTE_MAX_AGE_MS && v.voter != voter);
    entry.votes.push(VoteRecord {
        voter,
        chosen_provider: req.chosen.source,
        chosen_id: req.chosen.id.clone(),
        wrong_provider: req.wrong_provider,
        wrong_id: req.wrong_id.clone(),
        ts_ms: now,
    });
    if entry.votes.len() > MAX_STORED_VOTES {
        let drop_n = entry.votes.len() - MAX_STORED_VOTES;
        entry.votes.drain(0..drop_n);
    }
    entry.updated_at_ms = now;

    let votes_for_chosen = entry
        .votes
        .iter()
        .filter(|v| v.chosen_provider == req.chosen.source && v.chosen_id == req.chosen.id)
        .count() as u32;
    let total_votes = entry.votes.len().max(1) as u32;
    let share = votes_for_chosen as f32 / total_votes as f32;

    let mut promoted = entry.promoted.as_ref().is_some_and(|p| {
        p.track.source == req.chosen.source && p.track.id == req.chosen.id
    });

    if !promoted && votes_for_chosen >= PROMOTE_MIN_VOTES && share >= PROMOTE_MIN_SHARE {
        if let Some(verified_track) = verify_chosen(svc, &req.track, &req.chosen).await {
            entry.promoted = Some(PromotedMatch {
                track: verified_track,
                promoted_at_ms: now,
                votes: votes_for_chosen,
            });
            promoted = true;
        }
    }

    // Checked: `entry.promoted` is only populated from `verify_chosen`, which
    // fetches metadata directly from the server's own provider lookup.
    trust::put_shared(&*svc.cache, &key, &Verified::checked(entry), TTL_S).await;

    Ok(WrongMatchReportResponse {
        recorded: true,
        promoted,
        votes: votes_for_chosen,
        threshold: PROMOTE_MIN_VOTES,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    fn wanted_song() -> Track {
        track("dz1", ProviderId::Deezer, "Believer", "Imagine Dragons", 204)
    }

    fn yt_track(id: &str, title: &str, artist: &str, secs: u64) -> Track {
        track(id, ProviderId::YouTube, title, artist, secs)
    }

    fn api_harness() -> Harness {
        let h = harness(all_secrets());
        h.http.on(
            "id=7wtfhZwyrcc",
            200,
            r#"{"items":[{"snippet":{"title":"Believer","channelTitle":"Imagine Dragons - Topic"},"contentDetails":{"duration":"PT3M24S"}}]}"#,
        );
        h.http.on(
            "id=altversion1",
            200,
            r#"{"items":[{"snippet":{"title":"Imagine Dragons - Believer (Official Audio)","channelTitle":"Interscope Records"},"contentDetails":{"duration":"PT3M26S"}}]}"#,
        );
        h.http.on(
            "id=rickroll123",
            200,
            r#"{"items":[{"snippet":{"title":"Never Gonna Give You Up","channelTitle":"Rick Astley"},"contentDetails":{"duration":"PT3M33S"}}]}"#,
        );
        h
    }

    #[test]
    fn single_vote_is_saved_but_never_switches_globally() {
        let h = api_harness();
        let req = WrongMatchReportRequest {
            track: wanted_song(),
            wrong_provider: Some(ProviderId::YouTube),
            wrong_id: Some("badmatch123".into()),
            chosen: yt_track("7wtfhZwyrcc", "Believer", "Imagine Dragons", 204),
        };
        block_on(async {
            let r1 = report(&h.svc, "acc:user1", req.clone()).await.unwrap();
            assert!(r1.recorded);
            assert!(!r1.promoted);
            assert_eq!(r1.votes, 1);
            assert_eq!(r1.threshold, PROMOTE_MIN_VOTES);
            assert_eq!(lookup_promoted(&h.svc, &wanted_song()).await, None);

            // Same voter repeating does NOT inflate votes.
            let r1_dup = report(&h.svc, "acc:user1", req.clone()).await.unwrap();
            assert!(!r1_dup.promoted);
            assert_eq!(r1_dup.votes, 1);

            // Second distinct voter: still stored, still not promoted.
            let r2 = report(&h.svc, "acc:user2", req.clone()).await.unwrap();
            assert!(!r2.promoted);
            assert_eq!(r2.votes, 2);
            assert_eq!(lookup_promoted(&h.svc, &wanted_song()).await, None);

            // Third distinct voter: consensus + server verification promotes globally!
            let r3 = report(&h.svc, "acc:user3", req).await.unwrap();
            assert!(r3.recorded);
            assert!(r3.promoted);
            assert_eq!(r3.votes, 3);
            let promoted = lookup_promoted(&h.svc, &wanted_song()).await.unwrap();
            assert_eq!(promoted.source, ProviderId::YouTube);
            assert_eq!(promoted.id, "7wtfhZwyrcc");
        });
    }

    #[test]
    fn split_votes_do_not_promote_until_majority_reached() {
        let h = api_harness();
        let vote_a = WrongMatchReportRequest {
            track: wanted_song(),
            wrong_provider: None,
            wrong_id: None,
            chosen: yt_track("7wtfhZwyrcc", "Believer", "Imagine Dragons", 204),
        };
        let vote_b = WrongMatchReportRequest {
            track: wanted_song(),
            wrong_provider: None,
            wrong_id: None,
            chosen: yt_track("altversion1", "Believer (Official Audio)", "Imagine Dragons", 206),
        };
        block_on(async {
            for u in ["u1", "u2"] {
                assert!(!report(&h.svc, u, vote_a.clone()).await.unwrap().promoted);
            }
            for u in ["u3", "u4"] {
                assert!(!report(&h.svc, u, vote_b.clone()).await.unwrap().promoted);
            }
            assert_eq!(lookup_promoted(&h.svc, &wanted_song()).await, None);
            // A 5th voter tips B to 3/5 (60%) -> now B reaches consensus and promotes.
            assert!(report(&h.svc, "u5", vote_b.clone()).await.unwrap().promoted);
            assert_eq!(lookup_promoted(&h.svc, &wanted_song()).await.unwrap().id, "altversion1");
        });
        let h2 = api_harness();
        block_on(async {
            for u in ["u1", "u2", "u3"] {
                let _ = report(&h2.svc, u, vote_a.clone()).await.unwrap();
            }
            // Now 3 votes for A -> promoted to A.
            assert_eq!(lookup_promoted(&h2.svc, &wanted_song()).await.unwrap().id, "7wtfhZwyrcc");
            // 3 votes for B gives 3/6 = 50% (< 60%), must NOT flip away from A!
            for u in ["u4", "u5", "u6"] {
                let r = report(&h2.svc, u, vote_b.clone()).await.unwrap();
                assert!(!r.promoted);
            }
            assert_eq!(lookup_promoted(&h2.svc, &wanted_song()).await.unwrap().id, "7wtfhZwyrcc");
        });
    }

    #[test]
    fn spam_candidate_never_promotes_even_with_three_votes() {
        let h = api_harness();
        let spam = WrongMatchReportRequest {
            track: wanted_song(),
            wrong_provider: None,
            wrong_id: None,
            chosen: yt_track("rickroll123", "Believer", "Imagine Dragons", 204),
        };
        block_on(async {
            for u in ["troll1", "troll2", "troll3", "troll4"] {
                let r = report(&h.svc, u, spam.clone()).await.unwrap();
                assert!(r.recorded);
                assert!(!r.promoted, "unrelated video must never be promoted globally");
            }
            assert_eq!(lookup_promoted(&h.svc, &wanted_song()).await, None);
        });
    }

    #[test]
    fn promoted_match_overrides_resolver_for_everyone() {
        let h = harness_with_ytdlp(all_secrets());
        h.http.on(
            "id=7wtfhZwyrcc",
            200,
            r#"{"items":[{"snippet":{"title":"Believer","channelTitle":"Imagine Dragons - Topic"},"contentDetails":{"duration":"PT3M24S"}}]}"#,
        );
        let req = WrongMatchReportRequest {
            track: wanted_song(),
            wrong_provider: None,
            wrong_id: None,
            chosen: yt_track("7wtfhZwyrcc", "Believer", "Imagine Dragons", 204),
        };
        block_on(async {
            for u in ["a", "b", "c"] {
                report(&h.svc, u, req.clone()).await.unwrap();
            }
            let res = resolver::resolve(
                &h.svc,
                glyph_protocol::ResolveRequest {
                    track: wanted_song(),
                    priority: vec![ProviderId::YouTube],
                    youtube_only: false,
                },
            )
            .await;
            assert_eq!(res.matched.unwrap().id, "7wtfhZwyrcc");
            assert_eq!(res.confidence, 1.0);
        });
    }

    #[test]
    fn alternatives_returns_candidates_with_preview_urls_and_tags() {
        let h = harness(all_secrets());
        let mut config = h.svc.config.clone();
        config.proxy_audio = true;
        let svc = Services {
            http: h.svc.http.clone(),
            cache: h.svc.cache.clone(),
            limiter: h.svc.limiter.clone(),
            rt: h.svc.rt.clone(),
            cmds: h.svc.cmds.clone(),
            config,
        };
        h.http.on(
            "audius.co",
            200,
            r#"{"data":[{"id":"au1","title":"Believer","duration":204,"user":{"name":"Imagine Dragons"}}]}"#,
        );
        h.http.on(
            "jamendo.com",
            200,
            r#"{"results":[{"id":"999","name":"Believer (Remix)","artist_name":"Imagine Dragons","duration":205,"audio":"https://jam/999.mp3"}]}"#,
        );
        block_on(async {
            let res = alternatives(
                &svc,
                MatchAlternativesRequest {
                    track: wanted_song(),
                    query: None,
                    priority: vec![ProviderId::Audius, ProviderId::Jamendo],
                },
                false,
            )
            .await
            .unwrap();
            assert!(!res.alternatives.is_empty());
            assert!(res
                .alternatives
                .iter()
                .all(|a| a.preview_url.as_deref().is_some_and(|u| u.starts_with("/v1/audio/"))));
        });
    }
}
