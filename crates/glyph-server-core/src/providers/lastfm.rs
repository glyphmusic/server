//! Last.fm `track.getSimilar` for `/v1/similar`.

use super::*;
use crate::traits::HttpRequest;

#[derive(Deserialize)]
struct Resp {
    similartracks: Option<Similar>,
}
#[derive(Deserialize)]
struct Similar {
    #[serde(default)]
    track: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    name: String,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    duration: Option<u64>,
    mbid: Option<String>,
    url: Option<String>,
    artist: Option<Artist>,
}
#[derive(Deserialize)]
struct Artist {
    #[serde(default)]
    name: String,
}

pub async fn similar(svc: &Services, artist: &str, title: &str) -> PResult<Vec<Track>> {
    let key = svc.config.secrets.lastfm_api_key.as_deref().ok_or(ProviderError::NotConfigured)?;
    let url = build_url(
        "https://ws.audioscrobbler.com/2.0/",
        &[
            ("method", "track.getsimilar"),
            ("artist", artist),
            ("track", title),
            ("autocorrect", "1"),
            ("limit", "25"),
            ("api_key", key),
            ("format", "json"),
        ],
    );
    let resp: Resp = fetch_json(svc, HttpRequest::get(url)).await?;
    Ok(resp
        .similartracks
        .map(|s| s.track)
        .unwrap_or_default()
        .into_iter()
        .map(|i| {
            let artist = i.artist.map(|a| a.name).unwrap_or_default();
            let id = i.mbid.filter(|m| !m.is_empty()).unwrap_or_else(|| format!("{artist}|{}", i.name));
            let mut t = new_track(id, ProviderId::LastFm, &i.name, &artist);
            t.duration_ms = i.duration.filter(|d| *d > 0).map(|s| s * 1000);
            t.permalink = i.url;
            t
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn parses_similar() {
        let h = harness(all_secrets());
        h.http.on(
            "ws.audioscrobbler.com",
            200,
            r##"{"similartracks":{"track":[{"name":"Around the World","duration":"429","mbid":"",
            "url":"https://last.fm/x","artist":{"name":"Daft Punk"},"image":[{"#text":"","size":"small"}]}]}}"##,
        );
        let r = block_on(similar(&h.svc, "Daft Punk", "One More Time")).unwrap();
        assert_eq!(r[0].id, "Daft Punk|Around the World");
        assert_eq!(r[0].duration_ms, Some(429_000));
        assert_eq!(r[0].source, ProviderId::LastFm);
    }
}
