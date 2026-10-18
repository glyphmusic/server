//! Audius: open API, full playback via the official stream endpoint.

use super::*;
use crate::traits::HttpRequest;

const BASE: &str = "https://api.audius.co/v1";

#[derive(Deserialize)]
struct Resp {
    #[serde(default)]
    data: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    title: String,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    duration: Option<u64>,
    user: User,
    artwork: Option<Artwork>,
    permalink: Option<String>,
    is_streamable: Option<bool>,
    isrc: Option<String>,
}
#[derive(Deserialize)]
struct User {
    #[serde(default)]
    name: String,
}
#[derive(Deserialize)]
struct Artwork {
    #[serde(rename = "480x480")]
    medium: Option<String>,
    #[serde(rename = "150x150")]
    small: Option<String>,
}

/// Official stream endpoint; the app fetches audio from Audius directly.
pub fn playback(svc: &Services, id: &str) -> PlaybackSource {
    PlaybackSource::Stream {
        provider: ProviderId::Audius,
        url: build_url(&format!("{BASE}/tracks/{id}/stream"), &[("app_name", &svc.config.audius_app_name)]),
        mime: Some("audio/mpeg".into()),
    }
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    search_page(svc, q, 0).await
}

/// One page of 15 results (`page` 0 = first).
pub async fn search_page(svc: &Services, q: &str, page: usize) -> PResult<Vec<Candidate>> {
    let offset = (page * 15).to_string();
    let url = build_url(
        &format!("{BASE}/tracks/search"),
        &[("query", q), ("limit", "15"), ("offset", &offset), ("app_name", &svc.config.audius_app_name)],
    );
    let resp: Resp = fetch_json(svc, HttpRequest::get(url)).await?;
    Ok(resp
        .data
        .into_iter()
        .filter(|i| i.is_streamable.unwrap_or(true))
        .map(|i| {
            let mut t = new_track(&i.id, ProviderId::Audius, &i.title, &i.user.name);
            t.duration_ms = i.duration.map(|s| s * 1000);
            t.cover_url = i.artwork.and_then(|a| a.medium.or(a.small));
            t.permalink = i.permalink.map(|p| format!("https://audius.co{p}"));
            t.isrc = i.isrc.filter(|s| !s.is_empty());
            let pb = playback(svc, &t.id);
            candidate(t, Some(pb))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn parses_search() {
        let h = harness(Default::default());
        h.http.on(
            "audius.co/v1/tracks/search",
            200,
            r#"{"data":[{"id":"D7KyD","title":"Song","duration":215,"user":{"name":"Artist"},
            "artwork":{"480x480":"http://c/480"},"permalink":"/artist/song","is_streamable":true},
            {"id":"X","title":"Blocked","duration":1,"user":{"name":"A"},"is_streamable":false}]}"#,
        );
        let r = block_on(search(&h.svc, "song")).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].track.duration_ms, Some(215_000));
        assert_eq!(r[0].track.permalink.as_deref(), Some("https://audius.co/artist/song"));
        match r[0].playback.as_ref().unwrap() {
            PlaybackSource::Stream { url, .. } => {
                assert_eq!(url, "https://api.audius.co/v1/tracks/D7KyD/stream?app_name=glyph")
            }
            _ => panic!(),
        }
    }
}
