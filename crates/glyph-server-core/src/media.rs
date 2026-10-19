//! Media contracts shared by the Worker and its yt-dlp companion.
use crate::{
    providers::{self, ProviderError},
    traits::{with_timeout, Services},
};
use glyph_protocol::{PlaybackSource, ProviderId, ResolveResponse};
use serde::{Deserialize, Serialize};

// The app decodes AAC/Vorbis; selecting arbitrary WebM audio could return unsupported Opus.
pub const AUDIO_FORMAT: &str = "bestaudio[ext=m4a]/bestaudio[acodec=vorbis]";

/// A valid, ignorable MP4 free atom, appended only after successful encoding.
/// Unknown-length HTTP streams may lose transport errors at proxy boundaries;
/// require this trailer before publishing such a representation in R2.
pub const VARIANT_COMPLETE_MARKER: &[u8] = b"\x00\x00\x00\x21freeglyph-variant-complete-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioVariant { Low, Standard, High }
impl AudioVariant {
    pub fn parse(value: &str) -> Option<Self> {
        match value { "low" => Some(Self::Low), "standard" => Some(Self::Standard), "high" => Some(Self::High), _ => None }
    }
    pub fn name(self) -> &'static str {
        match self { Self::Low => "low", Self::Standard => "standard", Self::High => "high" }
    }
    pub fn bitrate(self) -> u32 {
        match self { Self::Low => 48, Self::Standard => 96, Self::High => 128 }
    }
}

/// Validate the full representation size, not merely a small requested range.
pub fn content_range_total(header: &str) -> Option<u64> {
    let (range, total) = header.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (start.parse::<u64>().ok()?, end.parse::<u64>().ok()?, total.parse::<u64>().ok()?);
    (start <= end && end < total).then_some(total)
}
#[derive(Serialize, Deserialize)]
pub struct CommandRequest {
    pub args: Vec<String>,
}
#[derive(Serialize, Deserialize)]
pub struct CommandResponse {
    pub stdout: String,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}
pub fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

/// Accept only Glyph's precise metadata/URL commands, never arbitrary tool flags.
pub fn valid_command(args: &[String]) -> bool {
    let prefix = ["--ignore-config", "--no-warnings", "--no-playlist-reverse"];
    if args.len() < 6 || !args.iter().take(3).map(String::as_str).eq(prefix) {
        return false;
    }
    match (args[3].as_str(), args[4].as_str()) {
        ("--dump-json", "--flat-playlist") => {
            args.len() == 6
                && args[5]
                    .strip_prefix("ytsearch10:")
                    .is_some_and(|q| !q.trim().is_empty() && q.len() <= 512)
        }
        ("--dump-json", "--skip-download") => {
            args.len() >= 7 && args.len() <= 16 && args[5] == "--" && args[6..].iter().all(|v| v.strip_prefix("https://www.youtube.com/watch?v=").is_some_and(valid_video_id))
        }
        ("--get-url", "--no-playlist") => {
            args.len() == 9
                && args[5] == "-f"
                && args[6] == AUDIO_FORMAT
                && args[7] == "--"
                && args[8]
                    .strip_prefix("https://www.youtube.com/watch?v=")
                    .is_some_and(valid_video_id)
        }
        _ => false,
    }
}

/// Resolved YouTube audio URLs are reused until shortly before they expire,
/// so replays, seeks and other users skip the yt-dlp run (several seconds).
const URL_MARGIN_S: u64 = 20 * 60;
/// Lifetime assumed when the URL carries no `expire=`.
const URL_DEFAULT_TTL_S: u64 = 60 * 60;
/// Longest a URL is kept, whatever `expire=` says.
const URL_MAX_TTL_S: u64 = 6 * 60 * 60;

fn audio_url_key(id: &str) -> String {
    format!("ytaudio:v1:{id}")
}

/// Seconds a googlevideo URL may still be handed out, from its `expire=`.
pub fn url_ttl_s(url: &url::Url, now_s: u64) -> u64 {
    let expire = url.query_pairs().find(|(k, _)| k == "expire").and_then(|(_, v)| v.parse::<u64>().ok());
    match expire {
        Some(e) => e.saturating_sub(now_s).saturating_sub(URL_MARGIN_S).min(URL_MAX_TTL_S),
        None => URL_DEFAULT_TTL_S,
    }
}

/// Drops a cached YouTube URL that the upstream refused (an empty value
/// counts as a miss; it also overwrites the copy on disk).
pub async fn forget_audio_url(svc: &Services, id: &str) {
    svc.cache.put(&crate::cache::norm_key(&audio_url_key(id)), Vec::new(), URL_MAX_TTL_S).await;
}

pub fn audio_path(provider: ProviderId, id: &str) -> String {
    let name = serde_json::to_value(provider)
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    format!("/v1/audio/{name}/{id}")
}

pub fn proxy_playback(resp: &mut ResolveResponse) {
    let (Some(track), Some(source)) = (&resp.matched, &resp.source) else {
        return;
    };
    let provider = source.provider();
    let id = if track.source == provider {
        Some(track.id.as_str())
    } else {
        track.external_ids.get(&provider).map(String::as_str)
    };
    let Some(id) = id.filter(|v| valid_id(v)) else {
        resp.source = None;
        return;
    };
    let url = audio_path(provider, id);
    resp.source = Some(match source {
        PlaybackSource::Preview { .. } => PlaybackSource::Preview { provider, url },
        PlaybackSource::Stream { mime, .. } => PlaybackSource::Stream {
            provider,
            url,
            mime: mime.clone(),
        },
        PlaybackSource::Embed {
            provider: ProviderId::YouTube,
            ..
        } => PlaybackSource::Stream {
            provider,
            url,
            mime: None,
        },
        _ => return,
    });
}

pub async fn audio_url(
    svc: &Services,
    provider: ProviderId,
    id: &str,
) -> Result<String, ProviderError> {
    if !valid_id(id) || !svc.config.provider_enabled(provider) {
        return Err(ProviderError::NotConfigured);
    }
    let work = async {
        if provider == ProviderId::YouTube {
            if !valid_video_id(id) {
                return Err(ProviderError::Parse);
            }
            let bin = svc
                .config
                .ytdlp_path
                .as_deref()
                .ok_or(ProviderError::NotConfigured)?;
            let key = audio_url_key(id);
            if let Some(hit) = crate::cache::get_json::<String>(&*svc.cache, &key).await {
                return Ok(hit);
            }
            let video = format!("https://www.youtube.com/watch?v={id}");
            let args: Vec<String> = [
                "--ignore-config",
                "--no-warnings",
                "--no-playlist-reverse",
                "--get-url",
                "--no-playlist",
                "-f",
                AUDIO_FORMAT,
                "--",
                &video,
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            let out = svc
                .cmds
                .run(bin, &args, 20_000)
                .await
                .map_err(|_| ProviderError::Network)?;
            let text = String::from_utf8(out).map_err(|_| ProviderError::Parse)?;
            let lines: Vec<_> = text.lines().filter(|v| !v.trim().is_empty()).collect();
            if lines.len() != 1 {
                return Err(ProviderError::Parse);
            }
            let url = url::Url::parse(lines[0]).map_err(|_| ProviderError::Parse)?;
            if url.scheme() != "https"
                || !url
                    .host_str()
                    .is_some_and(|h| h.ends_with(".googlevideo.com"))
            {
                return Err(ProviderError::Parse);
            }
            let ttl = url_ttl_s(&url, svc.rt.now_ms() / 1000);
            let url: String = url.into();
            if ttl >= 60 {
                crate::cache::put_json(&*svc.cache, &key, &url, ttl).await;
            }
            return Ok(url);
        }
        let pb = if provider == ProviderId::Deezer {
            providers::deezer::by_id(svc, id)
                .await?
                .playback
                .ok_or(ProviderError::Parse)?
        } else {
            providers::playback_for(svc, provider, id).await?
        };
        match pb {
            PlaybackSource::Stream { url, .. } | PlaybackSource::Preview { url, .. } => Ok(url),
            _ => Err(ProviderError::NotConfigured),
        }
    };
    with_timeout(&*svc.rt, 22_000, work)
        .await
        .ok_or(ProviderError::Timeout)?
}

/// Explicit download permission stays separate from the playback cache.
pub async fn download_url(
    svc: &Services,
    provider: ProviderId,
    id: &str,
) -> Result<Option<String>, ProviderError> {
    use crate::traits::HttpRequest;
    if !valid_id(id) || !svc.config.provider_enabled(provider) {
        return Ok(None);
    }
    match provider {
        ProviderId::Archive => audio_url(svc, provider, id).await.map(Some),
        ProviderId::Jamendo => {
            let key = svc
                .config
                .secrets
                .jamendo_client_id
                .as_deref()
                .ok_or(ProviderError::NotConfigured)?;
            let url = providers::build_url(
                "https://api.jamendo.com/v3.0/tracks/",
                &[("client_id", key), ("format", "json"), ("id", id)],
            );
            let v: serde_json::Value = providers::fetch_json(svc, HttpRequest::get(url)).await?;
            let t = &v["results"][0];
            Ok((t["audiodownload_allowed"] == true)
                .then(|| {
                    t["audiodownload"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
                .flatten())
        }
        ProviderId::Audius => {
            let url = providers::build_url(
                &format!("https://api.audius.co/v1/tracks/{id}"),
                &[("app_name", &svc.config.audius_app_name)],
            );
            let v: serde_json::Value = providers::fetch_json(svc, HttpRequest::get(url)).await?;
            let t = &v["data"];
            if !t["download_conditions"].is_null()
                || t["is_delete"] == true
                || !(t["access"]["download"] == true || t["is_downloadable"] == true)
            {
                return Ok(None);
            }
            Ok(Some(providers::build_url(
                &format!("https://api.audius.co/v1/tracks/{id}/download"),
                &[("app_name", &svc.config.audius_app_name)],
            )))
        }
        _ => Ok(None),
    }
}

/// Parse a single HTTP range, including suffix and open ranges.
pub fn byte_range(value: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let (a, b) = value.strip_prefix("bytes=")?.split_once('-')?;
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        return (n > 0).then_some((total.saturating_sub(n), total - 1));
    }
    let start: u64 = a.parse().ok()?;
    let end = if b.is_empty() {
        total - 1
    } else {
        b.parse::<u64>().ok()?.min(total - 1)
    };
    (start <= end && start < total).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_range_size_cannot_be_hidden_by_a_small_slice() {
        assert_eq!(content_range_total("bytes 0-9/999999999"), Some(999999999));
        for invalid in ["bytes 10-0/100", "bytes 0-10/10", "bytes 0-9/*", "bytes */100", "items 0-9/100"] {
            assert_eq!(content_range_total(invalid), None);
        }
    }
    #[test]
    fn ranges() {
        assert_eq!(byte_range("bytes=0-99", 50), Some((0, 49)));
        assert_eq!(byte_range("bytes=-10", 50), Some((40, 49)));
        assert_eq!(byte_range("bytes=20-", 50), Some((20, 49)));
        for bad in ["bytes=50-", "bytes=2-1", "bytes=0-2,4-5", "bytes=-0"] {
            assert_eq!(byte_range(bad, 50), None);
        }
    }
    #[test]
    fn rejects_command_injection() {
        for args in [
            vec!["--exec", "calc"],
            vec![
                "--ignore-config",
                "--no-warnings",
                "--no-playlist-reverse",
                "--dump-json",
                "--skip-download",
                "http://localhost/",
            ],
        ] {
            assert!(!valid_command(
                &args.into_iter().map(str::to_string).collect::<Vec<_>>()
            ));
        }
    }
    #[test]
    fn youtube_url_extraction_is_scoped() {
        use crate::testutil::*;
        use futures::executor::block_on;
        let h = harness_with_ytdlp(Default::default());
        h.cmds.on(
            "--get-url",
            "https://rr1.googlevideo.com/videoplayback?signature=test",
        );
        let url = block_on(audio_url(&h.svc, ProviderId::YouTube, "abcdefghijk")).unwrap();
        assert!(url.starts_with("https://rr1.googlevideo.com/"));
        let args = h.cmds.calls.lock().unwrap();
        assert!(args[0].contains(AUDIO_FORMAT));
        drop(args);
        assert!(block_on(audio_url(&h.svc, ProviderId::YouTube, "../bad")).is_err());
        // The second request is served from the cache, without yt-dlp.
        assert!(block_on(audio_url(&h.svc, ProviderId::YouTube, "abcdefghijk")).is_ok());
        assert_eq!(h.cmds.call_count("--get-url"), 1);
        block_on(forget_audio_url(&h.svc, "abcdefghijk"));
        h.cmds.rules.lock().unwrap().clear();
        h.cmds.on("--get-url", "https://localhost/private");
        assert!(block_on(audio_url(&h.svc, ProviderId::YouTube, "abcdefghijk")).is_err());
    }
    #[test]
    fn url_lifetime_follows_expire() {
        let u = url::Url::parse("https://rr1.googlevideo.com/videoplayback?expire=10000&x=1").unwrap();
        assert_eq!(url_ttl_s(&u, 10000 - 3600), 3600 - URL_MARGIN_S);
        assert_eq!(url_ttl_s(&u, 10000), 0);
        let u = url::Url::parse("https://rr1.googlevideo.com/videoplayback").unwrap();
        assert_eq!(url_ttl_s(&u, 0), URL_DEFAULT_TTL_S);
    }
    #[test]
    fn jamendo_download_requires_permission() {
        use crate::testutil::*;
        use futures::executor::block_on;
        let h = harness(all_secrets());
        h.http.on("api.jamendo.com", 200, r#"{"results":[{"audiodownload_allowed":false,"audiodownload":"https://dl.jamendo.com/file"}]}"#);
        assert_eq!(
            block_on(download_url(&h.svc, ProviderId::Jamendo, "1")).unwrap(),
            None
        );
        h.http.rules.lock().unwrap()[0].2 = r#"{"results":[{"audiodownload_allowed":true,"audiodownload":"https://dl.jamendo.com/file"}]}"#.into();
        assert!(block_on(download_url(&h.svc, ProviderId::Jamendo, "1"))
            .unwrap()
            .is_some());
    }
}
