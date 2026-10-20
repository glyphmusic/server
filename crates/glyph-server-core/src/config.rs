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

/// Key names accepted by the admin API / `server.toml` / Worker secrets
/// (the latter in upper case), with the provider each belongs to.
pub const KEY_NAMES: [(&str, ProviderId); 7] = [
    ("youtube_api_key", ProviderId::YouTube),
    ("jamendo_client_id", ProviderId::Jamendo),
    ("soundcloud_client_id", ProviderId::SoundCloud),
    ("soundcloud_client_secret", ProviderId::SoundCloud),
    ("spotify_client_id", ProviderId::Spotify),
    ("spotify_client_secret", ProviderId::Spotify),
    ("lastfm_api_key", ProviderId::LastFm),
];

/// Show only the last 4 characters, and only for keys long enough that this
/// reveals little; shorter values are fully hidden.
pub fn mask_secret(s: &str) -> String {
    let n = s.chars().count();
    if n >= 12 {
        let tail: String = s.chars().skip(n - 4).collect();
        format!("****{tail}")
    } else {
        "****".to_string()
    }
}

impl Secrets {
    fn slot(&mut self, name: &str) -> Option<&mut Option<String>> {
        Some(match name {
            "youtube_api_key" => &mut self.youtube_api_key,
            "jamendo_client_id" => &mut self.jamendo_client_id,
            "soundcloud_client_id" => &mut self.soundcloud_client_id,
            "soundcloud_client_secret" => &mut self.soundcloud_client_secret,
            "spotify_client_id" => &mut self.spotify_client_id,
            "spotify_client_secret" => &mut self.spotify_client_secret,
            "lastfm_api_key" => &mut self.lastfm_api_key,
            _ => return None,
        })
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        match name {
            "youtube_api_key" => self.youtube_api_key.as_deref(),
            "jamendo_client_id" => self.jamendo_client_id.as_deref(),
            "soundcloud_client_id" => self.soundcloud_client_id.as_deref(),
            "soundcloud_client_secret" => self.soundcloud_client_secret.as_deref(),
            "spotify_client_id" => self.spotify_client_id.as_deref(),
            "spotify_client_secret" => self.spotify_client_secret.as_deref(),
            "lastfm_api_key" => self.lastfm_api_key.as_deref(),
            _ => None,
        }
    }

    /// Set (or with `None`/empty clear) a key by name; false for unknown names.
    pub fn set(&mut self, name: &str, value: Option<String>) -> bool {
        let v = value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        match self.slot(name) {
            Some(s) => {
                *s = v;
                true
            }
            None => false,
        }
    }

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
    /// SHA-256 of the optional self-hosted server bearer (never an account token).
    pub server_bearer_hash: Option<String>,
    /// The host implements the streaming /v1/audio endpoint.
    pub proxy_audio: bool,
    /// Require a live account session for /v1 API calls (login and health stay public).
    pub require_account: bool,
    pub secrets: Secrets,
    pub server_version: String,
    /// Anonymous requests/minute; verified accounts get 4x, fixed IP ceiling 8x.
    pub rate_limit_per_min: u32,
    pub provider_timeout_ms: u64,
    /// Stop YouTube Data API reads at this many units per UTC day.
    /// Search through yt-dlp does not consume this API budget.
    pub youtube_daily_budget_units: u64,
    /// yt-dlp executable used for YouTube search. `None` = yt-dlp is not
    /// available on this platform (Cloudflare Workers), so search is off.
    pub ytdlp_path: Option<String>,
    /// Max pages fetched per playlist import (Workers free plan: 50 subrequests).
    pub import_max_pages: u32,
    /// Switch for the yt-dlp companion, changed with `remote companion on|off`.
    pub companion_enabled: bool,
    /// AI assistants may reach signed-in apps through this server (MCP relay,
    /// `/mcp` + OAuth). Changed with `remote mcp on|off`.
    pub mcp_relay: bool,
    /// Days / GB of companion audio kept in R2 (Worker).
    pub audio_cache_days: u32,
    pub audio_cache_max_gb: u32,
    pub d1_max_mb: u32,
    pub audius_app_name: String,
    /// Argon2id PHC hash of the admin password. `None` = admin API disabled.
    pub admin_password_hash: Option<String>,
    /// Origin of short playlist links (`https://glyph.my` -> `https://glyph.my/p/<id>`).
    /// `None` = links point at the host the request came in on.
    pub share_origin: Option<String>,
}

impl Config {
    pub fn new(secrets: Secrets) -> Self {
        Self {
            server_bearer_hash: None,
            proxy_audio: false,
            require_account: false,
            secrets: secrets.normalized(),
            server_version: crate::SERVER_VERSION.to_string(),
            rate_limit_per_min: 60,
            provider_timeout_ms: 4000,
            youtube_daily_budget_units: 9000,
            ytdlp_path: None,
            import_max_pages: 5,
            companion_enabled: true,
            mcp_relay: true,
            audio_cache_days: 15,
            audio_cache_max_gb: 10,
            d1_max_mb: 500,
            audius_app_name: "glyph".to_string(),
            admin_password_hash: None,
            share_origin: None,
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
            // Search needs yt-dlp; the API key alone only enables playlist import.
            ProviderId::YouTube => self.ytdlp_path.is_some(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_shows_at_most_last_four() {
        assert_eq!(mask_secret("AIzaSyD-abcdefghijklmnop1234"), "****1234");
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret("elevenchars"), "****");
        assert_eq!(mask_secret("twelve_chars"), "****hars");
        assert_eq!(mask_secret(""), "****");
        // never longer than 8 chars, regardless of input
        assert!(mask_secret(&"x".repeat(500)).len() == 8);
    }

    #[test]
    fn secrets_get_set_by_name() {
        let mut s = Secrets::default();
        assert!(s.set("youtube_api_key", Some("  k  ".into())));
        assert_eq!(s.get("youtube_api_key"), Some("k"));
        assert!(s.set("youtube_api_key", Some("".into())));
        assert_eq!(s.get("youtube_api_key"), None);
        assert!(!s.set("nope", Some("x".into())));
        for (n, _) in KEY_NAMES {
            assert!(s.set(n, Some("v".into())), "{n}");
            assert_eq!(s.get(n), Some("v"));
        }
    }
}
