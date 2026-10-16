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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

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
