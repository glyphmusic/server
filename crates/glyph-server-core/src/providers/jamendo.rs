//! Jamendo: official API with a client id; full playback via `audio` URLs.

use super::*;
use crate::traits::HttpRequest;

const BASE: &str = "https://api.jamendo.com/v3.0/tracks/";

#[derive(Deserialize)]
struct Resp {
    #[serde(default)]
    results: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    name: String,
    #[serde(default)]
    artist_name: String,
    album_name: Option<String>,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    duration: Option<u64>,
    image: Option<String>,
    audio: Option<String>,
    shareurl: Option<String>,
    audiodownload_allowed: Option<bool>,
}

fn key(svc: &Services) -> PResult<&str> {
    svc.config.secrets.jamendo_client_id.as_deref().ok_or(ProviderError::NotConfigured)
}

fn to_candidate(i: Item) -> Option<Candidate> {
    let audio = i.audio.filter(|a| !a.is_empty())?;
    let _ = i.audiodownload_allowed;
    let mut t = new_track(&i.id, ProviderId::Jamendo, &i.name, &i.artist_name);
    t.album = i.album_name.filter(|s| !s.is_empty());
    t.duration_ms = i.duration.map(|s| s * 1000);
    t.cover_url = i.image.filter(|s| !s.is_empty());
    t.permalink = i.shareurl;
    let pb = PlaybackSource::Stream { provider: ProviderId::Jamendo, url: audio, mime: Some("audio/mpeg".into()) };
    Some(candidate(t, Some(pb)))
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    search_page(svc, q, 0).await
}

/// One page of 15 results (`page` 0 = first).
pub async fn search_page(svc: &Services, q: &str, page: usize) -> PResult<Vec<Candidate>> {
    let offset = (page * 15).to_string();
    let url = build_url(
        BASE,
        &[("client_id", key(svc)?), ("format", "json"), ("limit", "15"), ("offset", &offset), ("search", q), ("audioformat", "mp32")],
    );
    let resp: Resp = fetch_json(svc, HttpRequest::get(url)).await?;
    Ok(resp.results.into_iter().filter_map(to_candidate).collect())
}

pub async fn playback(svc: &Services, id: &str) -> PResult<PlaybackSource> {
    let url = build_url(BASE, &[("client_id", key(svc)?), ("format", "json"), ("id", id), ("audioformat", "mp32")]);
    let resp: Resp = fetch_json(svc, HttpRequest::get(url)).await?;
    resp.results
        .into_iter()
        .find_map(to_candidate)
        .and_then(|c| c.playback)
        .ok_or(ProviderError::Parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn parses_numeric_strings() {
        let h = harness(all_secrets());
        h.http.on(
            "jamendo.com",
            200,
            r#"{"results":[{"id":"1","name":"T","artist_name":"A","duration":"187","audio":"https://s/a.mp3","image":"http://i"}]}"#,
        );
        let r = block_on(search(&h.svc, "t")).unwrap();
        assert_eq!(r[0].track.duration_ms, Some(187_000));
        assert!(matches!(r[0].playback, Some(PlaybackSource::Stream { .. })));
    }
}
