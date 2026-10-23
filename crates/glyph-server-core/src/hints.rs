//! Shared YouTube video ids ("hints").
//!
//! Desktop clients resolve YouTube with their own yt-dlp at home (the server
//! cannot: datacenter IPs get YouTube's bot check). After a sure match they
//! report "song -> video id" here; `/v1/resolve` hands the id to the next
//! client, which then skips its own (slow) yt-dlp search. Only ids are
//! stored, never audio.
//!
//! A report is a claim. The server looks the song up in its own catalog
//! (Deezer, by ISRC or text search) and checks the video against that entry,
//! not against the client's fields: the YouTube Data API (if a key is
//! configured) or the server's own yt-dlp must show the title, the artist's
//! channel and a length within six seconds. A song the catalog does not know
//! gets no shared id. The first verified video is kept, except that an
//! artist's "- Topic" upload replaces any other one.

use crate::cache::{self, DAY};
use crate::error::ApiErr;
use crate::merge::{artists_match, normalize_artist, normalize_title, plain_words};
use crate::providers::{build_url, deezer, fetch_json, ProviderError};
use crate::resolver;
use crate::search::search_provider;
use crate::traits::{with_timeout, HttpRequest, RateDecision, Services};
use crate::trust::{self, Claimed, Verified};
use glyph_protocol::{ProviderId, Track, YouTubeHintRequest, YouTubeHintResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hints live long; every confirmation is one cache (KV) write.
const TTL_S: u64 = 60 * DAY;
/// YouTube uploads often carry a few seconds of intro / outro.
const DURATION_TOLERANCE_MS: u64 = 6_000;
/// Reports per install (or IP) per hour.
const REPORTS_PER_HOUR: u32 = 60;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    confirmed: Option<String>,
    /// The confirmed video is on the artist's "- Topic" channel.
    #[serde(default)]
    topic: bool,
}

/// Cache key of a song: normalised artist + title (the same for every
/// client, whatever source the song came from).
pub fn song_key(t: &Track) -> Option<String> {
    let artist = plain_words(&normalize_artist(&t.artist));
    let title = plain_words(&normalize_title(&t.title));
    // v3: v2 entries were checked against client-sent lengths and titles.
    (!artist.is_empty() && !title.is_empty()).then(|| format!("ythint:verified-v3:{artist}:{title}"))
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

/// The channel belongs to `artist` ("Imagine Dragons", "ImagineDragonsVEVO",
/// "Imagine Dragons - Topic").
fn channel_is_artist(artist: &str, channel: &str) -> bool {
    let channel = channel.trim_end_matches(" - Topic").trim_end_matches("VEVO").trim();
    let compact = |s: &str| plain_words(&normalize_artist(s)).replace(' ', "");
    artists_match(artist, channel) || (!compact(artist).is_empty() && compact(artist) == compact(channel))
}

/// Does the video (title, channel, length) fit the catalog's song? The
/// artist must own the channel: anyone can put an artist's name into a
/// video title. Both lengths must be known.
pub fn video_fits(want: &Track, video_title: &str, channel: &str, video_ms: Option<u64>) -> bool {
    let want_title = plain_words(&normalize_title(&want.title));
    let have_title = plain_words(&normalize_title(video_title));
    let duration_ok = match (want.duration_ms, video_ms) {
        (Some(a), Some(b)) => a.abs_diff(b) <= DURATION_TOLERANCE_MS,
        _ => false,
    };
    contains_words(&have_title, &want_title) && channel_is_artist(&want.artist, channel) && duration_ok
}

/// The song as the server's own catalog (Deezer) knows it: by ISRC when the
/// claim carries one, else the best text match. `None` when the catalog has
/// no such song with a known length.
async fn canonical(svc: &Services, claim: &Track) -> Option<Track> {
    if !svc.config.provider_enabled(ProviderId::Deezer) {
        return None;
    }
    let timeout = svc.config.provider_timeout_ms;
    if let Some(isrc) = claim.isrc.as_deref() {
        if let Some(Ok(Some(c))) = with_timeout(&*svc.rt, timeout, deezer::by_isrc(svc, isrc)).await {
            if c.track.duration_ms.is_some() {
                return Some(c.track);
            }
        }
    }
    let query = format!("{} {}", claim.artist, claim.title);
    let hits = with_timeout(&*svc.rt, timeout, search_provider(svc, ProviderId::Deezer, query.trim())).await?.ok()?;
    hits.into_iter()
        .filter(|c| c.track.duration_ms.is_some())
        .map(|c| (resolver::score(claim, &c), c))
        .filter(|(s, _)| *s >= resolver::ACCEPT_SCORE)
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, c)| c.track)
}

/// What a check of the video found.
enum Check {
    /// No key, no yt-dlp or the lookup failed.
    Unavailable,
    /// The video does not exist or does not fit the song.
    Mismatch,
    /// It fits; `topic`: on the artist's "- Topic" channel.
    Fits { topic: bool },
}

fn check(want: &Track, title: &str, channel: &str, ms: Option<u64>) -> Check {
    if video_fits(want, title, channel, ms) {
        Check::Fits { topic: channel.ends_with(" - Topic") }
    } else {
        Check::Mismatch
    }
}

/// Checks the video with the Data API (1 quota unit), else with yt-dlp.
async fn verify(svc: &Services, want: &Track, video_id: &str) -> Check {
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
    let Ok(r) = r else { return Check::Unavailable };
    let Some(item) = r.items.into_iter().next() else { return Check::Mismatch };
    let ms = item.details.and_then(|d| d.duration).and_then(|d| crate::providers::youtube::parse_iso8601_duration(&d));
    check(want, &item.snippet.title, &item.snippet.channel, ms)
}

/// Same check through yt-dlp (metadata only).
async fn verify_ytdlp(svc: &Services, want: &Track, video_id: &str) -> Check {
    if !svc.config.provider_enabled(ProviderId::YouTube) {
        return Check::Unavailable;
    }
    // No metadata (bot check, empty output): do not confirm client input.
    match crate::providers::youtube::video_info(svc, video_id).await {
        Ok(Some((title, channel, ms))) => check(want, &title, &channel, ms),
        _ => Check::Unavailable,
    }
}

/// Records one report. Writes the cache only when the shared id changes.
pub async fn report(svc: &Services, voter: &str, req: impl Into<Claimed<YouTubeHintRequest>>) -> Result<YouTubeHintResponse, ApiErr> {
    let claim = req.into();
    let req = claim.get();
    let key = song_key(&req.track).ok_or_else(|| ApiErr::bad_request("track.title and track.artist are required"))?;
    let voter = voter_hash(voter);
    if let RateDecision::Limited { retry_after_s } = svc.limiter.check(&format!("hint:{voter}"), REPORTS_PER_HOUR, 3600).await {
        return Err(ApiErr::rate_limited(retry_after_s));
    }
    let video = req.video_id.as_str();
    let mut entry: Entry = cache::get_json(&*svc.cache, &key).await.unwrap_or_default();
    if entry.confirmed.as_deref() == Some(video) {
        return Ok(YouTubeHintResponse { accepted: true, confirmed: true });
    }
    // The claim only finds the song; the catalog entry is what gets checked.
    // It must be the same song the key names, or the key would point a
    // different song at this video.
    let song = match canonical(svc, &req.track).await {
        Some(song) if song_key(&song).as_deref() == Some(key.as_str()) => song,
        _ => return Ok(YouTubeHintResponse { accepted: true, confirmed: false }),
    };
    match verify(svc, &song, video).await {
        Check::Unavailable => Ok(YouTubeHintResponse { accepted: true, confirmed: false }),
        Check::Mismatch => Ok(YouTubeHintResponse { accepted: false, confirmed: false }),
        Check::Fits { topic } => {
            if entry.confirmed.is_none() || (topic && !entry.topic) {
                entry = Entry { confirmed: Some(video.to_string()), topic };
                // Checked: catalog song and video metadata both come from
                // the server's own lookups above.
                trust::put_shared(&*svc.cache, &key, &Verified::checked(entry.clone()), TTL_S).await;
            }
            Ok(YouTubeHintResponse { accepted: true, confirmed: entry.confirmed.as_deref() == Some(video) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;
    use glyph_protocol::{ProviderId, VideoId};

    const DEEZER_BELIEVER: &str =
        r#"{"data":[{"id":7,"title":"Believer","duration":204,"artist":{"name":"Imagine Dragons"}}]}"#;

    fn song() -> Track {
        track("dz1", ProviderId::Deezer, "Believer", "Imagine Dragons", 204)
    }

    fn req(video: &str) -> YouTubeHintRequest {
        YouTubeHintRequest { track: song(), video_id: VideoId::parse(video).unwrap() }
    }

    #[test]
    fn keys() {
        let mut other = song();
        other.title = "Believer (Official Audio)".into();
        other.source = ProviderId::Spotify;
        assert_eq!(song_key(&song()), song_key(&other));
        other.title = String::new();
        assert_eq!(song_key(&other), None);
    }

    #[test]
    fn fit_checks_title_channel_and_length() {
        let s = song();
        assert!(video_fits(&s, "Imagine Dragons - Believer (Official Music Video)", "ImagineDragonsVEVO", Some(217_000 - 10_000)));
        assert!(video_fits(&s, "Believer", "Imagine Dragons - Topic", Some(204_000)));
        assert!(!video_fits(&s, "Thunder", "Imagine Dragons - Topic", Some(204_000)));
        assert!(!video_fits(&s, "Believer", "Some Cover Channel", Some(204_000)));
        assert!(!video_fits(&s, "Imagine Dragons - Believer", "Some Cover Channel", Some(204_000)), "artist in the title is not enough");
        assert!(!video_fits(&s, "Believer", "Imagine Dragons - Topic", Some(290_000)));
        assert!(!video_fits(&s, "Believer", "Imagine Dragons - Topic", None), "unknown length never fits");
        let mut no_length = s.clone();
        no_length.duration_ms = None;
        assert!(!video_fits(&no_length, "Believer", "Imagine Dragons - Topic", Some(204_000)));
    }

    #[test]
    fn unverifiable_reports_never_confirm() {
        let h = harness(Default::default());
        h.http.on("api.deezer.com/search", 200, DEEZER_BELIEVER);
        block_on(async {
            for i in 0..20 {
                let result = report(&h.svc, &format!("attacker-{i}"), req("7wtfhZwyrcc")).await.unwrap();
                assert!(result.accepted && !result.confirmed);
            }
            assert_eq!(lookup(&h.svc, &song()).await, None);
            // Values confirmed under the old rules are ignored.
            let old_key = song_key(&song()).unwrap().replace("verified-v3", "verified-v2");
            cache::put_json(&*h.svc.cache, &old_key, &Entry { confirmed: Some("xxxxxxxxxxx".into()), topic: false }, TTL_S).await;
            assert_eq!(lookup(&h.svc, &song()).await, None);
        });
        assert_eq!(h.http.call_count("googleapis"), 0);
    }

    fn api_harness() -> Harness {
        let h = harness(all_secrets());
        h.http.on("api.deezer.com/search", 200, DEEZER_BELIEVER);
        h.http.on(
            "id=7wtfhZwyrcc",
            200,
            r#"{"items":[{"snippet":{"title":"Believer","channelTitle":"Imagine Dragons - Topic"},"contentDetails":{"duration":"PT3M24S"}}]}"#,
        );
        h.http.on(
            "id=fanupload12",
            200,
            r#"{"items":[{"snippet":{"title":"Imagine Dragons - Believer (lyrics)","channelTitle":"Imagine Dragons"},"contentDetails":{"duration":"PT3M26S"}}]}"#,
        );
        h.http.on(
            "id=xxxxxxxxxxx",
            200,
            r#"{"items":[{"snippet":{"title":"Never Gonna Give You Up","channelTitle":"Rick Astley"},"contentDetails":{"duration":"PT3M33S"}}]}"#,
        );
        h.http.on(
            "id=squatter123",
            200,
            r#"{"items":[{"snippet":{"title":"Imagine Dragons - Believer","channelTitle":"Free Music 24"},"contentDetails":{"duration":"PT3M24S"}}]}"#,
        );
        h
    }

    #[test]
    fn api_key_verifies_against_the_catalog_song() {
        let h = api_harness();
        block_on(async {
            assert!(!report(&h.svc, "a", req("xxxxxxxxxxx")).await.unwrap().accepted);
            assert!(!report(&h.svc, "a", req("squatter123")).await.unwrap().accepted, "a foreign channel cannot claim the song");
            assert_eq!(lookup(&h.svc, &song()).await, None);
            let ok = report(&h.svc, "a", req("7wtfhZwyrcc")).await.unwrap();
            assert!(ok.accepted && ok.confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("7wtfhZwyrcc"));
        });
    }

    #[test]
    fn client_lengths_and_titles_are_not_trusted() {
        let h = api_harness();
        block_on(async {
            // The claim drops its length and invents one; the catalog's 204 s count.
            let mut r = req("xxxxxxxxxxx");
            r.track.duration_ms = None;
            assert!(!report(&h.svc, "a", r).await.unwrap().accepted);
            // A song the catalog does not know never gets a shared id.
            let mut unknown = req("7wtfhZwyrcc");
            unknown.track.title = "Believer 2 Unreleased".into();
            let res = report(&h.svc, "a", unknown.clone()).await.unwrap();
            assert!(!res.confirmed);
            assert_eq!(lookup(&h.svc, &unknown.track).await, None);
        });
    }

    #[test]
    fn topic_upload_replaces_an_earlier_artist_upload() {
        let h = api_harness();
        block_on(async {
            assert!(report(&h.svc, "a", req("fanupload12")).await.unwrap().confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("fanupload12"));
            assert!(report(&h.svc, "b", req("7wtfhZwyrcc")).await.unwrap().confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("7wtfhZwyrcc"));
            // Once a Topic upload is in, other uploads cannot replace it.
            assert!(!report(&h.svc, "c", req("fanupload12")).await.unwrap().confirmed);
            assert_eq!(lookup(&h.svc, &song()).await.as_deref(), Some("7wtfhZwyrcc"));
        });
    }

    #[test]
    fn ytdlp_verifies_without_api_key() {
        let h = harness_with_ytdlp(Default::default());
        h.http.on("api.deezer.com/search", 200, DEEZER_BELIEVER);
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
            let mut r = req("7wtfhZwyrcc");
            r.track.artist.clear();
            assert_eq!(report(&h.svc, "a", r).await.unwrap_err().status, 400);
        });
        assert!(serde_json::from_str::<YouTubeHintRequest>(
            r#"{"track":{"id":"1","source":"deezer","title":"a","artist":"b"},"video_id":"nope"}"#
        ).is_err());
    }
}
