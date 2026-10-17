//! `POST /v1/import/spotify`: read the track list of a *public* playlist
//! (Spotify, Deezer or YouTube link). Metadata only; resolving happens per
//! track via `/v1/resolve`. Private libraries are read by the app itself.

use crate::error::ApiErr;
use crate::providers::{self, build_url, fetch_json, new_track, spotify, ProviderError};
use crate::traits::{HttpRequest, Services};
use glyph_protocol::{ImportResponse, ProviderId, Track};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Spotify(String),
    Deezer(String),
    DeezerUser(String),
    LastFm(String),
    YouTube(String),
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn parse_url(raw: &str) -> Option<Target> {
    let raw = raw.trim();
    if let Some(id) = raw.strip_prefix("spotify:playlist:") {
        return valid_id(id).then(|| Target::Spotify(id.to_string()));
    }
    let u = url::Url::parse(raw).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.trim_start_matches("www.").to_ascii_lowercase();
    let segs: Vec<&str> = u.path_segments()?.collect();
    let after = |name: &str| segs.iter().position(|s| *s == name).and_then(|i| segs.get(i + 1)).copied();
    match host.as_str() {
        "open.spotify.com" | "play.spotify.com" => after("playlist").filter(|i| valid_id(i)).map(|i| Target::Spotify(i.into())),
        "deezer.com" | "deezer.page.link" => {
            let numeric = |i: &&str| !i.is_empty() && i.len() <= 64 && i.bytes().all(|c| c.is_ascii_digit());
            after("playlist").filter(numeric).map(|i| Target::Deezer(i.into()))
                .or_else(|| after("profile").or_else(|| after("user")).filter(numeric).map(|i| Target::DeezerUser(i.into())))
        }
        "last.fm" | "lastfm.de" | "lastfm.com" => after("user").filter(|i| valid_id(i)).map(|i| Target::LastFm(i.into())),
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be" => u
            .query_pairs()
            .find(|(k, _)| k == "list")
            .map(|(_, v)| v.into_owned())
            .filter(|v| valid_id(v))
            .map(Target::YouTube),
        _ => None,
    }
}

fn map_err(e: ProviderError) -> ApiErr {
    match e {
        ProviderError::Http(404) => ApiErr::not_found("playlist not found or not public"),
        ProviderError::Http(401) | ProviderError::Http(403) => {
            ApiErr::new(403, "playlist_not_accessible", "the playlist is private or the provider denied access")
        }
        ProviderError::NotConfigured => ApiErr::unavailable("this server has no key for that provider"),
        other => ApiErr::unavailable(format!("provider error: {other}")),
    }
}

pub async fn import(svc: &Services, url: &str) -> Result<ImportResponse, ApiErr> {
    let target = parse_url(url)
        .ok_or_else(|| ApiErr::bad_request("unsupported link; expected a public Spotify, Deezer or YouTube playlist URL"))?;
    match target {
        Target::Spotify(id) => spotify_playlist(svc, &id).await.map_err(map_err),
        Target::Deezer(id) => deezer_playlist(svc, &id).await.map_err(map_err),
        Target::DeezerUser(id) => deezer_favorites(svc, &id).await.map_err(map_err),
        Target::LastFm(user) => lastfm_loved(svc, &user).await.map_err(map_err),
        Target::YouTube(id) => youtube_playlist(svc, &id).await.map_err(map_err),
    }
}

async fn deezer_favorites(svc: &Services, id: &str) -> Result<ImportResponse, ProviderError> {
    #[derive(Deserialize)]
    struct Page { data: Vec<providers::deezer::DzTrack>, next: Option<String> }
    let mut tracks = Vec::new();
    for page in 0..svc.config.import_max_pages {
        let offset = (page * 100).to_string();
        let url = build_url(&format!("https://api.deezer.com/user/{id}/tracks"), &[("limit", "100"), ("index", &offset)]);
        let page: Page = fetch_json(svc, HttpRequest::get(url)).await?;
        tracks.extend(page.data.into_iter().map(|t| providers::deezer::to_candidate(t).track));
        if page.next.is_none() { break; }
    }
    Ok(ImportResponse { name: format!("Deezer favorites ({id})"), source: ProviderId::Deezer, tracks })
}

async fn lastfm_loved(svc: &Services, user: &str) -> Result<ImportResponse, ProviderError> {
    let key = svc.config.secrets.lastfm_api_key.as_deref().ok_or(ProviderError::NotConfigured)?;
    let mut tracks = Vec::new();
    for page in 1..=svc.config.import_max_pages {
        let page_s = page.to_string();
        let url = build_url("https://ws.audioscrobbler.com/2.0/", &[("method","user.getlovedtracks"),("api_key",key),("format","json"),("user",user),("limit","200"),("page",&page_s)]);
        let v: serde_json::Value = fetch_json(svc, HttpRequest::get(url)).await?;
        if v["error"].is_number() { return Err(ProviderError::Parse); }
        let list = &v["lovedtracks"]["track"];
        let items = list.as_array().cloned().unwrap_or_else(|| if list.is_object() { vec![list.clone()] } else { vec![] });
        for item in items {
            let Some(title) = item["name"].as_str() else { continue };
            let artist = item["artist"]["name"].as_str().or_else(|| item["artist"].as_str()).unwrap_or("");
            let mut t = new_track(format!("{artist}|{title}"), ProviderId::LastFm, title, artist);
            t.permalink = item["url"].as_str().map(str::to_string);
            tracks.push(t);
        }
        let total = v["lovedtracks"]["@attr"]["totalPages"].as_str().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
        if page >= total { break; }
    }
    Ok(ImportResponse { name: format!("{user} (Last.fm loved)"), source: ProviderId::LastFm, tracks })
}

async fn spotify_playlist(svc: &Services, id: &str) -> Result<ImportResponse, ProviderError> {
    #[derive(Deserialize)]
    struct Name {
        name: String,
    }
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        items: Vec<Entry>,
        next: Option<String>,
    }
    #[derive(Deserialize)]
    struct Entry {
        track: Option<spotify::SpTrack>,
    }
    let tok = spotify::token(svc).await?;
    let (k, v) = spotify::auth(&tok);
    let base = format!("https://api.spotify.com/v1/playlists/{id}");
    let name: Name = fetch_json(svc, HttpRequest::get(build_url(&base, &[("fields", "name")])).header(k, v.clone())).await?;
    let mut tracks = Vec::new();
    let mut offset = 0u32;
    for _ in 0..svc.config.import_max_pages {
        let off = offset.to_string();
        let url = build_url(
            &format!("{base}/tracks"),
            &[
                ("limit", "100"),
                ("offset", &off),
                ("fields", "items(track(id,name,duration_ms,artists(name),album(name,images),external_ids(isrc),external_urls(spotify))),next"),
            ],
        );
        let page: Page = fetch_json(svc, HttpRequest::get(url).header(k, v.clone())).await?;
        tracks.extend(page.items.into_iter().filter_map(|e| e.track).filter_map(spotify::to_track));
        if page.next.is_none() {
            break;
        }
        offset += 100;
    }
    Ok(ImportResponse { name: name.name, source: ProviderId::Spotify, tracks })
}

async fn deezer_playlist(svc: &Services, id: &str) -> Result<ImportResponse, ProviderError> {
    #[derive(Deserialize)]
    struct Pl {
        #[serde(default)]
        title: String,
        tracks: Option<Tracks>,
    }
    #[derive(Deserialize)]
    struct Tracks {
        #[serde(default)]
        data: Vec<providers::deezer::DzTrack>,
    }
    let pl: Pl = fetch_json(svc, HttpRequest::get(format!("https://api.deezer.com/playlist/{id}"))).await?;
    let tracks: Vec<Track> = pl
        .tracks
        .map(|t| t.data)
        .unwrap_or_default()
        .into_iter()
        .map(|t| providers::deezer::to_candidate(t).track)
        .collect();
    Ok(ImportResponse { name: pl.title, source: ProviderId::Deezer, tracks })
}

async fn youtube_playlist(svc: &Services, id: &str) -> Result<ImportResponse, ProviderError> {
    #[derive(Deserialize)]
    struct Items {
        #[serde(default)]
        items: Vec<Item>,
        #[serde(rename = "nextPageToken")]
        next: Option<String>,
    }
    #[derive(Deserialize)]
    struct Item {
        snippet: Snip,
    }
    #[derive(Deserialize)]
    struct Snip {
        #[serde(default)]
        title: String,
        #[serde(rename = "videoOwnerChannelTitle", default)]
        channel: String,
        #[serde(rename = "resourceId")]
        resource: Option<Res>,
    }
    #[derive(Deserialize)]
    struct Res {
        #[serde(rename = "videoId")]
        video_id: Option<String>,
    }
    #[derive(Deserialize)]
    struct Pls {
        #[serde(default)]
        items: Vec<Pl>,
    }
    #[derive(Deserialize)]
    struct Pl {
        snippet: PlSnip,
    }
    #[derive(Deserialize)]
    struct PlSnip {
        #[serde(default)]
        title: String,
    }
    let key = svc.config.secrets.youtube_api_key.as_deref().ok_or(ProviderError::NotConfigured)?;
    let pls: Pls = fetch_json(
        svc,
        HttpRequest::get(build_url(
            "https://www.googleapis.com/youtube/v3/playlists",
            &[("part", "snippet"), ("id", id), ("key", key)],
        )),
    )
    .await
    .map_err(|e| if e == ProviderError::Http(403) { ProviderError::Quota } else { e })?;
    let name = pls.items.into_iter().next().ok_or(ProviderError::Http(404))?.snippet.title;
    let mut tracks = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..svc.config.import_max_pages {
        let mut params = vec![("part", "snippet"), ("playlistId", id), ("maxResults", "50"), ("key", key)];
        if let Some(t) = token.as_deref() {
            params.push(("pageToken", t));
        }
        let page: Items = fetch_json(svc, HttpRequest::get(build_url("https://www.googleapis.com/youtube/v3/playlistItems", &params))).await?;
        for it in page.items {
            let Some(vid) = it.snippet.resource.and_then(|r| r.video_id) else { continue };
            let artist = it.snippet.channel.trim_end_matches(" - Topic").to_string();
            let mut t = new_track(&vid, ProviderId::YouTube, &it.snippet.title, &artist);
            t.permalink = Some(format!("https://www.youtube.com/watch?v={vid}"));
            tracks.push(t);
        }
        token = page.next;
        if token.is_none() {
            break;
        }
    }
    Ok(ImportResponse { name, source: ProviderId::YouTube, tracks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_url("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc"),
            Some(Target::Spotify("37i9dQZF1DXcBWIGoYBM5M".into()))
        );
        assert_eq!(
            parse_url("https://open.spotify.com/intl-de/playlist/37i9dQZF1DXcBWIGoYBM5M"),
            Some(Target::Spotify("37i9dQZF1DXcBWIGoYBM5M".into()))
        );
        assert_eq!(parse_url("spotify:playlist:abc123"), Some(Target::Spotify("abc123".into())));
        assert_eq!(parse_url("https://www.deezer.com/en/playlist/1234567"), Some(Target::Deezer("1234567".into())));
        assert_eq!(parse_url("https://www.youtube.com/playlist?list=PLabc_-1"), Some(Target::YouTube("PLabc_-1".into())));
        assert_eq!(parse_url("https://evil.example/playlist/abc"), None);
        assert_eq!(parse_url("https://open.spotify.com/album/abc"), None);
        assert_eq!(parse_url("javascript:alert(1)"), None);
        assert_eq!(parse_url("not a url"), None);
    }

    #[test]
    fn spotify_import_paginates_and_skips_local_tracks() {
        let h = harness(all_secrets());
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on("fields=name", 200, r#"{"name":"My List"}"#);
        h.http.on(
            "/tracks?",
            200,
            r#"{"items":[{"track":{"id":"1","name":"A","artists":[{"name":"X"}],"duration_ms":1000}},{"track":{"id":null,"name":"Local","artists":[]}},{"track":null}],"next":null}"#,
        );
        let r = block_on(import(&h.svc, "https://open.spotify.com/playlist/abc")).unwrap();
        assert_eq!(r.name, "My List");
        assert_eq!(r.tracks.len(), 1);
    }

    #[test]
    fn bad_link_is_400_and_private_is_403() {
        let h = harness(all_secrets());
        assert_eq!(block_on(import(&h.svc, "https://example.com")).unwrap_err().status, 400);
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on("fields=name", 403, "{}");
        assert_eq!(block_on(import(&h.svc, "https://open.spotify.com/playlist/abc")).unwrap_err().status, 403);
    }
}
