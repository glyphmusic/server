//! Apple Music / iTunes public catalog search (no key required): used as an
//! additional explicit-rating source (`trackExplicitness`) and to confirm
//! official artist profiles (`entity=musicArtist`).

use super::*;
use crate::merge::plain_words;
use crate::resolver::{score, ACCEPT_SCORE};
use crate::traits::HttpRequest;
use glyph_protocol::ExplicitRating;

#[derive(Deserialize)]
struct SearchResp<T> {
    #[serde(default = "Vec::new")]
    results: Vec<T>,
}

#[derive(Deserialize)]
struct ItunesSong {
    #[serde(rename = "trackName", default)]
    track_name: String,
    #[serde(rename = "artistName", default)]
    artist_name: String,
    #[serde(rename = "trackTimeMillis", default, deserialize_with = "de_u64_lenient")]
    track_time_millis: Option<u64>,
    #[serde(rename = "trackExplicitness")]
    track_explicitness: Option<String>,
    #[serde(rename = "collectionExplicitness")]
    collection_explicitness: Option<String>,
}

#[derive(Deserialize)]
struct ItunesArtistItem {
    #[serde(rename = "artistName", default)]
    artist_name: String,
    #[serde(rename = "artistLinkUrl")]
    artist_link_url: Option<String>,
    #[serde(rename = "primaryGenreName")]
    primary_genre_name: Option<String>,
    #[serde(rename = "amgArtistId", default, deserialize_with = "de_u64_lenient")]
    amg_artist_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItunesArtist {
    pub name: String,
    pub url: Option<String>,
    pub genre: Option<String>,
}

fn wants_clean_edit(title: &str) -> bool {
    let low = title.to_lowercase();
    low.contains("radio edit")
        || low.contains("(clean")
        || low.contains("[clean")
        || low.contains("clean version")
        || low.contains("censored")
        || low.contains("edited")
}

/// Looks up `track` in the iTunes/Apple Music catalog and returns its
/// explicitness rating when a high-confidence match is found.
pub async fn rate_track(svc: &Services, track: &Track) -> PResult<Option<ExplicitRating>> {
    let q = format!("{} {}", track.artist, track.title);
    let url = build_url(
        "https://itunes.apple.com/search",
        &[("term", q.trim()), ("media", "music"), ("entity", "song"), ("limit", "6")],
    );
    let resp: SearchResp<ItunesSong> = fetch_json(svc, HttpRequest::get(url)).await?;
    let want_clean = wants_clean_edit(&track.title);
    let mut best_score = 0.0f32;
    let mut best_rating: Option<ExplicitRating> = None;
    let mut any_explicit = false;

    for item in resp.results {
        if item.track_name.trim().is_empty() || item.artist_name.trim().is_empty() {
            continue;
        }
        let mut t = new_track("itunes", ProviderId::MusicBrainz, &item.track_name, &item.artist_name);
        t.duration_ms = item.track_time_millis;
        let sc = score(track, &candidate(t, None));
        if sc < ACCEPT_SCORE {
            continue;
        }
        let explicitness = item
            .track_explicitness
            .as_deref()
            .or(item.collection_explicitness.as_deref())
            .unwrap_or("");
        let r = match explicitness {
            "explicit" => ExplicitRating::Explicit,
            "notExplicit" | "cleaned" => ExplicitRating::Clean,
            _ => continue,
        };
        if r == ExplicitRating::Explicit {
            any_explicit = true;
        }
        if sc > best_score {
            best_score = sc;
            best_rating = Some(r);
        }
    }

    if !want_clean && any_explicit {
        return Ok(Some(ExplicitRating::Explicit));
    }
    Ok(best_rating)
}

/// Looks up an official artist entry in Apple Music / iTunes by exact normalized name.
pub async fn lookup_artist(svc: &Services, name: &str) -> PResult<Option<ItunesArtist>> {
    let url = build_url(
        "https://itunes.apple.com/search",
        &[("term", name.trim()), ("media", "music"), ("entity", "musicArtist"), ("limit", "6")],
    );
    let resp: SearchResp<ItunesArtistItem> = fetch_json(svc, HttpRequest::get(url)).await?;
    let want = plain_words(name);
    if want.is_empty() {
        return Ok(None);
    }
    let best = resp
        .results
        .into_iter()
        .filter(|a| plain_words(&a.artist_name) == want)
        .max_by_key(|a| (a.amg_artist_id.is_some(), a.artist_link_url.is_some()));
    Ok(best.map(|a| ItunesArtist {
        name: a.artist_name,
        url: a.artist_link_url.filter(|u| u.starts_with("https://")),
        genre: a.primary_genre_name.filter(|g| !g.trim().is_empty()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn rates_explicit_and_clean_tracks() {
        let h = harness(Default::default());
        h.http.on(
            "itunes.apple.com/search",
            200,
            r#"{"results":[
              {"trackName":"Stronger","artistName":"Kanye West","trackTimeMillis":311000,"trackExplicitness":"cleaned"},
              {"trackName":"Stronger","artistName":"Kanye West","trackTimeMillis":312000,"trackExplicitness":"explicit"}
            ]}"#,
        );
        let t = track("v1", ProviderId::YouTube, "Stronger", "Kanye West", 312);
        assert_eq!(block_on(rate_track(&h.svc, &t)).unwrap(), Some(ExplicitRating::Explicit));

        // Explicit Clean / Radio Edit preserves clean rating.
        let clean = track("v2", ProviderId::YouTube, "Stronger (Radio Edit)", "Kanye West", 311);
        assert_eq!(block_on(rate_track(&h.svc, &clean)).unwrap(), Some(ExplicitRating::Clean));
    }

    #[test]
    fn looks_up_exact_artist() {
        let h = harness(Default::default());
        h.http.on(
            "itunes.apple.com/search",
            200,
            r#"{"results":[
              {"artistName":"Daft Punk Tribute","artistLinkUrl":"https://music.apple.com/us/artist/1"},
              {"artistName":"Daft Punk","artistLinkUrl":"https://music.apple.com/us/artist/5468295","primaryGenreName":"Electronic","amgArtistId":168773}
            ]}"#,
        );
        let a = block_on(lookup_artist(&h.svc, "daft punk")).unwrap().unwrap();
        assert_eq!(a.name, "Daft Punk");
        assert_eq!(a.genre.as_deref(), Some("Electronic"));
        assert!(a.url.as_deref().unwrap().contains("5468295"));
    }
}
