//! `POST /v1/import/spotify`: read the track list of a *public* playlist
//! (Spotify, Deezer, YouTube, SoundCloud link or a Last.fm profile) with the
//! server's keys, so the app needs none. Metadata only; resolving happens
//! per track via `/v1/resolve`. Private libraries are read by the app itself.

use crate::error::ApiErr;
use crate::providers::{self, build_url, fetch_json, new_track, spotify, ProviderError};
use crate::traits::{HttpRequest, Services};
use glyph_protocol::{ImportResponse, ProviderId};

pub const DEFAULT_PLAYLIST_TRACKS: usize = 5_000;
pub const HARD_MAX_PLAYLIST_TRACKS: usize = 50_000;
fn default_max_tracks() -> usize { DEFAULT_PLAYLIST_TRACKS }
pub const IMPORT_PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    pub url: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_max_tracks")]
    pub max_tracks: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PageResponse {
    #[serde(flatten)]
    pub list: ImportResponse,
    pub next_offset: Option<usize>,
}

impl PageRequest {
    pub fn validate(&self) -> Result<(), ApiErr> {
        if self.url.len() > 2048 || self.offset >= self.max_tracks || !(1..=HARD_MAX_PLAYLIST_TRACKS).contains(&self.max_tracks) || self.offset % IMPORT_PAGE_SIZE != 0 || parse_url(&self.url).is_none() {
            return Err(ApiErr::bad_request("invalid import URL or page offset"));
        }
        Ok(())
    }
}

/// Native orchestration: each request reads at most 100 metadata rows.
/// The Worker only proxies and caches this bounded response.
pub async fn page(svc: &Services, req: &PageRequest) -> Result<PageResponse, ApiErr> {
    req.validate()?;
    let canonical = canonical_url(&req.url).ok_or_else(|| ApiErr::bad_request("unsupported import link"))?;
    let key = format!("import-native-page-v3:{}:{}:{}", req.max_tracks, canonical, req.offset);
    if let Some(page) = crate::cache::get_json::<PageResponse>(&*svc.cache, &key).await { return Ok(page); }
    let page = provider_page(svc, req).await?;
    crate::cache::put_json(&*svc.cache, &key, &page, 600).await;
    Ok(page)
}

/// Each request performs bounded native work: at most two provider pages.
/// Only validated offsets or companion-cached opaque cursors are used.
async fn provider_page(svc: &Services, req: &PageRequest) -> Result<PageResponse, ApiErr> {
    async fn read(svc: &Services, request: HttpRequest) -> Result<serde_json::Value, ApiErr> {
        fetch_json(svc, request).await.map_err(map_err)
    }
    fn check_size(total: Option<u64>, limit: usize) -> Result<(), ApiErr> {
        if total.is_some_and(|n| n > limit as u64) {
            return Err(ApiErr::bad_request(format!("playlists may contain at most {limit} songs")));
        }
        Ok(())
    }
    let target = parse_url(&req.url).ok_or_else(|| ApiErr::bad_request("unsupported import link"))?;
    let offset = req.offset.to_string();
    let limit = (req.max_tracks - req.offset).min(IMPORT_PAGE_SIZE);
    let count = limit.to_string();
    let (name, source, tracks, more) = match target {
        Target::Spotify(id) => {
            let tok = spotify::token(svc).await.map_err(map_err)?;
            let (k, v) = spotify::auth(&tok);
            let base = format!("https://api.spotify.com/v1/playlists/{id}");
            let meta = read(svc, HttpRequest::get(build_url(&base, &[("fields", "name,items(total)")])).header(k, v.clone())).await?;
            check_size(meta["items"]["total"].as_u64().or_else(|| meta["tracks"]["total"].as_u64()), req.max_tracks)?;
            let mut tracks = Vec::new();
            let mut more = false;
            for step in (0..limit).step_by(50) {
                let offset = (req.offset + step).to_string();
                let count = (limit - step).min(50).to_string();
                let page = read(svc, HttpRequest::get(build_url(&format!("{base}/items"), &[("limit", &count), ("offset", &offset)])).header(k, v.clone())).await?;
                check_size(page["total"].as_u64(), req.max_tracks)?;
                let rows = page["items"].as_array().ok_or_else(|| ApiErr::unavailable("invalid Spotify page"))?;
                for row in rows {
                    let Some(item) = row.get("item").filter(|t| !t.is_null()).or_else(|| row.get("track").filter(|t| !t.is_null())) else { continue };
                    if item["type"].as_str().is_some_and(|kind| kind != "track") || row["is_local"].as_bool() == Some(true) { continue; }
                    let song: spotify::SpTrack = serde_json::from_value(item.clone()).map_err(|_| ApiErr::unavailable("invalid Spotify track metadata"))?;
                    if let Some(track) = spotify::to_track(song) { tracks.push(track); }
                }
                more = page["next"].as_str().is_some();
                if !more { break; }
            }
            (meta["name"].as_str().unwrap_or("Spotify playlist").to_string(), ProviderId::Spotify, tracks, more)
        }
        Target::Deezer(id) | Target::DeezerUser(id) => {
            let favorites = matches!(parse_url(&req.url), Some(Target::DeezerUser(_)));
            let base = if favorites { format!("https://api.deezer.com/user/{id}") } else { format!("https://api.deezer.com/playlist/{id}") };
            let name = if favorites { format!("Deezer favorites ({id})") } else {
                let meta = read(svc, HttpRequest::get(base.clone())).await?;
                check_size(meta["nb_tracks"].as_u64(), req.max_tracks)?;
                meta["title"].as_str().unwrap_or("Deezer playlist").to_string()
            };
            let page = read(svc, HttpRequest::get(build_url(&format!("{base}/tracks"), &[("limit", &count), ("index", &offset)]))).await?;
            check_size(page["total"].as_u64(), req.max_tracks)?;
            let rows = page["data"].as_array().ok_or_else(|| ApiErr::unavailable("invalid Deezer page"))?;
            let tracks = rows.iter().filter_map(|t| serde_json::from_value::<providers::deezer::DzTrack>(t.clone()).ok()).map(|t| providers::deezer::to_candidate(t).track).collect();
            (name, ProviderId::Deezer, tracks, page["next"].as_str().is_some())
        }
        Target::LastFm(user) => {
            let key = svc.config.secrets.lastfm_api_key.as_deref().ok_or_else(|| map_err(ProviderError::NotConfigured))?;
            let page_num = (req.offset / IMPORT_PAGE_SIZE + 1).to_string();
            let page = read(svc, HttpRequest::get(build_url("https://ws.audioscrobbler.com/2.0/", &[("method", "user.getlovedtracks"), ("api_key", key), ("format", "json"), ("user", &user), ("limit", "100"), ("page", &page_num)]))).await?;
            if page["error"].is_number() { return Err(ApiErr::unavailable("Last.fm import failed")); }
            let list = &page["lovedtracks"];
            check_size(list["@attr"]["total"].as_str().and_then(|n| n.parse().ok()), req.max_tracks)?;
            let rows = list["track"].as_array().cloned().unwrap_or_else(|| if list["track"].is_object() { vec![list["track"].clone()] } else { vec![] });
            let tracks = rows.iter().filter_map(|t| {
                let title = t["name"].as_str()?;
                let artist = t["artist"]["name"].as_str().or_else(|| t["artist"].as_str()).unwrap_or("");
                Some(new_track(format!("{artist}|{title}"), ProviderId::LastFm, title, artist))
            }).collect();
            let pages = list["@attr"]["totalPages"].as_str().and_then(|n| n.parse::<usize>().ok()).unwrap_or(1);
            (format!("{user} (Last.fm loved)"), ProviderId::LastFm, tracks, req.offset / IMPORT_PAGE_SIZE + 1 < pages)
        }
        Target::YouTube(id) => {
            let key = svc.config.secrets.youtube_api_key.as_deref().ok_or_else(|| map_err(ProviderError::NotConfigured))?;
            let canonical = canonical_url(&req.url).ok_or_else(|| ApiErr::bad_request("unsupported import link"))?;
            let cursor_key = |offset: usize| format!("import-cursor-v3:{}:{}:{offset}", req.max_tracks, canonical);
            let mut token = if req.offset == 0 { None } else {
                Some(crate::cache::get_json::<String>(&*svc.cache, &cursor_key(req.offset)).await.ok_or_else(|| ApiErr::new(409, "import_expired", "import cursor expired; restart the import"))?)
            };
            let meta = read(svc, HttpRequest::get(build_url("https://www.googleapis.com/youtube/v3/playlists", &[("part", "snippet,contentDetails"), ("id", &id), ("key", key)]))).await?;
            let item = &meta["items"][0];
            if !item.is_object() { return Err(ApiErr::not_found("playlist not found or not public")); }
            check_size(item["contentDetails"]["itemCount"].as_u64(), req.max_tracks)?;
            let name = item["snippet"]["title"].as_str().unwrap_or("YouTube playlist").to_string();
            let mut tracks = Vec::new();
            for _ in 0..2 {
                let page_size = (limit - tracks.len()).min(50).to_string();
                if page_size == "0" { break; }
                let mut params = vec![("part", "snippet"), ("playlistId", &id), ("maxResults", &page_size), ("key", key)];
                if let Some(t) = token.as_deref() { params.push(("pageToken", t)); }
                let page = read(svc, HttpRequest::get(build_url("https://www.googleapis.com/youtube/v3/playlistItems", &params))).await?;
                check_size(page["pageInfo"]["totalResults"].as_u64(), req.max_tracks)?;
                let rows = page["items"].as_array().ok_or_else(|| ApiErr::unavailable("invalid YouTube page"))?;
                for row in rows {
                    let s = &row["snippet"];
                    let Some(id) = s["resourceId"]["videoId"].as_str() else { continue };
                    let title = s["title"].as_str().unwrap_or("");
                    if matches!(title, "Private video" | "Deleted video" | "") { continue; }
                    let artist = s["videoOwnerChannelTitle"].as_str().unwrap_or("").trim_end_matches(" - Topic");
                    let mut track = new_track(id, ProviderId::YouTube, title, artist);
                    track.permalink = Some(format!("https://www.youtube.com/watch?v={id}"));
                    tracks.push(track);
                }
                token = page["nextPageToken"].as_str().map(str::to_string);
                if token.is_none() { break; }
            }
            if let Some(t) = &token { crate::cache::put_json(&*svc.cache, &cursor_key(req.offset + IMPORT_PAGE_SIZE), t, 600).await; }
            (name, ProviderId::YouTube, tracks, token.is_some())
        }
        Target::SoundCloud(segs) => return providers::soundcloud::import_page(svc, &req.url, &segs, req.offset, req.max_tracks).await.map_err(map_err),
    };
    if tracks.len() > limit || (more && req.offset + IMPORT_PAGE_SIZE >= req.max_tracks) {
        return Err(ApiErr::bad_request(format!("playlists may contain at most {} songs", req.max_tracks)));
    }
    Ok(PageResponse { list: ImportResponse { name, source, tracks }, next_offset: more.then_some(req.offset + IMPORT_PAGE_SIZE) })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Spotify(String),
    Deezer(String),
    DeezerUser(String),
    LastFm(String),
    YouTube(String),
    /// soundcloud.com link: path segments (set, likes, tracks, one song).
    SoundCloud(Vec<String>),
}

/// soundcloud.com paths that are pages of the site, not music.
const SOUNDCLOUD_RESERVED: [&str; 14] = [
    "discover", "search", "you", "stream", "upload", "settings", "messages", "notifications", "charts", "pages", "mobile", "imprint", "terms-of-use", "pro",
];

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn parse_url(raw: &str) -> Option<Target> {
    let raw = raw.trim();
    if raw.len() > 2048 { return None; }
    if let Some(id) = raw.strip_prefix("spotify:playlist:") {
        return valid_id(id).then(|| Target::Spotify(id.to_string()));
    }
    let u = url::Url::parse(raw).ok()?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some() || u.port().is_some() {
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
        "soundcloud.com" | "m.soundcloud.com" => {
            let segs: Vec<String> = segs.iter().filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
            let first = segs.first()?;
            (!SOUNDCLOUD_RESERVED.contains(&first.as_str()) && segs.len() <= 3 && segs.iter().all(|s| valid_id(s))).then_some(Target::SoundCloud(segs))
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

/// Strip share/tracking parameters before cache lookup; varying a client URL
/// must not force another expensive read of the same provider collection.
pub fn canonical_url(raw: &str) -> Option<String> {
    Some(match parse_url(raw)? {
        Target::Spotify(id) => format!("spotify:playlist:{id}"),
        Target::Deezer(id) => format!("https://www.deezer.com/playlist/{id}"),
        Target::DeezerUser(id) => format!("https://www.deezer.com/profile/{id}"),
        Target::LastFm(user) => format!("https://www.last.fm/user/{user}"),
        Target::YouTube(id) => format!("https://www.youtube.com/playlist?list={id}"),
        Target::SoundCloud(segs) => format!("https://soundcloud.com/{}", segs.join("/")),
    })
}

fn map_err(e: ProviderError) -> ApiErr {
    match e {
        ProviderError::Http(404) => ApiErr::not_found("playlist not found or not public"),
        ProviderError::Http(401) | ProviderError::Http(403) => {
            ApiErr::new(403, "playlist_not_accessible", "the provider denied playlist access; use an authorized account or import a playlist file")
        }
        ProviderError::NotConfigured => ApiErr::unavailable("this server has no key for that provider"),
        other => ApiErr::unavailable(format!("provider error: {other}")),
    }
}

/// Native callers that need a complete list use the same bounded page reader.
pub async fn import(svc: &Services, url: &str) -> Result<ImportResponse, ApiErr> {
    let mut req = PageRequest { url: url.to_string(), offset: 0, max_tracks: svc.config.playlist_max_tracks };
    let mut page = page(svc, &req).await?;
    let mut result = page.list;
    while let Some(offset) = page.next_offset {
        req.offset = offset;
        page = self::page(svc, &req).await?;
        result.tracks.extend(page.list.tracks);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn page_offsets_and_urls_are_server_validated() {
        for offset in [1, 99, 5000, usize::MAX] {
            assert!(PageRequest { url: "spotify:playlist:abc".into(), offset, max_tracks: 5000 }.validate().is_err());
        }
        for offset in [0, 100, 4900] {
            assert!(PageRequest { url: "spotify:playlist:abc".into(), offset, max_tracks: 5000 }.validate().is_ok());
        }
        for url in ["https://localhost/playlist/a", "https://user:secret@open.spotify.com/playlist/a", "https://open.spotify.com:444/playlist/a"] {
            assert!(PageRequest { url: url.into(), offset: 0, max_tracks: 5000 }.validate().is_err());
        }
    }

    #[test]
    fn companion_reads_bounded_spotify_pages_and_caches_retries() {
        let h = harness(all_secrets());
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on("fields=name", 200, r#"{"name":"Full list","tracks":{"total":201}}"#);
        h.http.on("offset=50", 200, r#"{"items":[],"next":"ignored"}"#);
        h.http.on("offset=150", 200, r#"{"items":[],"next":"ignored"}"#);
        h.http.on("offset=100", 200, r#"{"items":[{"item":{"id":"second","name":"B","artists":[{"name":"X"}]}}],"next":"ignored"}"#);
        h.http.on("/items?", 200, r#"{"items":[{"track":{"id":"first","name":"A","artists":[{"name":"X"}]}},{"track":null}],"next":"ignored"}"#);
        let mut req = PageRequest { url: "spotify:playlist:abc".into(), offset: 0, max_tracks: 5000 };
        let r = block_on(page(&h.svc, &req)).unwrap();
        assert_eq!(r.list.tracks[0].id, "first");
        assert_eq!(r.next_offset, Some(100));
        block_on(page(&h.svc, &req)).unwrap();
        assert_eq!(h.http.call_count("/items?"), 2);
        let shared_link = PageRequest { url: "https://open.spotify.com/playlist/abc?si=another-share-id".into(), ..req.clone() };
        block_on(page(&h.svc, &shared_link)).unwrap();
        assert_eq!(h.http.call_count("/items?"), 2, "share parameters must reuse the native page cache");
        req.offset = 100;
        let r = block_on(page(&h.svc, &req)).unwrap();
        assert_eq!(r.list.tracks[0].id, "second");
        assert_eq!(r.next_offset, Some(200));
        assert_eq!(h.http.call_count("/items?"), 4);
    }

    #[test]
    fn oversized_provider_playlist_is_rejected_before_fetching_tracks() {
        let h = harness(all_secrets());
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on("fields=name", 200, r#"{"name":"Too large","items":{"total":6000}}"#);
        let req = PageRequest { url: "spotify:playlist:abc".into(), offset: 0, max_tracks: 5000 };
        assert_eq!(block_on(page(&h.svc, &req)).unwrap_err().status, 400);
        assert_eq!(h.http.call_count("/items?"), 0);
    }

    #[test]
    fn deezer_import_reads_the_tracks_endpoint_beyond_the_embedded_preview() {
        let h = harness(all_secrets());
        h.http.on("index=100", 200, r#"{"data":[{"id":2,"title":"Second"}],"total":101}"#);
        h.http.on("/tracks?", 200, r#"{"data":[{"id":1,"title":"First"}],"total":101,"next":"ignored"}"#);
        h.http.on("playlist/1", 200, r#"{"title":"Full list","nb_tracks":101,"tracks":{"data":[]}}"#);
        let list = block_on(import(&h.svc, "https://www.deezer.com/playlist/1")).unwrap();
        assert_eq!(list.tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["First", "Second"]);
        assert_eq!(h.http.call_count("/tracks?"), 2);
    }

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
        assert_eq!(parse_url("https://soundcloud.com/odesza/likes"), Some(Target::SoundCloud(vec!["odesza".into(), "likes".into()])));
        assert_eq!(parse_url("https://soundcloud.com/discover"), None);
        assert_eq!(parse_url("javascript:alert(1)"), None);
        assert_eq!(parse_url("not a url"), None);
    }

    #[test]
    fn spotify_import_paginates_and_skips_local_tracks() {
        let h = harness(all_secrets());
        h.http.on("api/token", 200, r#"{"access_token":"T","expires_in":3600}"#);
        h.http.on("fields=name", 200, r#"{"name":"My List"}"#);
        h.http.on(
            "/items?",
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
