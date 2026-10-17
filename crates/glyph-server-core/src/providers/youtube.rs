//! YouTube search via the `yt-dlp` binary (no API key, no quota). We ask it
//! for a flat search first (fast, ids + titles + channels), then one follow-up
//! call for the durations. Playback is only ever the official IFrame embed
//! (`embed_id` = video id), so nothing is downloaded.

use super::*;
use crate::traits::{with_timeout, CommandError};

/// Results per search, like the old `search.list?maxResults=10`.
const MAX_RESULTS: usize = 10;
/// A flat search is fast; the detail call extracts ten pages.
const FLAT_TIMEOUT_MS: u64 = 8_000;
const DETAIL_TIMEOUT_MS: u64 = 12_000;

/// One `--dump-json` line, as lenient as the tool's output gets.
#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    uploader: String,
    #[serde(default)]
    thumbnail: Option<String>,
    /// Full dump lists every size; flat search usually has none.
    #[serde(default)]
    thumbnails: Option<Vec<Thumb>>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    availability: Option<String>,
    #[serde(default)]
    live_status: Option<String>,
}

#[derive(Deserialize)]
struct Thumb {
    url: String,
    #[serde(default)]
    width: Option<u64>,
}

/// The biggest preview we are given, or `None`.
fn best_thumb(e: &Entry) -> Option<String> {
    if let Some(t) = e.thumbnails.as_ref().filter(|v| !v.is_empty()) {
        if let Some(big) = t.iter().max_by_key(|t| t.width.unwrap_or(0)) {
            return Some(big.url.clone());
        }
    }
    e.thumbnail.clone().filter(|u| !u.is_empty())
}

pub fn playback(video_id: &str) -> PlaybackSource {
    PlaybackSource::Embed { provider: ProviderId::YouTube, embed_id: video_id.to_string() }
}

/// `PT1H2M3S` -> milliseconds (used by the playlist importer).
pub fn parse_iso8601_duration(s: &str) -> Option<u64> {
    let rest = s.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in date.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u64 = num.parse().ok()?;
            num.clear();
            total += match c {
                'D' => n * 86_400,
                'W' => n * 604_800,
                _ => return None,
            };
        }
    }
    for c in time.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let n: f64 = num.parse().ok()?;
            num.clear();
            total += match c {
                'H' => (n * 3600.0) as u64,
                'M' => (n * 60.0) as u64,
                'S' => n as u64,
                _ => return None,
            };
        }
    }
    Some(total * 1000)
}

fn map_err(e: CommandError) -> ProviderError {
    match e {
        CommandError::Unavailable => ProviderError::NotConfigured,
        CommandError::Timeout => ProviderError::Timeout,
        CommandError::Failed => ProviderError::Network,
    }
}

/// Run yt-dlp and parse its newline-delimited JSON.
async fn run(svc: &Services, bin: &str, args: &[String], timeout_ms: u64) -> PResult<Vec<Entry>> {
    let out = with_timeout(&*svc.rt, timeout_ms, svc.cmds.run(bin, args, timeout_ms))
        .await
        .ok_or(ProviderError::Timeout)?
        .map_err(map_err)?;
    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(e) = serde_json::from_str::<Entry>(line) {
            entries.push(e);
        }
    }
    Ok(entries)
}

fn base_args() -> Vec<String> {
    ["--ignore-config", "--no-warnings", "--no-playlist-reverse"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Title, channel and length of one video (`--dump-json --skip-download`,
/// nothing is downloaded). `Ok(None)` = the video is gone or private.
pub async fn video_info(svc: &Services, video_id: &str) -> PResult<Option<(String, String, Option<u64>)>> {
    let bin = svc.config.ytdlp_path.clone().ok_or(ProviderError::NotConfigured)?;
    let mut args = base_args();
    args.push("--dump-json".into());
    args.push("--skip-download".into());
    args.push("--".into());
    args.push(format!("https://www.youtube.com/watch?v={video_id}"));
    let entries = run(svc, &bin, &args, DETAIL_TIMEOUT_MS).await?;
    Ok(entries.into_iter().find(|e| e.id == video_id && !e.title.is_empty() && e.live_status.as_deref() != Some("is_live")).map(|e| {
        let channel = if e.channel.is_empty() { e.uploader } else { e.channel };
        let ms = e.duration.filter(|s| *s > 0.0).map(|s| (s * 1000.0) as u64);
        (e.title, channel, ms)
    }))
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    let bin = svc.config.ytdlp_path.clone().ok_or(ProviderError::NotConfigured)?;

    let mut args = base_args();
    args.push("--dump-json".into());
    args.push("--flat-playlist".into());
    args.push(format!("ytsearch{MAX_RESULTS}:{q}"));
    let flat = run(svc, &bin, &args, FLAT_TIMEOUT_MS).await?;

    let mut ids: Vec<String> = Vec::new();
    for e in &flat {
        if !e.id.is_empty() && !e.id.contains("://") && !ids.contains(&e.id) {
            ids.push(e.id.clone());
        }
    }

    // Durations are missing in flat mode; one call for all ids at once.
    let mut detail: Vec<Entry> = Vec::new();
    if !ids.is_empty() {
        let mut args = base_args();
        args.push("--dump-json".into());
        args.push("--skip-download".into());
        args.push("--".into());
        args.extend(ids.iter().map(|id| format!("https://www.youtube.com/watch?v={id}")));
        // Metadata is a nice-to-have: keep the hits when it fails.
        detail = run(svc, &bin, &args, DETAIL_TIMEOUT_MS).await.unwrap_or_default();
    }

    let mut out = Vec::new();
    for e in &flat {
        if e.id.is_empty() || e.title.is_empty() || !ids.contains(&e.id) {
            continue;
        }
        let live = e.live_status.as_deref().is_some_and(|s| s != "not_live");
        if live || e.availability.as_deref() == Some("private") {
            continue;
        }
        // Prefer the full metadata; fall back to the flat entry.
        let full = detail.iter().find(|d| d.id == e.id);
        fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}
        let channel = full
            .and_then(|d| non_empty(&d.channel))
            .or_else(|| non_empty(&e.channel))
            .or_else(|| non_empty(&e.uploader))
            .unwrap_or_default();
        let is_topic = channel.ends_with(" - Topic");
        let artist = channel.trim_end_matches(" - Topic").trim().to_string();
        if artist.is_empty() {
            continue;
        }
        let raw_title = full.map(|d| d.title.as_str()).filter(|t| !t.is_empty()).unwrap_or(&e.title);
        let mut title = raw_title.to_string();
        // "Artist - Title" -> "Title" when the prefix is the channel/artist.
        if let Some((a, rest)) = raw_title.split_once(" - ") {
            if crate::merge::artists_match(a, &artist) {
                title = rest.to_string();
            }
        }
        let low = raw_title.to_lowercase();
        let mut bonus = 0.0;
        if is_topic {
            bonus += 0.15;
        }
        if low.contains("official audio") {
            bonus += 0.10;
        }
        let mut t = new_track(&e.id, ProviderId::YouTube, &title, &artist);
        t.duration_ms = full
            .and_then(|d| d.duration)
            .or(e.duration)
            .filter(|s| *s > 0.0)
            .map(|s| (s * 1000.0) as u64);
        t.cover_url = full.and_then(best_thumb).or_else(|| best_thumb(e));
        t.permalink = Some(format!("https://www.youtube.com/watch?v={}", e.id));
        out.push(Candidate { track: t, bonus, playback: Some(playback(&e.id)) });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn durations() {
        assert_eq!(parse_iso8601_duration("PT3M45S"), Some(225_000));
        assert_eq!(parse_iso8601_duration("PT1H2M3S"), Some(3_723_000));
        assert_eq!(parse_iso8601_duration("PT45S"), Some(45_000));
        assert_eq!(parse_iso8601_duration("P1DT1S"), Some(86_401_000));
        assert_eq!(parse_iso8601_duration("garbage"), None);
    }

    #[test]
    fn search_uses_ytdlp_and_drops_livestreams() {
        let h = harness_with_ytdlp(all_secrets());
        h.cmds.on(
            "ytsearch10:",
            concat!(
                r#"{"id":"a1","title":"Daft Punk - Get Lucky (Official Audio)","channel":"Daft Punk - Topic"}"#,
                "\n",
                r#"{"id":"b2","title":"Live Set","channel":"Some Band","live_status":"is_live"}"#,
                "\n",
                r#"{"id":"c3","title":"Blocked","channel":"X","availability":"private"}"#,
                "\n",
                r#"{"id":"d4","title":"No Artist","channel":""}"#,
            ),
        );
        h.cmds.on(
            "a1",
            r#"{"id":"a1","title":"Daft Punk - Get Lucky (Official Audio)","channel":"Daft Punk - Topic","duration":369.0,"thumbnails":[{"url":"https://i.ytimg.com/small.jpg","width":120},{"url":"https://i.ytimg.com/big.jpg","width":480}]}"#,
        );
        let r = block_on(search(&h.svc, "get lucky")).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].track.id, "a1");
        assert_eq!(r[0].track.artist, "Daft Punk");
        assert_eq!(r[0].track.title, "Get Lucky (Official Audio)");
        assert_eq!(r[0].track.duration_ms, Some(369_000));
        assert_eq!(r[0].track.cover_url.as_deref(), Some("https://i.ytimg.com/big.jpg"));
        assert!(r[0].bonus > 0.2);
        assert_eq!(r[0].playback, Some(playback("a1")));
        // No API key was used, and the durations came from yt-dlp.
        assert_eq!(h.http.call_count("googleapis.com"), 0);
        assert_eq!(h.cmds.call_count("ytsearch10:get lucky"), 1);
    }

    #[test]
    fn results_survive_a_failing_detail_call() {
        let h = harness_with_ytdlp(all_secrets());
        h.cmds.on("ytsearch10:", r#"{"id":"a1","title":"Song","channel":"Artist","duration":200.0}"#);
        *h.cmds.missing.lock().unwrap() = Some("--skip-download".into());
        let r = block_on(search(&h.svc, "song")).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].track.duration_ms, Some(200_000));
    }

    #[test]
    fn a_missing_binary_is_reported_as_not_configured() {
        let h = harness_with_ytdlp(all_secrets());
        *h.cmds.missing.lock().unwrap() = Some("ytsearch".into());
        assert_eq!(block_on(search(&h.svc, "x")).unwrap_err(), ProviderError::NotConfigured);
    }
}
