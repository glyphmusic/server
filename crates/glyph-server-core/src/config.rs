use glyph_protocol::ProviderId;

/// Server-held provider credentials. Never serialized, never logged.
#[derive(Clone, Default)]
pub struct Secrets {
    pub youtube_api_key: Option<String>,
    pub jamendo_client_id: Option<String>,
    pub soundcloud_client_id: Option<String>,
    pub soundcloud_client_secret: Option<String>,
    pub spotify_client_id: Option<String>,
    pub spotify_client_secret: Option<String>,
    pub lastfm_api_key: Option<String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |o: &Option<String>| if o.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("Secrets")
            .field("youtube_api_key", &set(&self.youtube_api_key))
            .field("jamendo_client_id", &set(&self.jamendo_client_id))
            .field("soundcloud_client_id", &set(&self.soundcloud_client_id))
            .field("soundcloud_client_secret", &set(&self.soundcloud_client_secret))
            .field("spotify_client_id", &set(&self.spotify_client_id))
            .field("spotify_client_secret", &set(&self.spotify_client_secret))
            .field("lastfm_api_key", &set(&self.lastfm_api_key))
            .finish()
    }
}

impl Secrets {
    /// Treat empty / whitespace-only values as unset and trim the rest.
    pub fn normalized(mut self) -> Self {
        for f in [
            &mut self.youtube_api_key,
            &mut self.jamendo_client_id,
            &mut self.soundcloud_client_id,
            &mut self.soundcloud_client_secret,
            &mut self.spotify_client_id,
            &mut self.spotify_client_secret,
            &mut self.lastfm_api_key,
        ] {
            if let Some(s) = f.as_mut() {
                *s = s.trim().to_string();
            }
            if f.as_deref() == Some("") {
                *f = None;
            }
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub secrets: Secrets,
    pub server_version: String,
    /// Requests per minute per install id (per IP the limit is 4x, for NAT).
    pub rate_limit_per_min: u32,
    pub provider_timeout_ms: u64,
    /// Stop calling YouTube once this many quota units were spent today.
    pub youtube_daily_budget_units: u64,
    /// Max pages fetched per playlist import (Workers free plan: 50 subrequests).
    pub import_max_pages: u32,
    pub audius_app_name: String,
}

impl Config {
    pub fn new(secrets: Secrets) -> Self {
        Self {
            secrets: secrets.normalized(),
            server_version: crate::SERVER_VERSION.to_string(),
            rate_limit_per_min: 60,
            provider_timeout_ms: 4000,
            youtube_daily_budget_units: 9000,
            import_max_pages: 5,
            audius_app_name: "glyph".to_string(),
        }
    }

    pub fn provider_enabled(&self, id: ProviderId) -> bool {
        let s = &self.secrets;
        match id {
            ProviderId::Audius | ProviderId::Archive | ProviderId::Deezer | ProviderId::LrcLib => true,
            ProviderId::Jamendo => s.jamendo_client_id.is_some(),
            ProviderId::SoundCloud => {
                s.soundcloud_client_id.is_some() && s.soundcloud_client_secret.is_some()
            }
            ProviderId::YouTube => s.youtube_api_key.is_some(),
            ProviderId::Spotify => s.spotify_client_id.is_some() && s.spotify_client_secret.is_some(),
            ProviderId::LastFm => s.lastfm_api_key.is_some(),
            // MusicBrainz is not served by this server.
            ProviderId::MusicBrainz => false,
        }
    }

    pub fn enabled_providers(&self) -> Vec<ProviderId> {
        ProviderId::ALL.into_iter().filter(|p| self.provider_enabled(*p)).collect()
    }

    /// Providers queried by `/v1/search`.
    pub fn search_providers(&self) -> Vec<ProviderId> {
        use ProviderId::*;
        [Audius, Jamendo, Archive, SoundCloud, YouTube, Spotify, Deezer]
            .into_iter()
            .filter(|p| self.provider_enabled(*p))
            .collect()
    }
}
