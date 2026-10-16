//! Internet Archive: Advanced Search restricted to Creative-Commons audio.

use super::*;
use crate::traits::HttpRequest;

#[derive(Deserialize)]
struct SearchResp {
    response: Docs,
}
#[derive(Deserialize)]
struct Docs {
    #[serde(default)]
    docs: Vec<Doc>,
}
#[derive(Deserialize)]
struct Doc {
    identifier: String,
    title: Option<serde_json::Value>,
    creator: Option<serde_json::Value>,
}

fn first_string(v: Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Array(a)) => {
            a.into_iter().find_map(|x| x.as_str().map(str::to_string)).unwrap_or_default()
        }
        _ => String::new(),
    }
}

pub async fn search(svc: &Services, q: &str) -> PResult<Vec<Candidate>> {
    // Strip Lucene operators from user input; keep it a plain phrase query.
    let clean: String = q.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect();
    if clean.trim().is_empty() {
        return Ok(vec![]);
    }
    let query = format!("({}) AND mediatype:audio AND licenseurl:*creativecommons*", clean.trim());
    let url = build_url(
        "https://archive.org/advancedsearch.php",
        &[
            ("q", &query),
            ("fl[]", "identifier"),
            ("fl[]", "title"),
            ("fl[]", "creator"),
            ("rows", "10"),
            ("output", "json"),
        ],
    );
    let resp: SearchResp = fetch_json(svc, HttpRequest::get(url)).await?;
    Ok(resp
        .response
        .docs
        .into_iter()
        .map(|d| {
            let title = first_string(d.title);
            let mut t = new_track(&d.identifier, ProviderId::Archive, &title, &first_string(d.creator));
            t.permalink = Some(format!("https://archive.org/details/{}", d.identifier));
            t.cover_url = Some(format!("https://archive.org/services/img/{}", d.identifier));
            candidate(t, None)
        })
        .collect())
}

#[derive(Deserialize)]
struct Meta {
    #[serde(default)]
    files: Vec<File>,
    server: Option<String>,
}
#[derive(Deserialize)]
struct File {
    name: String,
    format: Option<String>,
}

/// Pick the first MP3 (or Ogg) file of the item; the app streams it from archive.org.
pub async fn playback(svc: &Services, id: &str) -> PResult<PlaybackSource> {
    let meta: Meta = fetch_json(svc, HttpRequest::get(format!("https://archive.org/metadata/{}", url_path(id)))).await?;
    let _ = meta.server;
    let pick = |fmt_part: &str| {
        meta.files.iter().find(|f| f.format.as_deref().map(|x| x.contains(fmt_part)).unwrap_or(false))
    };
    let (f, mime) = match pick("MP3") {
        Some(f) => (f, "audio/mpeg"),
        None => (pick("Ogg").ok_or(ProviderError::Parse)?, "audio/ogg"),
    };
    Ok(PlaybackSource::Stream {
        provider: ProviderId::Archive,
        url: format!("https://archive.org/download/{}/{}", url_path(id), url_path(&f.name)),
        mime: Some(mime.into()),
    })
}

fn url_path(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20").replace("%2F", "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn search_and_playback() {
        let h = harness(Default::default());
        h.http.on("advancedsearch.php", 200, r#"{"response":{"docs":[{"identifier":"abc","title":"T","creator":["C"]}]}}"#);
        h.http.on(
            "metadata/abc",
            200,
            r#"{"files":[{"name":"a.flac","format":"Flac"},{"name":"my song.mp3","format":"VBR MP3"}]}"#,
        );
        let r = block_on(search(&h.svc, "x")).unwrap();
        assert_eq!(r[0].track.artist, "C");
        let p = block_on(playback(&h.svc, "abc")).unwrap();
        match p {
            PlaybackSource::Stream { url, .. } => assert_eq!(url, "https://archive.org/download/abc/my%20song.mp3"),
            _ => panic!(),
        }
    }
}
