//! Live key checks with plain-language results. Used by the admin API, the
//! setup wizard and `doctor`. Uses cheap endpoints (YouTube: 1 quota unit).
//! Messages never contain the key.

use crate::config::Secrets;
use crate::providers::build_url;
use crate::traits::{HttpClient, HttpRequest};
use base64::Engine;
use glyph_protocol::ProviderId;

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
}

fn ok(m: impl Into<String>) -> Outcome {
    Outcome { ok: true, message: m.into() }
}
fn bad(m: impl Into<String>) -> Outcome {
    Outcome { ok: false, message: m.into() }
}

async fn get(http: &dyn HttpClient, req: HttpRequest, who: &str) -> Result<(u16, String), Outcome> {
    let req = req.header("user-agent", "glyph-server (key test)");
    match http.send(req).await {
        Ok(r) => Ok((r.status, String::from_utf8_lossy(&r.body).into_owned())),
        Err(_) => Err(bad(format!(
            "Could not reach {who} from this machine. Check the internet connection, DNS and firewall."
        ))),
    }
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
}

pub async fn test_provider(http: &dyn HttpClient, secrets: &Secrets, id: ProviderId) -> Outcome {
    match id {
        ProviderId::YouTube => match secrets.youtube_api_key.as_deref() {
            None => bad("No YouTube API key is set."),
            Some(k) => youtube(http, k).await,
        },
        ProviderId::Jamendo => match secrets.jamendo_client_id.as_deref() {
            None => bad("No Jamendo client id is set."),
            Some(k) => jamendo(http, k).await,
        },
        ProviderId::SoundCloud => match (&secrets.soundcloud_client_id, &secrets.soundcloud_client_secret) {
            (Some(i), Some(s)) => {
                token(http, "https://secure.soundcloud.com/oauth/token", i, s, "SoundCloud").await
            }
            _ => bad("SoundCloud needs both a client id and a client secret."),
        },
        ProviderId::Spotify => match (&secrets.spotify_client_id, &secrets.spotify_client_secret) {
            (Some(i), Some(s)) => token(http, "https://accounts.spotify.com/api/token", i, s, "Spotify").await,
            _ => bad("Spotify needs both a client id and a client secret."),
        },
        ProviderId::LastFm => match secrets.lastfm_api_key.as_deref() {
            None => bad("No Last.fm API key is set."),
            Some(k) => lastfm(http, k).await,
        },
        ProviderId::Audius => reach(http, "https://api.audius.co/v1/tracks/trending?limit=1&app_name=glyph", "Audius").await,
        ProviderId::Deezer => reach(http, "https://api.deezer.com/chart/0/tracks?limit=1", "Deezer").await,
        ProviderId::Archive => reach(http, "https://archive.org/advancedsearch.php?q=mediatype%3Aaudio&rows=1&output=json&fl%5B%5D=identifier", "Internet Archive").await,
        ProviderId::LrcLib => reach(http, "https://lrclib.net/api/search?q=test", "LRCLIB").await,
        ProviderId::MusicBrainz => bad("MusicBrainz is not used by the server."),
    }
}

async fn reach(http: &dyn HttpClient, url: &str, who: &str) -> Outcome {
    match get(http, HttpRequest::get(url), who).await {
        Err(o) => o,
        Ok((s, _)) if (200..300).contains(&s) => ok(format!("{who} is reachable (no key needed).")),
        Ok((s, _)) => bad(format!("{who} answered with HTTP {s}. It may be down; try again later.")),
    }
}

async fn youtube(http: &dyn HttpClient, key: &str) -> Outcome {
    let url = build_url(
        "https://www.googleapis.com/youtube/v3/videos",
        &[("part", "id"), ("id", "dQw4w9WgXcQ"), ("key", key)],
    );
    let (status, body) = match get(http, HttpRequest::get(url), "YouTube").await {
        Ok(v) => v,
        Err(o) => return o,
    };
    if (200..300).contains(&status) {
        return ok("The YouTube key works. Note: one search costs 100 of your 10,000 daily units.");
    }
    let reason = json(&body)["error"]["errors"][0]["reason"].as_str().unwrap_or("").to_string();
    match (status, reason.as_str()) {
        (_, "quotaExceeded") | (_, "dailyLimitExceeded") => {
            ok("The key is valid, but today's YouTube quota is used up. It resets at midnight Pacific time.")
        }
        (403, "accessNotConfigured") | (403, "SERVICE_DISABLED") => bad(
            "Google accepted the key, but the YouTube Data API v3 is not enabled for its project. \
             Open the Google Cloud console, go to APIs & Services, and enable \"YouTube Data API v3\".",
        ),
        (400, _) | (_, "keyInvalid") => bad(
            "Google says this key is not valid. Copy the whole key again (it usually starts with \"AIza\").",
        ),
        (403, _) => bad("Google refused this key. It may be restricted to other websites or IP addresses; remove the restriction or allow this server's IP."),
        (s, _) => bad(format!("YouTube answered with HTTP {s}. Try again later.")),
    }
}

async fn jamendo(http: &dyn HttpClient, id: &str) -> Outcome {
    let url = build_url(
        "https://api.jamendo.com/v3.0/tracks/",
        &[("client_id", id), ("format", "json"), ("limit", "1")],
    );
    let (status, body) = match get(http, HttpRequest::get(url), "Jamendo").await {
        Ok(v) => v,
        Err(o) => return o,
    };
    let v = json(&body);
    if status == 200 && v["headers"]["status"] == "success" {
        ok("The Jamendo client id works.")
    } else if status == 200 || status == 401 || status == 403 {
        bad("Jamendo does not accept this client id. Copy it again from your Jamendo developer dashboard.")
    } else {
        bad(format!("Jamendo answered with HTTP {status}. Try again later."))
    }
}

async fn token(http: &dyn HttpClient, url: &str, id: &str, secret: &str, who: &str) -> Outcome {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let req = HttpRequest::post_form(url, &[("grant_type", "client_credentials")])
        .header("authorization", format!("Basic {basic}"))
        .header("accept", "application/json");
    match get(http, req, who).await {
        Err(o) => o,
        Ok((s, _)) if (200..300).contains(&s) => ok(format!("{who} accepted the client id and secret.")),
        Ok((400, _)) | Ok((401, _)) | Ok((403, _)) => bad(format!(
            "{who} rejected the client id or secret. Check that you copied both completely and did not mix them up."
        )),
        Ok((s, _)) => bad(format!("{who} answered with HTTP {s}. Try again later.")),
    }
}

async fn lastfm(http: &dyn HttpClient, key: &str) -> Outcome {
    let url = build_url(
        "https://ws.audioscrobbler.com/2.0/",
        &[("method", "chart.gettoptracks"), ("limit", "1"), ("api_key", key), ("format", "json")],
    );
    let (status, body) = match get(http, HttpRequest::get(url), "Last.fm").await {
        Ok(v) => v,
        Err(o) => return o,
    };
    let v = json(&body);
    match v["error"].as_i64() {
        None if status == 200 => ok("The Last.fm key works."),
        Some(10) => bad("Last.fm says this API key is invalid. Copy the \"API key\" (not the shared secret) again."),
        Some(26) => bad("Last.fm suspended this API key."),
        Some(29) => ok("The key is valid, but Last.fm is rate limiting it right now."),
        _ => bad(format!("Last.fm answered with HTTP {status}. Try again later.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockHttp;
    use futures::executor::block_on;

    fn secrets(f: impl FnOnce(&mut Secrets)) -> Secrets {
        let mut s = Secrets::default();
        f(&mut s);
        s
    }

    #[test]
    fn youtube_results_are_plain_language_and_leak_nothing() {
        let h = MockHttp::default();
        h.on("videos", 400, r#"{"error":{"errors":[{"reason":"badRequest"}]}}"#);
        let s = secrets(|s| s.youtube_api_key = Some("SECRETKEYVALUE".into()));
        let o = block_on(test_provider(&h, &s, ProviderId::YouTube));
        assert!(!o.ok && o.message.contains("not valid") && !o.message.contains("SECRETKEYVALUE"));

        let h = MockHttp::default();
        h.on("videos", 403, r#"{"error":{"errors":[{"reason":"accessNotConfigured"}]}}"#);
        let o = block_on(test_provider(&h, &s, ProviderId::YouTube));
        assert!(!o.ok && o.message.contains("enable"));

        let h = MockHttp::default();
        h.on("videos", 403, r#"{"error":{"errors":[{"reason":"quotaExceeded"}]}}"#);
        assert!(block_on(test_provider(&h, &s, ProviderId::YouTube)).ok);

        let h = MockHttp::default();
        h.on("videos", 200, "{}");
        assert!(block_on(test_provider(&h, &s, ProviderId::YouTube)).ok);
    }

    #[test]
    fn other_providers() {
        let h = MockHttp::default();
        h.on("jamendo", 200, r#"{"headers":{"status":"failed"}}"#);
        let s = secrets(|s| s.jamendo_client_id = Some("x".into()));
        assert!(!block_on(test_provider(&h, &s, ProviderId::Jamendo)).ok);

        let h = MockHttp::default();
        h.on("accounts.spotify.com", 400, "{}");
        let s = secrets(|s| {
            s.spotify_client_id = Some("a".into());
            s.spotify_client_secret = Some("b".into());
        });
        assert!(!block_on(test_provider(&h, &s, ProviderId::Spotify)).ok);

        let h = MockHttp::default();
        h.on("audioscrobbler", 200, r#"{"error":10,"message":"Invalid API key"}"#);
        let s = secrets(|s| s.lastfm_api_key = Some("k".into()));
        assert!(!block_on(test_provider(&h, &s, ProviderId::LastFm)).ok);

        assert!(!block_on(test_provider(&h, &Secrets::default(), ProviderId::SoundCloud)).ok);
        let h = MockHttp::default();
        h.on("deezer", 200, "{}");
        assert!(block_on(test_provider(&h, &Secrets::default(), ProviderId::Deezer)).ok);
    }
}
