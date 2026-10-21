//! SoundCloud: official API (client-credentials token), playback via the
//! official embed widget only (the track must be marked streamable/playable).

use super::*;
use crate::cache;
use crate::traits::HttpRequest;
use base64::Engine;

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Fetch (and cache) an OAuth client-credentials token. Shared with Spotify.
pub(super) async fn cached_token(
    svc: &Services,
    cache_key: &str,
    token_url: &str,
    id: &str,
    secret: &str,
) -> PResult<String> {
    if let Some(t) = cache::get_json::<String>(&*svc.cache, cache_key).await {
        return Ok(t);
    }
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let req = HttpRequest::post_form(token_url, &[("grant_type", "client_credentials")])
        .header("authorization", format!("Basic {basic}"))
        .header("accept", "application/json");
    let tok: TokenResp = fetch_json(svc, req).await?;
    let ttl = tok.expires_in.unwrap_or(3600).saturating_sub(120).max(60);
    cache::put_json(&*svc.cache, cache_key, &tok.access_token, ttl).await;
    Ok(tok.access_token)
}

async fn token(svc: &Services) -> PResult<String> {
    let s = &svc.config.secrets;
    let (id, secret) = match (&s.soundcloud_client_id, &s.soundcloud_client_secret) {
        (Some(i), Some(s)) => (i, s),
        _ => return Err(ProviderError::NotConfigured),
    };
    cached_token(svc, "token:soundcloud", "https://secure.soundcloud.com/oauth/token", id, secret).await
}

#[derive(Deserialize)]
struct Item {
    id: u64,
    title: String,
    user: Option<User>,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    duration: Option<u64>,
    artwork_url: Option<String>,
    permalink_url: Option<String>,
    streamable: Option<bool>,
    access: Option<String>,
    isrc: Option<String>,
}
#[derive(Deserialize)]
struct User {
    #[serde(default)]
    username: String,
}

/// Embed id = numeric track id; the app builds the widget URL from it.
pub fn playback(id: &str) -> PlaybackSource {
    PlaybackSource::Embed { provider: ProviderId::SoundCloud, embed_id: id.to_string() }
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    let tok = token(svc).await?;
    let url = build_url("https://api.soundcloud.com/tracks", &[("q", q), ("limit", "15"), ("access", "playable")]);
    let items: Vec<Item> = fetch_json(svc, HttpRequest::get(url).header("authorization", format!("OAuth {tok}"))).await?;
    Ok(items
        .into_iter()
        .filter(|i| i.streamable.unwrap_or(true) && i.access.as_deref().map(|a| a == "playable").unwrap_or(true))
        .map(|i| {
            let mut t = new_track(
                i.id.to_string(),
                ProviderId::SoundCloud,
                &i.title,
                i.user.as_ref().map(|u| u.username.as_str()).unwrap_or(""),
            );
            t.duration_ms = i.duration;
            t.cover_url = i.artwork_url;
            t.permalink = i.permalink_url;
            t.isrc = i.isrc.filter(|s| !s.is_empty());
            let pb = playback(&t.id);
            candidate(t, Some(pb))
        })
        .collect())
}

/// Import: a playable song keeps SoundCloud as its source (the embed plays
/// it), others are metadata only and get matched elsewhere by the app.
fn import_track(v: &serde_json::Value) -> Option<Track> {
    let v = if v["track"].is_object() { &v["track"] } else { v };
    let i: Item = serde_json::from_value(v.clone()).ok()?;
    let playable = i.streamable.unwrap_or(true) && i.access.as_deref().map(|a| a == "playable").unwrap_or(true);
    let (id, source) = if playable { (i.id.to_string(), ProviderId::SoundCloud) } else { (format!("sc:{}", i.id), ProviderId::MusicBrainz) };
    let mut t = new_track(id, source, &i.title, i.user.as_ref().map(|u| u.username.as_str()).unwrap_or(""));
    t.duration_ms = i.duration;
    t.cover_url = i.artwork_url.map(|u| u.replace("-large.", "-t500x500."));
    t.permalink = i.permalink_url;
    t.isrc = i.isrc.filter(|s| !s.is_empty());
    Some(t)
}

/// Every song of a paged list (`linked_partitioning`, `next_href`).
async fn import_paged(svc: &Services, tok: &str, first: String) -> PResult<Vec<Track>> {
    let mut out = Vec::new();
    let mut url = first;
    for page in 0..=svc.config.playlist_max_tracks / 200 {
        let v: serde_json::Value = fetch_json(svc, HttpRequest::get(url.clone()).header("authorization", format!("OAuth {tok}"))).await?;
        let items = v["collection"].as_array().cloned().or_else(|| v.as_array().cloned()).unwrap_or_default();
        out.extend(items.iter().filter_map(import_track));
        if out.len() > svc.config.playlist_max_tracks { return Err(ProviderError::Parse); }
        match v["next_href"].as_str().filter(|s| !s.is_empty()) {
            Some(n) => {
                let next = url::Url::parse(n).map_err(|_| ProviderError::Parse)?;
                if page == svc.config.playlist_max_tracks / 200 || next.scheme() != "https" || next.host_str() != Some("api.soundcloud.com")
                    || next.port_or_known_default() != Some(443) || !next.username().is_empty() || next.password().is_some() {
                    return Err(ProviderError::Parse);
                }
                url = next.into();
            },
            None => break,
        }
    }
    Ok(out)
}

/// Bounded Companion import. Continuation URLs stay in the native cache;
/// the public client can only request a validated integer offset.
pub(crate) async fn import_page(svc: &Services, link: &str, segs: &[String], offset: usize, max_tracks: usize) -> PResult<crate::import::PageResponse> {
    use crate::import::{PageResponse, IMPORT_PAGE_SIZE};
    let tok = token(svc).await?;
    let likes = segs.get(1).is_some_and(|s| s == "likes");
    let user_tracks = segs.len() == 1 || segs.get(1).is_some_and(|s| s == "tracks");
    let target = if likes || user_tracks { format!("https://soundcloud.com/{}", segs[0]) } else { link.to_string() };
    let meta: serde_json::Value = fetch_json(svc, HttpRequest::get(build_url("https://api.soundcloud.com/resolve", &[("url", &target)])).header("authorization", format!("OAuth {tok}"))).await?;
    if meta["kind"] == "track" {
        if offset != 0 { return Err(ProviderError::Parse); }
        let t = import_track(&meta).ok_or(ProviderError::Parse)?;
        return Ok(PageResponse { list: glyph_protocol::ImportResponse { name: t.title.clone(), source: ProviderId::SoundCloud, tracks: vec![t] }, next_offset: None });
    }
    if meta["kind"] == "playlist" && meta["track_count"].as_u64().is_some_and(|n| n > max_tracks as u64) { return Err(ProviderError::Parse); }
    let urn = meta["urn"].as_str().map(str::to_string).or_else(|| meta["id"].as_u64().map(|n| n.to_string())).ok_or(ProviderError::Parse)?;
    let name = meta["title"].as_str().or_else(|| meta["username"].as_str()).unwrap_or("SoundCloud").to_string();
    let cursor_key = |offset: usize| format!("sc-import-next:{max_tracks}:{link}:{offset}");
    let limit = (max_tracks - offset).min(IMPORT_PAGE_SIZE);
    let first = if offset == 0 {
        let path = match meta["kind"].as_str() {
            Some("playlist") => format!("/playlists/{urn}/tracks"),
            Some("user") if likes => format!("/users/{urn}/likes/tracks"),
            Some("user") => format!("/users/{urn}/tracks"),
            _ => return Err(ProviderError::Parse),
        };
        build_url(&format!("https://api.soundcloud.com{path}"), &[("linked_partitioning", "true"), ("limit", &limit.to_string())])
    } else {
        cache::get_json::<String>(&*svc.cache, &cursor_key(offset)).await.ok_or(ProviderError::Parse)?
    };
    let next = url::Url::parse(&first).map_err(|_| ProviderError::Parse)?;
    if next.scheme() != "https" || next.host_str() != Some("api.soundcloud.com") || next.port_or_known_default() != Some(443) || !next.username().is_empty() || next.password().is_some() { return Err(ProviderError::Parse); }
    let page: serde_json::Value = fetch_json(svc, HttpRequest::get(first).header("authorization", format!("OAuth {tok}"))).await?;
    let rows = page["collection"].as_array().or_else(|| page.as_array()).ok_or(ProviderError::Parse)?;
    let tracks: Vec<_> = rows.iter().filter_map(import_track).collect();
    let next = page["next_href"].as_str().filter(|s| !s.is_empty());
    if tracks.len() > limit || (next.is_some() && offset + IMPORT_PAGE_SIZE >= max_tracks) { return Err(ProviderError::Parse); }
    let next_offset = next.map(|_| offset + IMPORT_PAGE_SIZE);
    if let Some(next) = next { cache::put_json(&*svc.cache, &cursor_key(offset + IMPORT_PAGE_SIZE), &next, 600).await; }
    Ok(PageResponse { list: glyph_protocol::ImportResponse { name, source: ProviderId::SoundCloud, tracks }, next_offset })
}

/// Name and songs of a soundcloud.com link: a set, a user's likes
/// (`/<user>/likes`), a user's tracks (`/<user>`, `/<user>/tracks`) or one song.
pub async fn import_link(svc: &Services, url: &str, segs: &[String]) -> PResult<(String, Vec<Track>)> {
    let tok = token(svc).await?;
    let likes = segs.get(1).is_some_and(|s| s == "likes");
    let user_tracks = segs.len() == 1 || segs.get(1).is_some_and(|s| s == "tracks");
    let target = if likes || user_tracks { format!("https://soundcloud.com/{}", segs[0]) } else { url.to_string() };
    let v: serde_json::Value = fetch_json(
        svc,
        HttpRequest::get(build_url("https://api.soundcloud.com/resolve", &[("url", &target)])).header("authorization", format!("OAuth {tok}")),
    )
    .await?;
    let urn = v["urn"].as_str().map(str::to_string).or_else(|| v["id"].as_u64().map(|n| n.to_string())).ok_or(ProviderError::Parse)?;
    let page = |path: String| build_url(&format!("https://api.soundcloud.com{path}"), &[("linked_partitioning", "true"), ("limit", "200")]);
    match v["kind"].as_str().unwrap_or("") {
        "track" => {
            let t = import_track(&v).ok_or(ProviderError::Parse)?;
            Ok((t.title.clone(), vec![t]))
        }
        "playlist" => {
            let name = v["title"].as_str().unwrap_or("SoundCloud set").to_string();
            Ok((name, import_paged(svc, &tok, page(format!("/playlists/{urn}/tracks"))).await?))
        }
        "user" => {
            let user = v["username"].as_str().unwrap_or("SoundCloud").to_string();
            if likes {
                Ok((format!("{user} · likes"), import_paged(svc, &tok, page(format!("/users/{urn}/likes/tracks"))).await?))
            } else {
                Ok((format!("{user} · tracks"), import_paged(svc, &tok, page(format!("/users/{urn}/tracks"))).await?))
            }
        }
        _ => Err(ProviderError::Parse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn import_likes_of_a_profile() {
        let h = harness(all_secrets());
        h.http.on("oauth/token", 200, r#"{"access_token":"TOK","expires_in":3600}"#);
        h.http.on("resolve", 200, r#"{"kind":"user","id":42,"username":"odesza"}"#);
        h.http.on(
            "likes/tracks",
            200,
            r#"{"collection":[{"id":5,"title":"S","user":{"username":"U"},"duration":123000,"streamable":true,"access":"playable"},
                {"id":6,"title":"Blocked","user":{"username":"U"},"access":"blocked"}],"next_href":null}"#,
        );
        let segs = vec!["odesza".to_string(), "likes".to_string()];
        let (name, t) = block_on(import_link(&h.svc, "https://soundcloud.com/odesza/likes", &segs)).unwrap();
        assert_eq!(name, "odesza · likes");
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].id.as_str(), t[0].source), ("5", ProviderId::SoundCloud));
        assert_eq!((t[1].id.as_str(), t[1].source), ("sc:6", ProviderId::MusicBrainz));
    }

    #[test]
    fn token_is_cached_and_not_leaked() {
        let h = harness(all_secrets());
        h.http.on("oauth/token", 200, r#"{"access_token":"TOK","expires_in":3600}"#);
        h.http.on(
            "api.soundcloud.com/tracks",
            200,
            r#"[{"id":5,"title":"S","user":{"username":"U"},"duration":123000,"streamable":true,"access":"playable"},
                {"id":6,"title":"Snippet","user":{"username":"U"},"duration":30000,"streamable":true,"access":"preview"}]"#,
        );
        let a = block_on(search(&h.svc, "s")).unwrap();
        let _ = block_on(search(&h.svc, "s2")).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].track.id, "5");
        assert_eq!(h.http.call_count("oauth/token"), 1);
        assert!(!format!("{:?}", a).contains("TOK"));
    }
}
