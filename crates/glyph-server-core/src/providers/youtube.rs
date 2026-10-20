//! YouTube search: YouTube's own web search endpoint first (one HTTP request,
//! a few hundred ms), the `yt-dlp` binary as the fallback (no API key, no
//! quota; a flat search, then one follow-up call for the durations).
//! Playback is only ever the official IFrame embed (`embed_id` = video id),
//! so nothing is downloaded.

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
    channel: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
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
        let channel = e.channel.filter(|s| !s.is_empty()).or(e.uploader).unwrap_or_default();
        let ms = e.duration.filter(|s| *s > 0.0).map(|s| (s * 1000.0) as u64);
        (e.title, channel, ms)
    }))
}

const SEARCH_URL: &str = "https://www.youtube.com/youtubei/v1/search?prettyPrint=false";
const WEB_CLIENT_VERSION: &str = "2.20260930.01.00";

fn text_of(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v["simpleText"].as_str() {
        return Some(s.to_string());
    }
    let s: String = v["runs"].as_array()?.iter().filter_map(|r| r["text"].as_str()).collect();
    (!s.is_empty()).then_some(s)
}

fn video_renderers<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(r) = m.get("videoRenderer") {
                out.push(r);
                return;
            }
            m.values().for_each(|x| video_renderers(x, out));
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| video_renderers(x, out)),
        _ => {}
    }
}

fn parse_length(s: &str) -> Option<u64> {
    let mut total = 0u64;
    for part in s.trim().split(':') {
        total = total * 60 + part.trim().parse::<u64>().ok()?;
    }
    (total > 0).then_some(total * 1000)
}

/// Candidates from a web search response (live streams, premieres and
/// broken entries left out).
pub(crate) fn parse_native(v: &serde_json::Value) -> Vec<Candidate> {
    let mut found = Vec::new();
    video_renderers(v, &mut found);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in found {
        let Some(id) = r["videoId"].as_str().filter(|id| crate::media::valid_video_id(id)) else { continue };
        let Some(raw_title) = text_of(&r["title"]) else { continue };
        let Some(ms) = text_of(&r["lengthText"]).and_then(|s| parse_length(&s)) else { continue };
        if !seen.insert(id.to_string()) {
            continue;
        }
        let channel = text_of(&r["ownerText"]).or_else(|| text_of(&r["longBylineText"])).unwrap_or_default();
        let is_topic = channel.ends_with(" - Topic");
        let mut artist = channel.trim_end_matches(" - Topic").trim_end_matches("VEVO").trim().to_string();
        let mut title = raw_title.clone();
        if let Some((a, rest)) = raw_title.split_once(" - ") {
            if !a.trim().is_empty() && !rest.trim().is_empty() && (!is_topic || crate::merge::artists_match(a, &artist)) {
                artist = a.trim().to_string();
                title = rest.trim().to_string();
            }
        }
        if artist.is_empty() {
            continue;
        }
        let low = raw_title.to_lowercase();
        let mut bonus = 0.0;
        if is_topic {
            bonus += 0.15;
        }
        if low.contains("official audio") {
            bonus += 0.10;
        }
        let mut t = new_track(id, ProviderId::YouTube, &title, &artist);
        t.duration_ms = Some(ms);
        t.cover_url = Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"));
        t.permalink = Some(format!("https://www.youtube.com/watch?v={id}"));
        out.push(Candidate { track: t, bonus, playback: Some(playback(id)) });
        if out.len() >= MAX_RESULTS {
            break;
        }
    }
    out
}

/// One request to YouTube's web search (videos only).
async fn native_search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    let body = serde_json::json!({
        "context": {"client": {"clientName": "WEB", "clientVersion": WEB_CLIENT_VERSION, "hl": "en", "gl": "US"}},
        "query": q,
        "params": "EgIQAQ==",
    });
    let req = crate::traits::HttpRequest {
        method: crate::traits::Method::Post,
        url: SEARCH_URL.into(),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("x-youtube-client-name".into(), "1".into()),
            ("x-youtube-client-version".into(), WEB_CLIENT_VERSION.into()),
            ("origin".into(), "https://www.youtube.com".into()),
        ],
        body: Some(body.to_string().into_bytes()),
    };
    let v: serde_json::Value = fetch_json(svc, req).await?;
    let out = parse_native(&v);
    if out.is_empty() {
        return Err(ProviderError::Parse);
    }
    Ok(out)
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    // Seconds faster than starting yt-dlp; yt-dlp stays the fallback (also
    // when YouTube answers the web request with a bot check).
    if let Some(Ok(hits)) = with_timeout(&*svc.rt, 4_000, native_search(svc, q)).await {
        return Ok(hits);
    }
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

    // Durations are missing in flat mode only on older yt-dlp or non-video entries;
    // never fetch full 200 KB video dumps inside the CPU-capped Worker runtime.
    let mut detail: Vec<Entry> = Vec::new();
    let missing: Vec<_> = flat
        .iter()
        .filter(|e| {
            !e.title.is_empty()
                && (e.channel.as_ref().is_some_and(|s| !s.is_empty()) || e.uploader.as_ref().is_some_and(|s| !s.is_empty()))
                && !e.live_status.as_deref().is_some_and(|s| s != "not_live")
                && e.availability.as_deref() != Some("private")
                && e.duration.filter(|d| *d > 0.0).is_none()
        })
        .map(|e| &e.id)
        .filter(|id| ids.contains(id))
        .collect();
    if !cfg!(target_arch = "wasm32") && !missing.is_empty() {
        let mut args = base_args();
        args.push("--dump-json".into());
        args.push("--skip-download".into());
        args.push("--".into());
        args.extend(missing.iter().map(|id| format!("https://www.youtube.com/watch?v={id}")));
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
        let channel = full
            .and_then(|d| d.channel.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| e.channel.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| full.and_then(|d| d.uploader.as_deref().filter(|s| !s.is_empty())))
            .or_else(|| e.uploader.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let is_topic = channel.ends_with(" - Topic");
        let mut artist = channel.trim_end_matches(" - Topic").trim_end_matches("VEVO").trim().to_string();
        let raw_title = full.map(|d| d.title.as_str()).filter(|t| !t.is_empty()).unwrap_or(&e.title);
        let mut title = raw_title.to_string();
        // Match the desktop parser: an upload channel need not be the artist.
        if let Some((a, rest)) = raw_title.split_once(" - ") {
            if !a.trim().is_empty() && !rest.trim().is_empty()
                && (!is_topic || crate::merge::artists_match(a, &artist)) {
                artist = a.trim().to_string();
                title = rest.trim().to_string();
            }
        }
        if artist.is_empty() { continue; }
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

    #[test]
    fn parses_native_search() {
        let v = serde_json::json!({"contents": [{"itemSectionRenderer": {"contents": [
            {"videoRenderer": {"videoId": "dQw4w9WgXcQ", "title": {"runs": [{"text": "Rick Astley - Never Gonna Give You Up"}]},
                "ownerText": {"runs": [{"text": "Rick Astley"}]}, "lengthText": {"simpleText": "3:33"}}},
            {"videoRenderer": {"videoId": "aaaaaaaaaaa", "title": {"runs": [{"text": "Live radio"}]}, "ownerText": {"runs": [{"text": "X"}]}}}
        ]}}]});
        let c = parse_native(&v);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].track.title, "Never Gonna Give You Up");
        assert_eq!(c[0].track.duration_ms, Some(213_000));
    }
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn search_reads_artist_from_title_and_accepts_nullable_channels() {
        let h = harness_with_ytdlp(all_secrets());
        h.cmds.on("ytsearch10:", concat!(
            r#"{"id":"album","title":"Daft Punk - Get Lucky (Album Version / Audio)","channel":"TheStruckFernVEVO","duration":372}"#,
            "\n",
            r#"{"id":"nullable","title":"Daft Punk - Get Lucky","channel":null,"uploader":null,"duration":369}"#,
        ));
        let hits = block_on(search(&h.svc, "Daft Punk Get Lucky audio")).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].track.artist, "Daft Punk");
        assert_eq!(hits[1].track.artist, "Daft Punk");
        let want = track("sp1", ProviderId::Spotify, "Get Lucky", "Daft Punk", 369);
        assert!(crate::resolver::score(&want, &hits[0]) >= crate::resolver::ACCEPT_SCORE);
    }

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
        assert_eq!(h.cmds.call_count("--skip-download"), 0);
    }

    #[test]
    fn a_missing_binary_is_reported_as_not_configured() {
        let h = harness_with_ytdlp(all_secrets());
        *h.cmds.missing.lock().unwrap() = Some("ytsearch".into());
        assert_eq!(block_on(search(&h.svc, "x")).unwrap_err(), ProviderError::NotConfigured);
    }
}
