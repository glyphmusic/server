//! LRCLIB lyrics (`/api/get`, fallback `/api/search`).

use super::*;
use crate::traits::HttpRequest;
use glyph_protocol::{LyricLine, LyricsResponse};

#[derive(Deserialize, Clone)]
struct Item {
    #[serde(default)]
    instrumental: bool,
    #[serde(rename = "plainLyrics")]
    plain: Option<String>,
    #[serde(rename = "syncedLyrics")]
    synced: Option<String>,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    duration: Option<u64>,
}

/// Parse LRC (`[mm:ss.xx]text`, several stamps per line allowed); metadata tags
/// like `[ar:...]` are ignored. Result is sorted by time.
pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for line in lrc.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some((tag, after)) = stripped.split_once(']') else { break };
            match parse_stamp(tag) {
                Some(ms) => stamps.push(ms),
                None => {
                    stamps.clear();
                    rest = "";
                    break;
                }
            }
            rest = after;
        }
        let text = rest.trim();
        for ms in stamps {
            out.push(LyricLine { time_ms: ms, text: text.to_string() });
        }
    }
    out.sort_by_key(|l| l.time_ms);
    out
}

fn parse_stamp(tag: &str) -> Option<u64> {
    let (m, s) = tag.split_once(':')?;
    let m: u64 = m.trim().parse().ok()?;
    let s: f64 = s.trim().parse().ok()?;
    if !(0.0..60.0).contains(&s) {
        return None;
    }
    Some(m * 60_000 + (s * 1000.0).round() as u64)
}

fn to_response(i: Item) -> LyricsResponse {
    LyricsResponse {
        provider: ProviderId::LrcLib,
        synced: i.synced.as_deref().map(parse_lrc).unwrap_or_default(),
        plain: i.plain.filter(|p| !p.trim().is_empty()),
        instrumental: i.instrumental,
    }
}

pub async fn lyrics(
    svc: &Services,
    title: &str,
    artist: &str,
    album: Option<&str>,
    duration_ms: Option<u64>,
) -> PResult<Option<LyricsResponse>> {
    let dur = duration_ms.map(|d| (d / 1000).to_string());
    let mut params = vec![("track_name", title), ("artist_name", artist)];
    if let Some(a) = album {
        params.push(("album_name", a));
    }
    if let Some(d) = dur.as_deref() {
        params.push(("duration", d));
    }
    match fetch_json::<Item>(svc, HttpRequest::get(build_url("https://lrclib.net/api/get", &params))).await {
        Ok(i) => return Ok(Some(to_response(i))),
        Err(ProviderError::Http(404)) => {}
        Err(e) => return Err(e),
    }
    let url = build_url("https://lrclib.net/api/search", &[("track_name", title), ("artist_name", artist)]);
    let items: Vec<Item> = fetch_json(svc, HttpRequest::get(url)).await?;
    let best = items
        .into_iter()
        .filter(|i| match (i.duration, duration_ms) {
            (Some(a), Some(b)) => (a * 1000).abs_diff(b) <= 5000,
            _ => true,
        })
        .max_by_key(|i| (i.synced.is_some() as u8, i.plain.is_some() as u8));
    Ok(best.map(to_response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn lrc_parsing() {
        let l = parse_lrc("[ar:Someone]\n[00:12.50]Hello\n[01:02.00][00:30.25]Chorus\n[00:40.00]\nno stamp");
        assert_eq!(l.len(), 4);
        assert_eq!((l[0].time_ms, l[0].text.as_str()), (12_500, "Hello"));
        assert_eq!((l[1].time_ms, l[1].text.as_str()), (30_250, "Chorus"));
        assert_eq!(l[2].text, "");
        assert_eq!(l[3].time_ms, 62_000);
    }

    #[test]
    fn falls_back_to_search_on_404() {
        let h = harness(Default::default());
        h.http.on("lrclib.net/api/get", 404, r#"{"code":404}"#);
        h.http.on(
            "lrclib.net/api/search",
            200,
            r#"[{"duration":215,"plainLyrics":"la","syncedLyrics":"[00:01.00]la","instrumental":false}]"#,
        );
        let r = block_on(lyrics(&h.svc, "t", "a", None, Some(214_000))).unwrap().unwrap();
        assert_eq!(r.synced.len(), 1);
        assert_eq!(r.plain.as_deref(), Some("la"));
        assert!(!r.instrumental);
    }
}
