//! Upstream providers. Official APIs only; keys come from [`Config`] and are
//! never copied into responses, errors or logs.

pub mod archive;
pub mod audius;
pub mod deezer;
pub mod jamendo;
pub mod lastfm;
pub mod lrclib;
pub mod soundcloud;
pub mod spotify;
pub mod youtube;

use crate::traits::{HttpRequest, Services};
use glyph_protocol::{PlaybackSource, ProviderId, Track};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;

/// A search hit plus resolver hints that are not part of the wire `Track`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub track: Track,
    /// Extra score for "Official Audio", YouTube "Topic" channels, ...
    #[serde(default)]
    pub bonus: f32,
    /// How to play this exact hit, when known without another request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback: Option<PlaybackSource>,
}

/// Why a provider call failed. Display strings are safe to send to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderError {
    NotConfigured,
    Skipped,
    Quota,
    Http(u16),
    Network,
    Parse,
    Timeout,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("provider not configured"),
            Self::Skipped => f.write_str("temporarily skipped after an earlier error"),
            Self::Quota => f.write_str("quota exhausted or rate limited"),
            Self::Http(s) => write!(f, "upstream returned HTTP {s}"),
            Self::Network => f.write_str("network error"),
            Self::Parse => f.write_str("unexpected upstream response"),
            Self::Timeout => f.write_str("timed out"),
        }
    }
}

pub type PResult<T> = Result<T, ProviderError>;

pub(crate) fn user_agent(svc: &Services) -> String {
    format!(
        "glyph-server/{} (+https://github.com/glyphmusic/glyph-server)",
        svc.config.server_version
    )
}

/// Build `base?k=v&...` with proper encoding.
pub(crate) fn build_url(base: &str, params: &[(&str, &str)]) -> String {
    match url::Url::parse_with_params(base, params) {
        Ok(u) => u.into(),
        Err(_) => base.to_string(),
    }
}

pub(crate) async fn fetch(svc: &Services, req: HttpRequest) -> PResult<Vec<u8>> {
    let req = req.header("user-agent", user_agent(svc));
    let resp = svc.http.send(req).await.map_err(|_| ProviderError::Network)?;
    match resp.status {
        200..=299 => Ok(resp.body),
        429 => Err(ProviderError::Quota),
        s => Err(ProviderError::Http(s)),
    }
}

pub(crate) async fn fetch_json<T: DeserializeOwned>(svc: &Services, req: HttpRequest) -> PResult<T> {
    let body = fetch(svc, req).await?;
    serde_json::from_slice(&body).map_err(|_| ProviderError::Parse)
}

pub(crate) fn new_track(id: impl Into<String>, source: ProviderId, title: &str, artist: &str) -> Track {
    Track {
        id: id.into(),
        source,
        title: title.trim().to_string(),
        artist: artist.trim().to_string(),
        album: None,
        duration_ms: None,
        cover_url: None,
        isrc: None,
        permalink: None,
        external_ids: BTreeMap::new(),
        explicit: false,
    }
}

pub(crate) fn candidate(track: Track, playback: Option<PlaybackSource>) -> Candidate {
    Candidate { track, bonus: 0.0, playback }
}

/// Providers that can return further result pages (`page` > 0).
pub fn pages(id: ProviderId) -> bool {
    matches!(id, ProviderId::Deezer | ProviderId::Audius | ProviderId::Jamendo | ProviderId::Archive)
}

/// Search one provider (no caching, no timeout; see `search::search_provider`).
/// `page` 0 is the first page; later pages only from providers with [`pages`].
pub async fn search(svc: &Services, id: ProviderId, q: &str, page: usize) -> PResult<Vec<Candidate>> {
    if page > 0 {
        return match id {
            ProviderId::Audius => audius::search_page(svc, q, page).await,
            ProviderId::Jamendo => jamendo::search_page(svc, q, page).await,
            ProviderId::Archive => archive::search_page(svc, q, page).await,
            ProviderId::Deezer => deezer::search_page(svc, q, page).await,
            _ => Ok(vec![]),
        };
    }
    match id {
        ProviderId::Audius => audius::search(svc, q).await,
        ProviderId::Jamendo => jamendo::search(svc, q).await,
        ProviderId::Archive => archive::search(svc, q).await,
        ProviderId::SoundCloud => soundcloud::search(svc, q).await,
        ProviderId::YouTube => youtube::search(svc, q).await,
        ProviderId::Spotify => spotify::search(svc, q).await,
        ProviderId::Deezer => deezer::search(svc, q).await,
        _ => Err(ProviderError::NotConfigured),
    }
}

/// Playback for a known provider-local id (no search needed).
pub async fn playback_for(svc: &Services, id: ProviderId, track_id: &str) -> PResult<PlaybackSource> {
    match id {
        ProviderId::Audius => Ok(audius::playback(svc, track_id)),
        ProviderId::YouTube => Ok(youtube::playback(track_id)),
        ProviderId::SoundCloud => Ok(soundcloud::playback(track_id)),
        ProviderId::Jamendo => jamendo::playback(svc, track_id).await,
        ProviderId::Archive => archive::playback(svc, track_id).await,
        _ => Err(ProviderError::NotConfigured),
    }
}

/// Providers that can serve complete songs.
pub fn is_full_source(id: ProviderId) -> bool {
    matches!(
        id,
        ProviderId::Audius
            | ProviderId::Jamendo
            | ProviderId::Archive
            | ProviderId::SoundCloud
            | ProviderId::YouTube
    )
}

/// Cache TTL for search results of a provider (seconds).
pub fn search_ttl(id: ProviderId) -> u64 {
    match id {
        // Each YouTube search is a yt-dlp run on the companion machine. Under
        // an hour, so the Worker keeps it in the Cache API and not in KV
        // (MinTtlCache, 1,000 KV writes/day on the free plan).
        ProviderId::YouTube => 55 * 60,
        _ => 15 * 60,
    }
}

/// Provider-sent JSON sometimes uses numbers or strings for the same field.
pub(crate) fn de_u64_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        U(u64),
        F(f64),
        S(String),
        Null,
    }
    Ok(match N::deserialize(d)? {
        N::U(v) => Some(v),
        N::F(v) if v >= 0.0 => Some(v as u64),
        N::S(s) => s.trim().parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| v as u64),
        _ => None,
    })
}
