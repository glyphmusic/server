//! Streaming Cache API path: audio never passes through KV or a whole-body buffer.
use futures::StreamExt;
use glyph_protocol::ProviderId;
use glyph_server_core::{media, Request, Server};
use worker::{Context, Fetch, Headers, RequestInit, RequestRedirect, Response, Result};

const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;

/// Exact provider-owned suffixes; redirects are checked again, never followed blindly.
pub fn allowed_upstream(url: &worker::Url, audio: bool) -> bool {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let domains: &[&str] = if audio {
        &[
            "dzcdn.net",
            "googlevideo.com",
            "audius.co",
            "audius.org",
            "audiuscontent.net",
            "jamendo.com",
            "jamendo.net",
            "archive.org",
        ]
    } else {
        &[
            "dzcdn.net",
            "scdn.co",
            "ytimg.com",
            "ggpht.com",
            "sndcdn.com",
            "audius.co",
            "audiuscontent.net",
            "jamendo.com",
            "jamendo.net",
            "archive.org",
            "wikimedia.org",
            "wikipedia.org",
            "musicbrainz.org",
            "coverartarchive.org",
        ]
    };
    domains
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

async fn origin(mut url: worker::Url, audio: bool) -> Result<Response> {
    for _ in 0..5 {
        // The caller checks audio-specific domains; all redirect targets must stay on known media hosts.
        if !allowed_upstream(&url, audio) {
            return Response::error("upstream host denied", 502);
        }
        let mut init = RequestInit::new();
        let headers = Headers::new();
        headers.set("accept-encoding", "identity")?;
        headers.set("user-agent", "glyph-server/0.1 (+https://github.com/glyphmusic/glyph-server)")?;
        init.with_headers(headers)
            .with_redirect(RequestRedirect::Manual);
        let req = worker::Request::new_with_init(url.as_str(), &init)?;
        let resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|_| worker::Error::RustError("media upstream unavailable".into()))?;
        if matches!(resp.status_code(), 301 | 302 | 303 | 307 | 308) {
            let Some(location) = resp.headers().get("location")? else {
                return Response::error("invalid upstream redirect", 502);
            };
            url = url.join(&location)?;
        } else {
            return Ok(resp);
        }
    }
    Response::error("too many upstream redirects", 502)
}

pub async fn handle(
    req: &worker::Request,
    core: &Request,
    server: &Server,
    ctx: &Context,
    env: &worker::Env,
) -> Result<Response> {
    let svc = server.services();
    // Streaming and cover loads use many more requests than metadata (especially seeks).
    let key = format!(
        "media:ip:{}",
        core.client_ip.as_deref().unwrap_or("unknown")
    );
    if let glyph_server_core::RateDecision::Limited { retry_after_s } = svc
        .limiter
        .check(
            &key,
            svc.config.rate_limit_per_min.max(1).saturating_mul(20),
            60,
        )
        .await
    {
        let e = glyph_server_core::ApiErr::rate_limited(retry_after_s);
        let mut out = Response::from_json(&e.body)?.with_status(e.status);
        out.headers_mut()
            .set("retry-after", &retry_after_s.to_string())?;
        return Ok(out);
    }
    if !matches!(req.method(), worker::Method::Get | worker::Method::Head) {
        return Response::error("GET or HEAD required", 405);
    }
    let input = req.url()?;
    let is_download = core.path.starts_with("/v1/download/");
    let is_audio = core.path.starts_with("/v1/audio/") || is_download;
    let mut key = input.clone();
    key.set_query(None);
    let upstream;
    let range = req.headers().get("range")?;
    if !is_audio {
        let Some(raw) = input
            .query_pairs()
            .find(|(k, _)| k == "url")
            .map(|(_, v)| v.into_owned())
        else {
            return Response::error("url required", 400);
        };
        let Ok(parsed) = worker::Url::parse(&raw) else {
            return Response::error("invalid url", 400);
        };
        if !allowed_upstream(&parsed, false) {
            return Response::error("host denied", 400);
        }
        key.query_pairs_mut().append_pair("url", &raw);
        upstream = Some(parsed);
    } else {
        upstream = None;
    }
    // Cloudflare automatically serves byte ranges from complete cached objects.
    let headers = Headers::new();
    if let Some(r) = &range {
        headers.set("range", r)?;
    }
    let mut init = RequestInit::new();
    init.with_headers(headers);
    let cache_req = worker::Request::new_with_init(key.as_str(), &init)?;
    let cache = worker::Cache::default();
    if let Some(hit) = cache.get(&cache_req, false).await? {
        let headers = hit.headers().clone();
        headers.set("x-glyph-cache", "HIT")?;
        let hit = hit.with_headers(headers);
        return if req.method() == worker::Method::Head {
            head(hit)
        } else {
            Ok(hit)
        };
    }
    let mut companion = false;
    let url = if let Some(url) = upstream {
        url
    } else {
        let prefix = if is_download {
            "/v1/download/"
        } else {
            "/v1/audio/"
        };
        let Some((provider, id)) = core
            .path
            .strip_prefix(prefix)
            .and_then(|p| p.split_once('/'))
        else {
            return Response::error("invalid audio path", 400);
        };
        let provider: ProviderId =
            match serde_json::from_value(serde_json::Value::String(provider.into())) {
                Ok(p) => p,
                Err(_) => return Response::error("unknown provider", 400),
            };
        if !media::valid_id(id) {
            return Response::error("invalid track id", 400);
        }
        if is_download {
            let raw = match media::download_url(&svc, provider, id).await {
                Ok(Some(u)) => u,
                _ => return Response::error("download not allowed", 403),
            };
            let url = worker::Url::parse(&raw)?;
            if !allowed_upstream(&url, true) {
                return Response::error("download host denied", 502);
            }
            url
        } else if provider == ProviderId::YouTube {
            if !media::valid_video_id(id) {
                return Response::error("invalid video id", 400);
            }
            let Some(base) =
                super::secret(env, "YTDLP_SERVICE_URL").filter(|u| u.starts_with("https://"))
            else {
                return Response::error("yt-dlp companion not configured", 503);
            };
            companion = true;
            worker::Url::parse(&format!(
                "{}/internal/audio/{id}",
                base.trim_end_matches('/')
            ))?
        } else {
            let raw = match media::audio_url(&svc, provider, id).await {
                Ok(u) => u,
                Err(_) => return Response::error("audio source unavailable", 502),
            };
            let url = worker::Url::parse(&raw)?;
            if !allowed_upstream(&url, true) {
                return Response::error("audio host denied", 502);
            }
            url
        }
    };
    let upstream = if companion {
        let Some(token) = super::secret(env, "YTDLP_SERVICE_TOKEN").filter(|t| t.len() >= 32)
        else {
            return Response::error("yt-dlp companion not configured", 503);
        };
        let headers = Headers::new();
        headers.set("authorization", &format!("Bearer {token}"))?;
        let mut init = RequestInit::new();
        init.with_headers(headers)
            .with_redirect(RequestRedirect::Manual);
        Fetch::Request(worker::Request::new_with_init(url.as_str(), &init)?)
            .send()
            .await?
    } else {
        origin(url, is_audio).await?
    };
    if upstream.status_code() != 200 {
        let status = match upstream.status_code() {
            401 | 403 | 404 => upstream.status_code(),
            _ => 502,
        };
        return Response::error("media upstream denied the request", status);
    }
    let length = upstream
        .headers()
        .get("content-length")?
        .and_then(|s| s.parse::<u64>().ok());
    if length.is_some_and(|n| n > MAX_CACHE_BYTES) {
        return Response::error("media exceeds 32 MiB limit", 413);
    }
    let content_type = upstream
        .headers()
        .get("content-type")?
        .unwrap_or_else(|| "application/octet-stream".into());
    if is_audio
        && !(content_type.starts_with("audio/")
            || content_type.starts_with("video/")
            || content_type.starts_with("application/octet-stream"))
    {
        return Response::error("upstream did not return audio", 502);
    }
    if !is_audio {
        let mime = content_type.split(';').next().unwrap_or("").trim();
        if !matches!(
            mime,
            "application/json"
                | "text/plain"
                | "image/jpeg"
                | "image/png"
                | "image/gif"
                | "image/webp"
                | "image/avif"
                | "application/octet-stream"
        ) {
            return Response::error("upstream is not metadata or an image", 502);
        }
    }
    let headers = Headers::new();
    headers.set("content-type", &content_type)?;
    headers.set("cache-control", "public, max-age=3600")?;
    headers.set("x-content-type-options", "nosniff")?;
    headers.set("content-security-policy", "default-src 'none'; sandbox")?;
    headers.set("referrer-policy", "no-referrer")?;
    if let Some(n) = length {
        headers.set("content-length", &n.to_string())?;
        headers.set("accept-ranges", "bytes")?;
    }
    // Rebuild to drop cookies, redirects and provider-specific headers.
    let mut upstream = upstream;
    let bounded = upstream.stream()?.scan(0u64, |received, chunk| {
        let chunk = chunk.and_then(|v| {
            *received += v.len() as u64;
            if *received > MAX_CACHE_BYTES {
                Err(worker::Error::RustError("media exceeds size limit".into()))
            } else {
                Ok(v)
            }
        });
        futures::future::ready(Some(chunk))
    });
    let mut full = Response::from_stream(bounded)?.with_headers(headers);
    if req.method() == worker::Method::Head {
        return head(full);
    }
    if length.is_some_and(|n| n > 0 && n <= MAX_CACHE_BYTES) {
        let cached = full.cloned()?;
        let key = key.to_string();
        ctx.wait_until(async move {
            let _ = worker::Cache::default().put(key, cached).await;
        });
    }
    full.headers_mut().set("x-glyph-cache", "MISS")?;
    if let (Some(value), Some(total)) = (range, length) {
        let Some((start, end)) = media::byte_range(&value, total) else {
            let mut out = Response::error("range not satisfiable", 416)?;
            out.headers_mut()
                .set("content-range", &format!("bytes */{total}"))?;
            return Ok(out);
        };
        let headers = full.headers().clone();
        headers.set("content-range", &format!("bytes {start}-{end}/{total}"))?;
        headers.set("content-length", &(end - start + 1).to_string())?;
        let stream = full.stream()?;
        let sliced =
            futures::stream::unfold((stream, 0u64), move |(mut stream, mut pos)| async move {
                while pos <= end {
                    let chunk = match stream.next().await? {
                        Ok(v) => v,
                        Err(e) => return Some((Err(e), (stream, end + 1))),
                    };
                    let offset = pos;
                    pos += chunk.len() as u64;
                    if pos <= start {
                        continue;
                    }
                    let a = start.saturating_sub(offset) as usize;
                    let b = (end + 1 - offset).min(chunk.len() as u64) as usize;
                    return Some((Ok(chunk[a..b].to_vec()), (stream, pos)));
                }
                None
            });
        return Ok(Response::from_stream(sliced)?
            .with_status(206)
            .with_headers(headers));
    }
    Ok(full)
}

fn head(resp: Response) -> Result<Response> {
    let status = resp.status_code();
    let headers = resp.headers().clone();
    Ok(Response::empty()?.with_status(status).with_headers(headers))
}
