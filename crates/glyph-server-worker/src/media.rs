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
        headers.set("user-agent", "glyph-server/0.1 (+https://github.com/glyphmusic/server)")?;
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
    if let Err(e) = server.admit(core, true).await {
        let r = glyph_server_core::Response::error(e);
        let mut out = Response::from_bytes(r.bytes().unwrap_or_default().to_vec())?.with_status(r.status);
        for (k, v) in &r.headers { out.headers_mut().set(k, v)?; }
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
    let variant = if let Some(id) = core.path.strip_prefix("/v1/audio/youtube/") {
        if !media::valid_video_id(id) { return Response::error("invalid video id",400); }
        if !svc.config.companion_enabled { return Response::error("companion disabled",403); }
        match core.query.get("quality") {
            Some(raw) => {
                let Some(quality)=media::AudioVariant::parse(raw) else { return Response::error("invalid audio quality",400); };
                if let (Ok(db),Ok(bucket))=(env.d1("GLYPH_DB"),env.bucket("GLYPH_COVERS")) {
                    match super::variants::cached(&db,bucket,id,quality,range.as_deref(),req.method()==worker::Method::Head,svc.config.audio_cache_days).await {
                        Ok(Some(hit)) => {
                            let env=env.clone();let id=id.to_string();
                            let days=svc.config.audio_cache_days;
                            let cap=u64::from(svc.config.audio_cache_max_gb)*1_000_000_000;
                            ctx.wait_until(async move { if super::variants::warm(env,id,days,cap).await.is_err(){worker::console_error!("audio quality preparation deferred");} });
                            return Ok(hit);
                        },
                        Ok(None) => {},
                        Err(_) => worker::console_error!("audio chunk cache lookup failed; using companion"),
                    }
                }
                Some((id.to_string(),quality))
            }
            None => None,
        }
    } else {None};
    // Cloudflare automatically serves byte ranges from complete cached objects.
    let headers = Headers::new();
    if let Some(r) = &range {
        headers.set("range", r)?;
    }
    let mut init = RequestInit::new();
    init.with_headers(headers);
    let cache_req = worker::Request::new_with_init(key.as_str(), &init)?;
    let cache = worker::Cache::default();
    let companion_audio = core.path.starts_with("/v1/audio/youtube/");
    if companion_audio && !svc.config.companion_enabled {
        return Response::error("companion disabled", 403);
    }
    // Edge hits would bypass the current R2 retention and companion switch.
    if let Some(hit) = if companion_audio { None } else { cache.get(&cache_req, false).await? } {
        let headers = hit.headers().clone();
        headers.set("x-glyph-cache", "HIT")?;
        let hit = hit.with_headers(headers);
        let length = hit.headers().get("content-length")?.and_then(|s| s.parse::<u64>().ok());
        // Stream-backed Cache API objects can return a full 200 even when
        // the lookup requested a range. Slice it explicitly, just like a miss.
        // Without a length, re-fetch rather than hand the decoder a different
        // response shape (and lose MP3's leading-ID3 workaround).
        if range.is_none() || hit.status_code() == 206 || length.is_some() {
            return if req.method() == worker::Method::Head { head(hit) }
                else { range_response(hit, range, length) };
        }
    }
    let mut companion = false;
    let mut cached_upstream: Option<Response> = None;
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
            if !svc.config.companion_enabled {
                return Response::error("companion disabled", 403);
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
    // Everything fetched through the companion is kept on Cloudflare (R2), so
    // a song plays once from the companion machine and from R2 after that,
    // even while that machine is off. The companion itself caches nothing.
    let audio_bucket = env.bucket("GLYPH_COVERS").ok();
    let r2_key = (companion && variant.is_none()).then(|| r2_audio_key(url.path())).flatten();
    let mut from_r2 = false;
    if let (Some(bucket), Some(k)) = (&audio_bucket, &r2_key) {
        let max_age_ms = u64::from(svc.config.audio_cache_days) * 86_400_000;
        if let Ok(Some(obj)) = bucket.get(k).execute().await {
            // Older than `audio_cache_days`: fetch again (and replace it).
            let age_ms = (worker::Date::now().as_millis()).saturating_sub(obj.uploaded().as_millis());
            if age_ms <= max_age_ms {
            if let Ok(db) = env.d1("GLYPH_DB") { super::audio_cache::touch(&db, k).await; }
            let content_type = obj.http_metadata().content_type.unwrap_or_else(|| "audio/mp4".into());
            let size = obj.size();
            if let Some(body) = obj.body() {
                let headers = Headers::new();
                headers.set("content-type", &content_type)?;
                headers.set("content-length", &size.to_string())?;
                cached_upstream = Some(Response::from_stream(body.stream()?)?.with_headers(headers));
                from_r2 = true;
            }
            }
        }
    }
    let upstream = if let Some(hit) = cached_upstream.take() {
        hit
    } else if companion {
        let Some(token) = super::secret(env, "YTDLP_SERVICE_TOKEN").filter(|t| t.len() >= 32)
        else {
            return Response::error("yt-dlp companion not configured", 503);
        };
        let headers = Headers::new();
        headers.set("authorization", &format!("Bearer {token}"))?;
        if let Some((_,quality))=&variant {
            // High is the original best source, as on the standalone server.
            // Do not wait for an entire download and remux before sending its
            // first bytes. Lower-bitrate representations still use the encoder.
            if *quality != media::AudioVariant::High {
                headers.set("x-glyph-audio-quality",quality.name())?;
            } else if let Some(range) = &range {
                headers.set("range", range)?;
            }
        }
        let mut init = RequestInit::new();
        init.with_headers(headers)
            .with_redirect(RequestRedirect::Manual);
        Fetch::Request(worker::Request::new_with_init(url.as_str(), &init)?)
            .send()
            .await?
    } else {
        origin(url, is_audio).await?
    };
    if upstream.status_code() == 206 && is_audio && range.is_some() {
        let Some(content_range) = upstream.headers().get("content-range")? else { return Response::error("missing upstream range",502); };
        if !media::content_range_total(&content_range).is_some_and(|n| n <= MAX_CACHE_BYTES) {
            return Response::error("invalid upstream range",502);
        }
        let headers = Headers::new();
        for name in ["content-type", "content-length", "content-range"] {
            if let Some(value) = upstream.headers().get(name)? { headers.set(name, &value)?; }
        }
        headers.set("accept-ranges", "bytes")?;
        headers.set("cache-control", "no-store")?;
        if let Some((id, _)) = &variant {
            let (env, id) = (env.clone(), id.clone());
            let days = svc.config.audio_cache_days;
            let cap = u64::from(svc.config.audio_cache_max_gb) * 1_000_000_000;
            ctx.wait_until(async move {
                if super::variants::warm(env, id, days, cap).await.is_err() {
                    worker::console_error!("background audio preparation deferred");
                }
            });
        }
        let mut partial = upstream;
        return Ok(Response::from_stream(partial.stream()?)?.with_status(206).with_headers(headers));
    }
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
    for name in ["etag", "last-modified"] {
        if let Some(value) = upstream.headers().get(name)? { headers.set(name, &value)?; }
    }
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
    if !companion_audio && length.is_some_and(|n| n > 0 && n <= MAX_CACHE_BYTES) {
        let cached = full.cloned()?;
        let key = key.to_string();
        ctx.wait_until(async move {
            let _ = worker::Cache::default().put(key, cached).await;
        });
    }
    if let Some((id,quality))=variant {
        if length.is_some_and(|n|n>0 && n<=MAX_CACHE_BYTES)
            && (quality != media::AudioVariant::High || content_type.starts_with("audio/mp4")) {
            if let (Ok(db),Ok(bucket))=(env.d1("GLYPH_DB"),env.bucket("GLYPH_COVERS")) {
                let mut copy=full.cloned()?;
                let days=svc.config.audio_cache_days;
                let cap=u64::from(svc.config.audio_cache_max_gb)*1_000_000_000;
                let warm_env=env.clone();
                ctx.wait_until(async move {
                    if let Ok(bytes)=copy.bytes().await {
                        if Some(bytes.len() as u64)!=length{return;}
                        if super::variants::store(&db,&bucket,&id,quality,bytes,days,cap).await.is_err(){
                            worker::console_error!("audio variant caching failed; bytes remain reserved");
                        }
                        if super::variants::warm(warm_env,id,days,cap).await.is_err(){
                            worker::console_error!("audio quality preparation deferred");
                        }
                    }
                });
            }
        }
    }
    if let (Some(bucket), Some(k), false) = (audio_bucket, r2_key, from_r2) {
        if length.is_some_and(|n| n > 0 && n <= MAX_CACHE_BYTES) {
            let mut copy = full.cloned()?;
            let content_type = content_type.clone();
            let db = env.d1("GLYPH_DB").ok();
            let max_bytes = u64::from(svc.config.audio_cache_max_gb) * 1_000_000_000;
            let days = svc.config.audio_cache_days;
            ctx.wait_until(async move {
                if let (Some(db), Ok(bytes)) = (db, copy.bytes().await) {
                    // Never charge a claimed Content-Length: account actual bytes.
                    if Some(bytes.len() as u64) != length { return; }
                    if super::audio_cache::store(&db, &bucket, &k, bytes, content_type, days, max_bytes).await.is_err() {
                        worker::console_error!("audio cache persistence/maintenance failed; reservation retained");
                    }
                }
            });
        }
    }
    full.headers_mut().set("x-glyph-cache", if from_r2 { "R2" } else { "MISS" })?;
    range_response(full, range, length)
}

fn range_response(mut full: Response, range: Option<String>, length: Option<u64>) -> Result<Response> {
    if full.status_code() != 200 { return Ok(full); }
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

/// R2 key for audio fetched from the companion (`/internal/audio/<id>` -> `audio/youtube/<id>`).
fn r2_audio_key(path: &str) -> Option<String> {
    let id = path.strip_prefix("/internal/audio/")?.rsplit('/').next()?;
    media::valid_video_id(id).then(|| format!("audio/youtube/{id}"))
}
