//! Spotify Web API (client credentials): metadata only, no playback.

use super::*;
use crate::traits::HttpRequest;

#[derive(Deserialize)]
pub(crate) struct SpTrack {
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub artists: Vec<Named>,
    pub album: Option<Album>,
    pub duration_ms: Option<u64>,
    pub external_ids: Option<ExtIds>,
    pub external_urls: Option<ExtUrls>,
}
#[derive(Deserialize)]
pub(crate) struct Named {
    #[serde(default)]
    pub name: String,
}
#[derive(Deserialize)]
pub(crate) struct Album {
    pub name: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}
#[derive(Deserialize)]
pub(crate) struct Image {
    pub url: String,
}
#[derive(Deserialize)]
pub(crate) struct ExtIds {
    pub isrc: Option<String>,
}
#[derive(Deserialize)]
pub(crate) struct ExtUrls {
    pub spotify: Option<String>,
}

pub(crate) async fn token(svc: &Services) -> PResult<String> {
    let s = &svc.config.secrets;
    let (id, secret) = match (&s.spotify_client_id, &s.spotify_client_secret) {
        (Some(i), Some(s)) => (i, s),
        _ => return Err(ProviderError::NotConfigured),
    };
    soundcloud::cached_token(svc, "token:spotify", "https://accounts.spotify.com/api/token", id, secret).await
}

pub(crate) fn to_track(t: SpTrack) -> Option<Track> {
    let id = t.id?;
    if t.name.is_empty() {
        return None;
    }
    let artist = t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
    let mut out = new_track(&id, ProviderId::Spotify, &t.name, &artist);
    out.duration_ms = t.duration_ms;
    if let Some(a) = t.album {
        out.album = a.name;
        out.cover_url = a.images.into_iter().next().map(|i| i.url);
    }
    out.isrc = t.external_ids.and_then(|e| e.isrc);
    out.permalink = t.external_urls.and_then(|u| u.spotify);
    Some(out)
}

pub(crate) fn auth(tok: &str) -> (&'static str, String) {
    ("authorization", format!("Bearer {tok}"))
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    #[derive(Deserialize)]
    struct Resp {
        tracks: Page,
    }
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        items: Vec<SpTrack>,
    }
    let tok = token(svc).await?;
    let url = build_url("https://api.spotify.com/v1/search", &[("q", q), ("type", "track"), ("limit", "15")]);
    let (k, v) = auth(&tok);
    let resp: Resp = fetch_json(svc, HttpRequest::get(url).header(k, v)).await?;
    Ok(resp.tracks.items.into_iter().filter_map(to_track).map(|t| candidate(t, None)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn maps_isrc_and_cover() {
        let h = harness(all_secrets());
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on(
            "v1/search",
            200,
            r#"{"tracks":{"items":[{"id":"s1","name":"N","artists":[{"name":"A"},{"name":"B"}],
            "album":{"name":"Alb","images":[{"url":"http://i"}]},"duration_ms":200000,
            "external_ids":{"isrc":"USX"},"external_urls":{"spotify":"http://sp"}}]}}"#,
        );
        let r = block_on(search(&h.svc, "n")).unwrap();
        let t = &r[0].track;
        assert_eq!((t.artist.as_str(), t.isrc.as_deref(), t.album.as_deref()), ("A, B", Some("USX"), Some("Alb")));
        assert!(r[0].playback.is_none());
    }
}
