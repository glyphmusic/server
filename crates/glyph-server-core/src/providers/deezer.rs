//! Deezer public API: metadata + official 30 s previews. No key required.

use super::*;
use crate::traits::HttpRequest;

#[derive(Deserialize)]
pub(crate) struct DzTrack {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub duration: Option<u64>,
    pub preview: Option<String>,
    pub link: Option<String>,
    pub isrc: Option<String>,
    pub artist: Option<DzArtist>,
    pub album: Option<DzAlbum>,
}
#[derive(Deserialize)]
pub(crate) struct DzArtist {
    #[serde(default)]
    pub name: String,
}
#[derive(Deserialize)]
pub(crate) struct DzAlbum {
    pub title: Option<String>,
    pub cover_medium: Option<String>,
}

pub(crate) fn to_candidate(t: DzTrack) -> Candidate {
    let mut out = new_track(
        t.id.to_string(),
        ProviderId::Deezer,
        &t.title,
        t.artist.as_ref().map(|a| a.name.as_str()).unwrap_or(""),
    );
    out.duration_ms = t.duration.map(|s| s * 1000);
    out.permalink = t.link;
    out.isrc = t.isrc.filter(|s| !s.is_empty());
    if let Some(a) = t.album {
        out.album = a.title;
        out.cover_url = a.cover_medium;
    }
    let pb = t
        .preview
        .filter(|p| !p.is_empty())
        .map(|url| PlaybackSource::Preview { provider: ProviderId::Deezer, url });
    candidate(out, pb)
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        data: Vec<DzTrack>,
    }
    let url = build_url("https://api.deezer.com/search", &[("q", q), ("limit", "15")]);
    let resp: Resp = fetch_json(svc, HttpRequest::get(url)).await?;
    Ok(resp.data.into_iter().map(to_candidate).collect())
}

/// Exact lookup by ISRC (used by the resolver before text search).
pub async fn by_isrc(svc: &Services, isrc: &str) -> PResult<Option<Candidate>> {
    if !isrc.chars().all(|c| c.is_ascii_alphanumeric()) || isrc.len() > 20 {
        return Ok(None);
    }
    match fetch_json::<DzTrack>(svc, HttpRequest::get(format!("https://api.deezer.com/track/isrc:{isrc}"))).await {
        Ok(t) if t.id != 0 => Ok(Some(to_candidate(t))),
        Ok(_) | Err(ProviderError::Parse) | Err(ProviderError::Http(404)) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn preview_is_playback() {
        let h = harness(Default::default());
        h.http.on(
            "api.deezer.com/search",
            200,
            r#"{"data":[{"id":3135556,"title":"One More Time","duration":320,"preview":"https://cdn/p.mp3",
              "link":"https://deezer/t","artist":{"name":"Daft Punk"},"album":{"title":"Discovery","cover_medium":"http://c"}}]}"#,
        );
        let r = block_on(search(&h.svc, "omt")).unwrap();
        assert_eq!(r[0].track.duration_ms, Some(320_000));
        assert!(matches!(r[0].playback, Some(PlaybackSource::Preview { .. })));
    }

    #[test]
    fn isrc_lookup_handles_error_body() {
        let h = harness(Default::default());
        h.http.on("track/isrc:NOPE", 200, r#"{"error":{"type":"DataException","code":800}}"#);
        assert!(block_on(by_isrc(&h.svc, "NOPE")).unwrap().is_none());
    }
}
