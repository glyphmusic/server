//! Artist metadata: bio from Wikipedia, high-quality portraits from Deezer
//! (`picture_xl` / `picture_big`) and Wikimedia Commons original photos,
//! cached in `Services::cache` (`GLYPH_KV` on Cloudflare Workers) for 14 days.
//! Strict musical disambiguation ensures non-musicians or cities are never matched.

use crate::cache::{self, DAY};
use crate::providers::{build_url, fetch_json};
use crate::traits::{HttpRequest, Services};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const ARTIST_TTL: u64 = 14 * DAY;
pub const NOT_FOUND_TTL: u64 = DAY;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistResponse {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fans: Option<u64>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub similar: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ArtistLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistLink {
    pub label: String,
    pub url: String,
}

const MUSIC_WORDS: &[&str] = &[
    "musician", "singer", "rapper", "band", "duo", "trio", "group", "dj", "producer", "songwriter", "composer",
    "vocalist", "guitarist", "drummer", "pianist", "record", "hip hop", "music", "musiker", "sänger", "sängerin",
    "komponist", "produzent", "рэп", "репер", "певец", "певица", "музыкант", "группа", "исполнитель", "продюсер",
    "композитор", "диджей", "автор песен",
];

pub(crate) fn is_musical(desc: &str) -> bool {
    let d = desc.to_lowercase();
    MUSIC_WORDS.iter().any(|w| d.contains(w))
}

pub(crate) fn names_match(name: &str, title: &str) -> bool {
    let n = crate::merge::plain_words(name);
    let t = crate::merge::plain_words(title.split(" (").next().unwrap_or(title));
    !n.is_empty() && n == t
}

#[derive(Deserialize)]
struct DzSearchResp {
    #[serde(default)]
    data: Vec<DzArtistItem>,
}

#[derive(Deserialize)]
struct DzArtistItem {
    #[serde(default)]
    name: String,
    link: Option<String>,
    picture: Option<String>,
    picture_medium: Option<String>,
    picture_big: Option<String>,
    picture_xl: Option<String>,
    nb_fan: Option<u64>,
}

async fn lookup_deezer(svc: &Services, name: &str) -> Option<(Option<String>, Option<u64>, Option<String>)> {
    let url = build_url("https://api.deezer.com/search/artist", &[("q", name), ("limit", "10")]);
    let resp: DzSearchResp = fetch_json(svc, HttpRequest::get(url)).await.ok()?;
    let want = crate::merge::plain_words(name);
    let best = resp
        .data
        .into_iter()
        .filter(|a| crate::merge::plain_words(&a.name) == want)
        .max_by_key(|a| a.nb_fan.unwrap_or(0))?;
    let photo = best.picture_xl.or(best.picture_big).or(best.picture_medium).or(best.picture);
    Some((photo, best.nb_fan, best.link))
}

struct WikiArtist {
    bio: String,
    url: String,
    original_img: Option<String>,
    thumb_img: Option<String>,
    is_landscape: bool,
}

async fn lookup_wikipedia(svc: &Services, name: &str, lang: &str) -> Option<WikiArtist> {
    let lang = if matches!(lang, "en" | "de" | "ru") { lang } else { "en" };
    let mut langs = vec![lang];
    if lang != "en" {
        langs.push("en");
    }
    for l in langs {
        if let Some(res) = lookup_wikipedia_in(svc, name, l).await {
            return Some(res);
        }
    }
    None
}

async fn lookup_wikipedia_in(svc: &Services, name: &str, lang: &str) -> Option<WikiArtist> {
    let search_url = build_url(
        &format!("https://{lang}.wikipedia.org/w/api.php"),
        &[("action", "query"), ("list", "search"), ("srsearch", name), ("srlimit", "5"), ("format", "json")],
    );
    let found: Value = fetch_json(svc, HttpRequest::get(search_url)).await.ok()?;
    let titles: Vec<String> = found["query"]["search"]
        .as_array()?
        .iter()
        .filter_map(|h| h["title"].as_str().map(str::to_string))
        .collect();

    for title in titles.into_iter().filter(|t| names_match(name, t)) {
        let mut summary_url = match url::Url::parse(&format!("https://{lang}.wikipedia.org/api/rest_v1/page/summary/")) {
            Ok(u) => u,
            Err(_) => continue,
        };
        if let Ok(mut segs) = summary_url.path_segments_mut() {
            segs.pop_if_empty().push(&title.replace(' ', "_"));
        }
        let s: Value = fetch_json(svc, HttpRequest::get(summary_url.as_str())).await.ok()?;
        if s["type"].as_str() == Some("disambiguation") {
            continue;
        }
        if !is_musical(s["description"].as_str().unwrap_or("")) {
            continue;
        }
        let bio = s["extract"].as_str().unwrap_or("").trim().to_string();
        if bio.is_empty() {
            continue;
        }
        let page = s["content_urls"]["desktop"]["page"].as_str().unwrap_or("").to_string();
        let original_img = s["originalimage"]["source"].as_str().map(str::to_string);
        let thumb_img = s["thumbnail"]["source"].as_str().map(str::to_string);
        let orig_w = s["originalimage"]["width"].as_u64().unwrap_or(0);
        let orig_h = s["originalimage"]["height"].as_u64().unwrap_or(0);
        let is_landscape = orig_w >= orig_h && orig_w > 0;
        return Some(WikiArtist {
            bio,
            url: page,
            original_img,
            thumb_img,
            is_landscape,
        });
    }
    None
}

/// Lookup artist metadata, checking `Services::cache` first.
pub async fn lookup(svc: &Arc<Services>, name: &str, lang: &str) -> Result<ArtistResponse, crate::ApiErr> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::ApiErr::bad_request("name is required"));
    }
    let key = format!("artist:v1:{}:{}", lang.to_ascii_lowercase(), crate::merge::plain_words(name));
    if let Some(hit) = cache::get_json::<ArtistResponse>(&*svc.cache, &key).await {
        return Ok(hit);
    }

    let deezer_fut = lookup_deezer(svc, name);
    let wiki_fut = lookup_wikipedia(svc, name, lang);
    let (dz, wiki) = futures::join!(deezer_fut, wiki_fut);

    let (dz_photo, fans, dz_link) = match dz {
        Some((p, f, l)) => (p, f, l),
        None => (None, None, None),
    };

    let (bio, wiki_url, wiki_orig, wiki_thumb, is_landscape) = match wiki {
        Some(w) => (Some(w.bio), Some(w.url), w.original_img, w.thumb_img, w.is_landscape),
        None => (None, None, None, None, false),
    };

    let wiki_img = wiki_orig.or(wiki_thumb);

    let (photo, banner) = match (&dz_photo, &wiki_img) {
        (Some(dp), Some(wp)) => {
            if is_landscape {
                (Some(dp.clone()), Some(wp.clone()))
            } else {
                (Some(dp.clone()), Some(wp.clone()))
            }
        }
        (Some(dp), None) => (Some(dp.clone()), Some(dp.clone())),
        (None, Some(wp)) => (Some(wp.clone()), Some(wp.clone())),
        (None, None) => (None, None),
    };

    let url = wiki_url.clone().or(dz_link.clone());
    let mut links = Vec::new();
    if let Some(u) = &wiki_url {
        links.push(ArtistLink { label: "Wikipedia".into(), url: u.clone() });
    }
    if let Some(u) = &dz_link {
        links.push(ArtistLink { label: "Deezer".into(), url: u.clone() });
    }

    let verified = fans.is_some_and(|f| f >= 50_000);
    let is_empty = bio.is_none() && photo.is_none() && fans.is_none();
    let ttl = if is_empty { NOT_FOUND_TTL } else { ARTIST_TTL };

    let resp = ArtistResponse {
        name: name.to_string(),
        bio,
        url,
        photo,
        banner,
        fans,
        verified,
        tags: Vec::new(),
        similar: Vec::new(),
        links,
    };

    cache::put_json(&*svc.cache, &key, &resp, ttl).await;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn disambiguation_and_matching() {
        assert!(is_musical("English rock band"));
        assert!(is_musical("deutscher Rapper"));
        assert!(!is_musical("Argentine footballer"));
        assert!(!is_musical("city in Germany"));
        assert!(names_match("Daft Punk", "Daft Punk"));
        assert!(names_match("Muse", "Muse (band)"));
        assert!(!names_match("Muse", "Museum of Modern Art"));
    }

    #[test]
    fn artist_lookup_merges_deezer_and_wikipedia() {
        let h = harness(Default::default());
        h.http.on(
            "api.deezer.com/search/artist",
            200,
            r#"{"data":[{"id":1,"name":"Daft Punk","picture_xl":"https://dz/xl.jpg","picture_big":"https://dz/big.jpg","nb_fan":4000000,"link":"https://deezer.com/artist/1"}]}"#,
        );
        h.http.on(
            "en.wikipedia.org/w/api.php",
            200,
            r#"{"query":{"search":[{"title":"Daft Punk"}]}}"#,
        );
        h.http.on(
            "en.wikipedia.org/api/rest_v1/page/summary/Daft_Punk",
            200,
            r#"{"type":"standard","description":"French electronic music duo","extract":"Daft Punk were a French electronic music duo.","content_urls":{"desktop":{"page":"https://en.wikipedia.org/wiki/Daft_Punk"}},"originalimage":{"source":"https://commons/orig.jpg","width":1920,"height":1080}}"#,
        );

        let res = block_on(lookup(&h.svc, "Daft Punk", "en")).unwrap();
        assert_eq!(res.name, "Daft Punk");
        assert_eq!(res.bio.as_deref(), Some("Daft Punk were a French electronic music duo."));
        assert_eq!(res.photo.as_deref(), Some("https://dz/xl.jpg"));
        assert_eq!(res.banner.as_deref(), Some("https://commons/orig.jpg"));
        assert_eq!(res.fans, Some(4000000));
        assert!(res.verified);
        assert_eq!(res.links.len(), 2);

        // Cached hit returns identical data:
        let cached = block_on(lookup(&h.svc, "Daft Punk", "en")).unwrap();
        assert_eq!(cached, res);
    }
}
