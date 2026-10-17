//! Shared YouTube video ids ("hints").
//!
//! Desktop clients resolve YouTube with their own yt-dlp at home (the server
//! cannot: datacenter IPs get YouTube's bot check). After a sure match they
//! report "song -> video id" here; `/v1/resolve` hands the id to the next
//! client, which then skips its own (slow) yt-dlp search. Only ids are
//! stored, never audio.
//!
//! A report is trusted when the YouTube Data API (if a key is configured) or
//! the server's own yt-dlp confirms the video exists and its title / channel /
//! duration fit the song, or else once [`MIN_VOTES`] different installs
//! reported the same video. With yt-dlp the first sure report is shared at once.

use crate::cache::{self, DAY};
use crate::error::ApiErr;
use crate::merge::{artists_match, normalize_artist, normalize_title, plain_words};
use crate::providers::{build_url, fetch_json, ProviderError};
use crate::traits::{HttpRequest, RateDecision, Services};
use glyph_protocol::{Track, YouTubeHintRequest, YouTubeHintResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Different installs that must agree when the Data API cannot check.
pub const MIN_VOTES: usize = 2;
/// Hints live long; every vote is one cache (KV) write.
const TTL_S: u64 = 60 * DAY;
const MAX_VOTES: usize = 12;
/// YouTube uploads often carry a few seconds of intro / outro.
const DURATION_TOLERANCE_MS: u64 = 6_000;
/// Reports per install (or IP) per hour.
const REPORTS_PER_HOUR: u32 = 60;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    confirmed: Option<String>,
    #[serde(default)]
    votes: Vec<Vote>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Vote {
    video: String,
    /// Hashed install id / IP (never stored in clear).
    voter: String,
}

/// `true` for a YouTube video id: exactly 11 of `[A-Za-z0-9_-]`.
pub fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Cache key of a song: normalised artist + title (the same for every
/// client, whatever source the song came from).
pub fn song_key(t: &Track) -> Option<String> {
    let artist = plain_words(&normalize_artist(&t.artist));
    let title = plain_words(&normalize_title(&t.title));
    (!artist.is_empty() && !title.is_empty()).then(|| format!("ythint:{artist}:{title}"))
}

/// The shared video id for `t`, if one is confirmed.
pub async fn lookup(svc: &Services, t: &Track) -> Option<String> {
    let key = song_key(t)?;
    cache::get_json::<Entry>(&*svc.cache, &key).await?.confirmed
}

fn voter_hash(voter: &str) -> String {
    Sha256::digest(voter.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// `true` if `hay` contains every word of `needle` in order.
fn contains_words(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && format!(" {hay} ").contains(&format!(" {needle} "))
}

/// Does the video (title, channel, duration) fit the wanted song?
pub fn video_fits(want: &Track, video_title: &str, channel: &str, video_ms: Option<u64>) -> bool {
    let want_title = plain_words(&normalize_title(&want.title));
    let have_title = plain_words(&normalize_title(video_title));
    if !contains_words(&have_title, &want_title) {
        return false;
    }
    let channel = channel.trim_end_matches(" - Topic").trim_end_matches("VEVO").trim();
    let artist_ok = artists_match(&want.artist, channel)
        || contains_words(&plain_words(video_title), &plain_words(&normalize_artist(&want.artist)));
    let duration_ok = match (want.duration_ms, video_ms) {
        (Some(a), Some(b)) => a.abs_diff(b) <= DURATION_TOLERANCE_MS,
        _ => true,
    };
    artist_ok && duration_ok
}

/// Checks the video with the Data API (1 quota unit). `None` = no key or the
/// API failed; then the vote decides.
async fn verify(svc: &Services, want: &Track, video_id: &str) -> Option<bool> {
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
    let Some(key) = svc.config.secrets.youtube_api_key.as_deref() else {
        return verify_ytdlp(svc, want, video_id).await;
    };
    let url = build_url(
        "https://www.googleapis.com/youtube/v3/videos",
        &[("part", "snippet,contentDetails"), ("id", video_id), ("key", key)],
    );
    let r: Result<Resp, ProviderError> = fetch_json(svc, HttpRequest::get(url)).await;
    let r = r.ok()?;
    let Some(item) = r.items.into_iter().next() else { return Some(false) };
    let ms = item.details.and_then(|d| d.duration).and_then(|d| crate::providers::youtube::parse_iso8601_duration(&d));
    Some(video_fits(want, &item.snippet.title, &item.snippet.channel, ms))
}

/// Same check through yt-dlp (metadata only). `None` = no yt-dlp or it failed.
async fn verify_ytdlp(svc: &Services, want: &Track, video_id: &str) -> Option<bool> {
    if !svc.config.provider_enabled(glyph_protocol::ProviderId::YouTube) {
        return None;
    }
    // No metadata (bot check, empty output): the votes decide.
    let (title, channel, ms) = crate::providers::youtube::video_info(svc, video_id).await.ok()??;
    Some(video_fits(want, &title, &channel, ms))
}

/// Records one report. Writes the cache only when something changed.
pub async fn report(svc: &Services, voter: &str, req: YouTubeHintRequest) -> Result<YouTubeHintResponse, ApiErr> {
    if !valid_video_id(&req.video_id) {
        return Err(ApiErr::bad_request("video_id must be an 11-character YouTube id"));
    }
    let key = song_key(&req.track).ok_or_else(|| ApiErr::bad_request("track.title and track.artist are required"))?;
    let voter = voter_hash(voter);
    if let RateDecision::Limited { retry_after_s } = svc.limiter.check(&format!("hint:{voter}"), REPORTS_PER_HOUR, 3600).await {
        return Err(ApiErr::rate_limited(retry_after_s));
    }
    let mut entry: Entry = cache::get_json(&*svc.cache, &key).await.unwrap_or_default();
    if entry.confirmed.as_deref() == Some(req.video_id.as_str()) {
        return Ok(YouTubeHintResponse { accepted: true, confirmed: true });
    }
    match verify(svc, &req.track, &req.video_id).await {
        Some(false) => return Ok(YouTubeHintResponse { accepted: false, confirmed: entry.confirmed.is_some() }),
        Some(true) if entry.confirmed.is_none() => {
            entry.confirmed = Some(req.video_id.clone());
            cache::put_json(&*svc.cache, &key, &entry, TTL_S).await;
            return Ok(YouTubeHintResponse { accepted: true, confirmed: true });
        }
        // Verified, but another verified id is already shared: count a vote.
        _ => {}
    }
    let before = entry.clone();
    entry.votes.retain(|v| v.voter != voter);
    entry.votes.push(Vote { video: req.video_id.clone(), voter });
    if entry.votes.len() > MAX_VOTES {
        let extra = entry.votes.len() - MAX_VOTES;
        entry.votes.drain(..extra);
    }
    // The id with the most votes wins once it has enough of them.
    let count = |id: &str| entry.votes.iter().filter(|v| v.video == id).count();
    let mut leader: Option<(String, usize)> = None;
    for v in &entry.votes {
        let n = count(&v.video);
        if leader.as_ref().map_or(true, |(_, best)| n > *best) {
            leader = Some((v.video.clone(), n));
        }
    }
    if let Some((id, n)) = leader {
        let current = entry.confirmed.as_deref().map(count).unwrap_or(0);
        if n >= MIN_VOTES && n > current {
            entry.confirmed = Some(id);
        }
    }
    if entry != before {
        cache::put_json(&*svc.cache, &key, &entry, TTL_S).await;
    }
    Ok(YouTubeHintResponse { accepted: true, confirmed: entry.confirmed.as_deref() == Some(req.video_id.as_str()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;
    use glyph_protocol::ProviderId;

    fn song() -> Track {
        track("dz1", ProviderId::Deezer, "Believer", "Imagine Dragons", 204)
    }

    fn req(video: &str) -> YouTubeHintRequest {
        YouTubeHintRequest { track: song(), video_id: video.into() }
    }

    #[test]
    fn keys_and_ids() {
        assert!(valid_video_id("7wtfhZwyrcc"));
        assert!(!valid_video_id("7wtfhZwyrc") && !valid_video_id("--exec=abcd") && !valid_video_id("7wtfhZwyrc/"));
        let mut other = song();
        other.title = "Believer (Official Audio)".into();
        other.source = ProviderId::Spotify;
        assert_eq!(song_key(&song()), song_key(&other));
        other.title = String::new();
        assert_eq!(song_key(&other), None);
    }

    #[test]
    fn fit_checks_title_artist_duration() {
        let s = song();
        assert!(video_fits(&s, "Imagine Dragons - Believer (Official Music Video)", "ImagineDragonsVEVO", Some(217_000 - 10_000)));
        assert!(video_fits(&s, "Believer", "Imagine Dragons - Topic", Some(204_000)));
        assert!(!video_fits(&s, "Thunder", "Imagine Dragons - Topic", Some(204_000)));
        assert!(!video_fits(&s, "Believer", "Some Cover Channel", Some(204_000)));
        assert!(!video_fits(&s, "Believer", "Imagine Dragons - Topic", Some(290_000)));
    }

    #[test]
    fn votes_confirm_without_api_key() {
        let h = harness(Default::default());
        block_on(async {
            let r = report(&h.svc, "install-a", req("7wtfhZwyrcc")).await.unwrap();
            assert!(r.accepted && !r.confirmed);
            // The same install again does not count twice.
            assert!(!report(&h.svc, "install-a", req("7wtfhZwyrcc")).await.unwrap().confirmed);
            assert_eq!(lookup(&h.svc, &song()).await, None);
            let r = report(&h.svc, "install-b", req("7wtfhZwyrcc")).await.unwrap();
            assert!(r.confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("7wtfhZwyrcc"));
            // One dissenting vote does not flip it.
            assert!(!report(&h.svc, "install-c", req("IhP3J0j9JmY")).await.unwrap().confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("7wtfhZwyrcc"));
        });
        assert_eq!(h.http.call_count("googleapis"), 0);
    }

    #[test]
    fn api_key_verifies_or_rejects() {
        let h = harness(all_secrets());
        h.http.on(
            "id=7wtfhZwyrcc",
            200,
            r#"{"items":[{"snippet":{"title":"Believer","channelTitle":"Imagine Dragons - Topic"},"contentDetails":{"duration":"PT3M24S"}}]}"#,
        );
        h.http.on(
            "id=xxxxxxxxxxx",
            200,
            r#"{"items":[{"snippet":{"title":"Never Gonna Give You Up","channelTitle":"Rick Astley"},"contentDetails":{"duration":"PT3M33S"}}]}"#,
        );
        block_on(async {
            let bad = report(&h.svc, "a", req("xxxxxxxxxxx")).await.unwrap();
            assert!(!bad.accepted);
            assert_eq!(lookup(&h.svc, &song()).await, None);
            let ok = report(&h.svc, "a", req("7wtfhZwyrcc")).await.unwrap();
            assert!(ok.accepted && ok.confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("7wtfhZwyrcc"));
        });
    }

    #[test]
    fn ytdlp_verifies_without_api_key() {
        let h = harness_with_ytdlp(Default::default());
        h.cmds.on(
            "watch?v=7wtfhZwyrcc",
            r#"{"id":"7wtfhZwyrcc","title":"Believer","channel":"Imagine Dragons - Topic","duration":204}"#,
        );
        h.cmds.on("watch?v=xxxxxxxxxxx", r#"{"id":"xxxxxxxxxxx","title":"Never Gonna Give You Up","channel":"Rick Astley","duration":213}"#);
        block_on(async {
            assert!(!report(&h.svc, "a", req("xxxxxxxxxxx")).await.unwrap().accepted);
            let ok = report(&h.svc, "a", req("7wtfhZwyrcc")).await.unwrap();
            assert!(ok.accepted && ok.confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("7wtfhZwyrcc"));
        });
    }

    #[test]
    fn rejects_bad_input() {
        let h = harness(Default::default());
        block_on(async {
            assert_eq!(report(&h.svc, "a", req("nope")).await.unwrap_err().status, 400);
            let mut r = req("7wtfhZwyrcc");
            r.track.artist.clear();
            assert_eq!(report(&h.svc, "a", r).await.unwrap_err().status, 400);
        });
    }
}
