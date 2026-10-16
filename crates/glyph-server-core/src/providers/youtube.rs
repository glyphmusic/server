//! YouTube Data API v3 (official). `search.list` costs 100 quota units, so
//! results are cached for hours and a daily budget guard stops calls early.
//! Playback is only ever the official IFrame embed (`embed_id` = video id).

use super::*;
use crate::cache;
use crate::traits::HttpRequest;

const SEARCH_COST: u64 = 101; // search.list (100) + videos.list (1)

#[derive(Deserialize)]
struct SearchResp {
    #[serde(default)]
    items: Vec<SearchItem>,
}
#[derive(Deserialize)]
struct SearchItem {
    id: VideoId,
    snippet: Snippet,
}
#[derive(Deserialize)]
struct VideoId {
    #[serde(rename = "videoId")]
    video_id: Option<String>,
}
#[derive(Deserialize)]
struct Snippet {
    title: String,
    #[serde(rename = "channelTitle", default)]
    channel_title: String,
    thumbnails: Option<Thumbs>,
}
#[derive(Deserialize)]
struct Thumbs {
    high: Option<Thumb>,
    medium: Option<Thumb>,
}
#[derive(Deserialize)]
struct Thumb {
    url: String,
}
#[derive(Deserialize)]
struct VideosResp {
    #[serde(default)]
    items: Vec<VideoItem>,
}
#[derive(Deserialize)]
struct VideoItem {
    id: String,
    #[serde(rename = "contentDetails")]
    content_details: Option<Details>,
    status: Option<Status>,
}
#[derive(Deserialize)]
struct Details {
    duration: String,
}
#[derive(Deserialize)]
struct Status {
    embeddable: Option<bool>,
}

pub fn playback(video_id: &str) -> PlaybackSource {
    PlaybackSource::Embed { provider: ProviderId::YouTube, embed_id: video_id.to_string() }
}

/// `PT1H2M3S` -> milliseconds.
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

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">")
}

fn day(svc: &Services) -> u64 {
    svc.rt.now_ms() / 86_400_000
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    let key = svc.config.secrets.youtube_api_key.as_deref().ok_or(ProviderError::NotConfigured)?;
    let quota_key = format!("quota:youtube:{}", day(svc));
    let spent: u64 = cache::get_json(&*svc.cache, &quota_key).await.unwrap_or(0);
    if spent + SEARCH_COST > svc.config.youtube_daily_budget_units {
        return Err(ProviderError::Quota);
    }

    let url = build_url(
        "https://www.googleapis.com/youtube/v3/search",
        &[
            ("part", "snippet"),
            ("type", "video"),
            ("videoCategoryId", "10"),
            ("maxResults", "10"),
            ("q", q),
            ("key", key),
        ],
    );
    let resp = fetch_json::<SearchResp>(svc, HttpRequest::get(url)).await;
    // 403 from the Data API is (almost always) quotaExceeded.
    let resp = match resp {
        Err(ProviderError::Http(403)) => return Err(ProviderError::Quota),
        other => other?,
    };
    cache::put_json(&*svc.cache, &quota_key, &(spent + SEARCH_COST), 2 * 86_400).await;

    let ids: Vec<String> = resp.items.iter().filter_map(|i| i.id.video_id.clone()).collect();
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let joined = ids.join(",");
    let vurl = build_url(
        "https://www.googleapis.com/youtube/v3/videos",
        &[("part", "contentDetails,status"), ("id", &joined), ("key", key)],
    );
    let videos: VideosResp = fetch_json(svc, HttpRequest::get(vurl)).await?;

    let mut out = Vec::new();
    for item in resp.items {
        let Some(vid) = item.id.video_id else { continue };
        let Some(v) = videos.items.iter().find(|v| v.id == vid) else { continue };
        if v.status.as_ref().and_then(|s| s.embeddable) == Some(false) {
            continue; // cannot be played in the embed widget
        }
        let channel = unescape(&item.snippet.channel_title);
        let is_topic = channel.ends_with(" - Topic");
        let artist = channel.trim_end_matches(" - Topic").to_string();
        let raw_title = unescape(&item.snippet.title);
        let mut title = raw_title.clone();
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
        let mut t = new_track(&vid, ProviderId::YouTube, &title, &artist);
        t.duration_ms = v.content_details.as_ref().and_then(|d| parse_iso8601_duration(&d.duration));
        t.cover_url = item.snippet.thumbnails.and_then(|t| t.high.or(t.medium)).map(|t| t.url);
        t.permalink = Some(format!("https://www.youtube.com/watch?v={vid}"));
        out.push(Candidate { track: t, bonus, playback: Some(playback(&vid)) });
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
    fn search_uses_music_category_and_filters_non_embeddable() {
        let h = harness(all_secrets());
        h.http.on(
            "youtube/v3/search",
            200,
            r#"{"items":[
              {"id":{"videoId":"a1"},"snippet":{"title":"Daft Punk - Get Lucky (Official Audio)","channelTitle":"Daft Punk - Topic"}},
              {"id":{"videoId":"b2"},"snippet":{"title":"Blocked","channelTitle":"X"}}]}"#,
        );
        h.http.on(
            "youtube/v3/videos",
            200,
            r#"{"items":[{"id":"a1","contentDetails":{"duration":"PT6M9S"},"status":{"embeddable":true}},
                         {"id":"b2","contentDetails":{"duration":"PT1M"},"status":{"embeddable":false}}]}"#,
        );
        let r = block_on(search(&h.svc, "get lucky")).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].track.artist, "Daft Punk");
        assert_eq!(r[0].track.title, "Get Lucky (Official Audio)");
        assert_eq!(r[0].track.duration_ms, Some(369_000));
        assert!(r[0].bonus > 0.2);
        let calls = h.http.calls.lock().unwrap().clone();
        assert!(calls[0].contains("videoCategoryId=10"));
    }

    #[test]
    fn daily_budget_guard() {
        let mut s = all_secrets();
        s.youtube_api_key = Some("K".into());
        let h = harness(s);
        h.http.on("youtube/v3/search", 200, r#"{"items":[]}"#);
        for _ in 0..200 {
            let _ = block_on(search(&h.svc, "q"));
        }
        // Budget (9000) / 101 per search => at most 89 upstream searches.
        assert!(h.http.call_count("youtube/v3/search") <= 90);
        assert_eq!(block_on(search(&h.svc, "q")).unwrap_err(), ProviderError::Quota);
    }
}
