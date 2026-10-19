//! Shared, bounded encodes. Readers replay the prefix and follow new fragments.
use super::*;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Output {
    chunks: Vec<Bytes>,
    size: usize,
    done: Option<bool>,
}
struct Job {
    output: Mutex<Output>,
    changed: tokio::sync::watch::Sender<u64>,
}
impl Job {
    fn new() -> Self {
        Self {
            output: Mutex::new(Output::default()),
            changed: tokio::sync::watch::channel(0).0,
        }
    }
    fn append(&self, bytes: Bytes) -> Result<(), std::io::Error> {
        let mut out = self.output.lock().unwrap();
        if out.size + bytes.len() > MAX_AUDIO_BYTES as usize {
            return Err(std::io::Error::other("encoded audio exceeds size limit"));
        }
        out.size += bytes.len();
        out.chunks.push(bytes);
        self.changed.send_modify(|n| *n += 1);
        Ok(())
    }
    fn finish(&self, ok: bool) {
        self.output.lock().unwrap().done = Some(ok);
        self.changed.send_modify(|n| *n += 1);
    }
    fn response(self: &Arc<Self>) -> Response {
        let state = (self.clone(), self.changed.subscribe(), 0);
        let stream = futures::stream::unfold(state, |(job, mut changed, mut index)| async move {
            loop {
                if index == usize::MAX {
                    return None;
                }
                let (chunk, done) = {
                    let out = job.output.lock().unwrap();
                    (out.chunks.get(index).cloned(), out.done)
                };
                if let Some(chunk) = chunk {
                    index += 1;
                    return Some((Ok::<_, std::io::Error>(chunk), (job, changed, index)));
                }
                if let Some(ok) = done {
                    if ok {
                        return None;
                    }
                    // Emit one terminal error, never a successful truncated EOF.
                    return Some((
                        Err(std::io::Error::other("audio encoding failed")),
                        (job, changed, usize::MAX),
                    ));
                }
                if index == usize::MAX || changed.changed().await.is_err() {
                    return None;
                }
            }
        });
        Response::builder()
            .header("content-type", "audio/mp4")
            .header("cache-control", "no-store")
            .body(Body::from_stream(stream))
            .unwrap()
    }
}
fn jobs() -> &'static Mutex<HashMap<String, Arc<Job>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}
fn cached(key: &str) -> Option<Bytes> {
    let mut cache = variant_cache().lock().unwrap();
    cache.retain(|(_, at, _)| at.elapsed() < std::time::Duration::from_secs(600));
    cache
        .iter()
        .find(|(k, _, _)| k == key)
        .map(|(_, _, bytes)| bytes.clone())
}

pub(super) async fn companion_variant(
    svc: &Arc<Services>,
    id: &str,
    variant: glyph_server_core::media::AudioVariant,
    permit: tokio::sync::OwnedSemaphorePermit,
    background: bool,
) -> Response {
    let key = format!("{id}/{}", variant.name());
    let job = {
        let mut active = jobs().lock().unwrap();
        // The job lock also covers completion/cache publication, avoiding a
        // second encode between cache lookup and registration.
        if let Some(bytes) = cached(&key) {
            return variant_response(bytes);
        }
        if let Some(job) = active.get(&key) {
            job.clone()
        } else {
            static ENCODERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
            static BACKGROUND: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
            let bg = if background {
                match BACKGROUND
                    .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
                    .clone()
                    .try_acquire_owned()
                {
                    Ok(p) => Some(p),
                    Err(_) => {
                        return plain(StatusCode::TOO_MANY_REQUESTS, "background encoder busy")
                    }
                }
            } else {
                None
            };
            let Ok(encoder) = ENCODERS
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(3)))
                .clone()
                .try_acquire_owned()
            else {
                return plain(
                    StatusCode::TOO_MANY_REQUESTS,
                    "audio encoder busy; retry later",
                );
            };
            let job = Arc::new(Job::new());
            active.insert(key.clone(), job.clone());
            let (svc, id, task_job) = (svc.clone(), id.to_string(), job.clone());
            tokio::spawn(async move {
                let job = task_job;
                let (_encoder, _background) = (encoder, bg);
                let ok = matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(90),
                        encode(&svc, &id, variant, permit, &job)
                    )
                    .await,
                    Ok(Ok(()))
                );
                let mut active = jobs().lock().unwrap();
                if ok {
                    let out = job.output.lock().unwrap();
                    let mut bytes = Vec::with_capacity(out.size);
                    for chunk in &out.chunks {
                        bytes.extend_from_slice(chunk);
                    }
                    let bytes = Bytes::from(bytes);
                    let mut cache = variant_cache().lock().unwrap();
                    while cache.len() >= 8
                        || cache.iter().map(|(_, _, v)| v.len()).sum::<usize>() + bytes.len()
                            > 64 * 1024 * 1024
                    {
                        if cache.pop_front().is_none() {
                            break;
                        }
                    }
                    cache.push_back((key.clone(), std::time::Instant::now(), bytes));
                }
                job.finish(ok);
                active.remove(&key);
            });
            job
        }
    };
    let response = job.response();
    if background {
        // Preparation callers need a complete representation with known size.
        match axum::body::to_bytes(response.into_body(), MAX_AUDIO_BYTES as usize).await {
            Ok(bytes) => variant_response(bytes),
            Err(_) => plain(StatusCode::BAD_GATEWAY, "audio encoding failed"),
        }
    } else {
        response
    }
}

// A non-faststart MP4 needs seeking to find its tail moov atom. Such inputs
// use a bounded temporary file; normal faststart MP4/Ogg/WebM can be piped.
fn pipe_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"OggS") {
        return Some("ogg");
    }
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some("matroska");
    }
    if bytes.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let mut at = 0usize;
    while let Some(header) = bytes.get(at..at.checked_add(8)?) {
        if &header[4..8] == b"moov" {
            return Some("mov");
        }
        if &header[4..8] == b"mdat" {
            return None;
        }
        let size = u32::from_be_bytes(header[..4].try_into().ok()?) as usize;
        if size < 8 {
            return None;
        }
        at = at.checked_add(size)?;
    }
    None
}
async fn encode(
    svc: &Services,
    id: &str,
    variant: glyph_server_core::media::AudioVariant,
    permit: tokio::sync::OwnedSemaphorePermit,
    job: &Job,
) -> Result<(), std::io::Error> {
    let error = || std::io::Error::other("audio encoding failed");
    let url = audio_url(svc, glyph_protocol::ProviderId::YouTube, id)
        .await
        .map_err(|_| error())?;
    let mut slot = Some(permit);
    let response = fetch_upstream_audio(&url, None, &mut slot).await;
    let response = retry_stale_youtube(svc, id, None, response, &mut slot).await;
    if response.status() != StatusCode::OK {
        return Err(error());
    }
    transcode(response.into_body().into_data_stream(), variant, job).await
}
async fn transcode(
    mut source: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
    variant: glyph_server_core::media::AudioVariant,
    job: &Job,
) -> Result<(), std::io::Error> {
    let error = || std::io::Error::other("audio encoding failed");
    let mut prefix = Vec::new();
    while prefix.len() < 65_536 {
        let Some(bytes) = source.next().await else {
            break;
        };
        prefix.extend_from_slice(&bytes.map_err(|_| error())?);
        if pipe_format(&prefix).is_some() {
            break;
        }
    }
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source.audio");
    let format = pipe_format(&prefix);
    if format.is_none() {
        if prefix.get(4..8) != Some(b"ftyp") {
            return Err(error());
        }
        let mut file = tokio::fs::File::create(&input).await?;
        file.write_all(&prefix).await?;
        let mut size = prefix.len();
        while let Some(bytes) = source.next().await {
            let bytes = bytes.map_err(|_| error())?;
            size += bytes.len();
            if size > MAX_AUDIO_BYTES as usize {
                return Err(error());
            }
            file.write_all(&bytes).await?;
        }
        file.flush().await?;
    }
    let mut command = tokio::process::Command::new(
        std::env::var("GLYPH_FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into()),
    );
    command.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-f",
        format.unwrap_or("mov"),
        "-i",
    ]);
    if format.is_some() {
        command.arg("pipe:0");
    } else {
        command.arg(&input);
    }
    command.args(["-map", "0:a:0", "-vn"]);
    if variant == glyph_server_core::media::AudioVariant::High && format.unwrap_or("mov") == "mov" {
        command.args(["-c:a", "copy"]);
    } else {
        command
            .args(["-ac", "2", "-ar", "44100", "-c:a", "aac", "-b:a"])
            .arg(format!("{}k", variant.bitrate()));
    }
    command
        .args([
            "-threads",
            "1",
            "-movflags",
            "+frag_keyframe+empty_moov+default_base_moof",
            "-frag_duration",
            "250000",
            "-f",
            "mp4",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(error)?;
    let mut stdout = child.stdout.take().ok_or_else(error)?;
    let feed = async {
        if format.is_some() {
            stdin.write_all(&prefix).await?;
            while let Some(bytes) = source.next().await {
                stdin.write_all(&bytes.map_err(|_| error())?).await?;
            }
        }
        drop(stdin);
        Ok::<_, std::io::Error>(())
    };
    let read = async {
        let mut buffer = vec![0; 32 * 1024];
        loop {
            let n = stdout.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            job.append(Bytes::copy_from_slice(&buffer[..n]))?;
        }
        Ok::<_, std::io::Error>(())
    };
    let wait = async {
        if child.wait().await?.success() {
            Ok(())
        } else {
            Err(error())
        }
    };
    tokio::try_join!(feed, read, wait)?;
    if job.output.lock().unwrap().size == 0 {
        return Err(error());
    }
    job.append(Bytes::from_static(
        glyph_server_core::media::VARIANT_COMPLETE_MARKER,
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn readers_receive_prefix_before_completion_and_late_readers_replay_it() {
        let job = Arc::new(Job::new());
        let mut first = job.response().into_body().into_data_stream();
        job.append(Bytes::from_static(b"first")).unwrap();
        assert_eq!(first.next().await.unwrap().unwrap(), "first");
        let mut second = job.response().into_body().into_data_stream();
        assert_eq!(second.next().await.unwrap().unwrap(), "first");
        assert!(futures::poll!(first.next()).is_pending());
        job.append(Bytes::from_static(b"second")).unwrap();
        job.finish(true);
        assert_eq!(first.next().await.unwrap().unwrap(), "second");
        assert_eq!(second.next().await.unwrap().unwrap(), "second");
        assert!(first.next().await.is_none());
        assert!(second.next().await.is_none());
    }

    #[tokio::test]
    async fn failure_is_a_terminal_stream_error() {
        let job = Arc::new(Job::new());
        let mut reader = job.response().into_body().into_data_stream();
        job.append(Bytes::from_static(b"prefix")).unwrap();
        job.finish(false);
        assert!(reader.next().await.unwrap().is_ok());
        assert!(reader.next().await.unwrap().is_err());
        assert!(reader.next().await.is_none());
    }

    #[tokio::test]
    async fn duplicate_requests_join_an_existing_encode_without_fetching_source() {
        let settings =
            Settings::resolve(Default::default(), None, "unused-test-config.toml".into());
        let svc = build_server(&settings, LogBuffer::new()).services();
        let id = "coalesce001";
        let key = format!("{id}/low");
        let job = Arc::new(Job::new());
        jobs().lock().unwrap().insert(key.clone(), job.clone());
        let permits = Arc::new(tokio::sync::Semaphore::new(2));
        let a = companion_variant(
            &svc,
            id,
            glyph_server_core::media::AudioVariant::Low,
            permits.clone().acquire_owned().await.unwrap(),
            false,
        )
        .await;
        let b = companion_variant(
            &svc,
            id,
            glyph_server_core::media::AudioVariant::Low,
            permits.clone().acquire_owned().await.unwrap(),
            false,
        )
        .await;
        assert_eq!(
            permits.available_permits(),
            2,
            "joined readers must not retain encoder/source slots"
        );
        job.append(Bytes::from_static(b"one shared encode"))
            .unwrap();
        job.finish(true);
        for response in [a, b] {
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 100)
                    .await
                    .unwrap(),
                "one shared encode"
            );
        }
        jobs().lock().unwrap().remove(&key);
    }

    #[test]
    fn tail_indexed_mp4_requires_a_seekable_file() {
        assert_eq!(
            pipe_format(b"\0\0\0\x10ftypM4A \0\0\0\0\0\0\0\x08moov"),
            Some("mov")
        );
        assert_eq!(
            pipe_format(b"\0\0\0\x10ftypM4A \0\0\0\0\0\0\0\x08mdat"),
            None
        );
        assert_eq!(pipe_format(b"OggS"), Some("ogg"));
        assert_eq!(pipe_format(&[0x1a, 0x45, 0xdf, 0xa3]), Some("matroska"));
        assert_eq!(pipe_format(b"playlist"), None);
    }

    #[tokio::test]
    #[ignore = "requires GLYPH_FFMPEG_PATH pointing to a real FFmpeg binary"]
    async fn real_encoder_streams_before_source_eof_and_preserves_cached_ranges() {
        let ffmpeg = std::env::var("GLYPH_FFMPEG_PATH").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.m4a");
        let status = tokio::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=8",
                "-c:a",
                "aac",
                "-movflags",
                "+faststart",
            ])
            .arg(&input)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let input = tokio::fs::read(input).await.unwrap();
        assert_eq!(pipe_format(&input), Some("mov"));
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let source =
            futures::stream::unfold(rx, |mut rx| async { rx.recv().await.map(|v| (v, rx)) });
        let source = Body::from_stream(source).into_data_stream();
        let job = Arc::new(Job::new());
        let task_job = job.clone();
        let encode = tokio::spawn(async move {
            let result = transcode(
                source,
                glyph_server_core::media::AudioVariant::Low,
                &task_job,
            )
            .await;
            task_job.finish(result.is_ok());
            result
        });
        let mut reader = job.response().into_body().into_data_stream();
        // Leave the source open: the encoder must deliver audio before EOF.
        tx.send(Ok(Bytes::copy_from_slice(&input[..input.len() * 3 / 4])))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut prefix = Vec::new();
            while !prefix.windows(4).any(|b| b == b"mdat") {
                prefix.extend_from_slice(&reader.next().await.unwrap().unwrap());
            }
        })
        .await
        .expect("no audio fragment before source EOF");
        tx.send(Ok(Bytes::copy_from_slice(&input[input.len() * 3 / 4..])))
            .await
            .unwrap();
        drop(tx);
        encode.await.unwrap().unwrap();
        let complete = axum::body::to_bytes(job.response().into_body(), MAX_AUDIO_BYTES as usize)
            .await
            .unwrap();
        // Exercise the same demuxer/decoder used by Glyph, including a source
        // without Content-Length or seek support, as on the first request.
        struct Unseekable(std::io::Cursor<Vec<u8>>);
        impl std::io::Read for Unseekable {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                std::io::Read::read(&mut self.0, b)
            }
        }
        impl std::io::Seek for Unseekable {
            fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
                Err(std::io::Error::other("unseekable"))
            }
        }
        impl symphonia::core::io::MediaSource for Unseekable {
            fn is_seekable(&self) -> bool {
                false
            }
            fn byte_len(&self) -> Option<u64> {
                None
            }
        }
        let source = symphonia::core::io::MediaSourceStream::new(
            Box::new(Unseekable(std::io::Cursor::new(complete.to_vec()))),
            Default::default(),
        );
        let mut format = symphonia::default::get_probe()
            .format(
                &Default::default(),
                source,
                &Default::default(),
                &Default::default(),
            )
            .unwrap()
            .format;
        let params = format.default_track().unwrap().codec_params.clone();
        let mut decoder = symphonia::default::get_codecs()
            .make(&params, &Default::default())
            .unwrap();
        let mut frames = 0;
        loop {
            match format.next_packet() {
                Ok(packet) => {
                    frames += decoder.decode(&packet).unwrap().frames();
                }
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break
                }
                Err(e) => panic!("fragmented audio decode failed: {e}"),
            }
        }
        assert!(
            frames >= 7 * 44100,
            "incomplete decoded song: {frames} frames"
        );
        let path = dir.path().join("output.m4a");
        tokio::fs::write(&path, &complete).await.unwrap();
        assert!(tokio::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(path)
            .args(["-f", "null", "-"])
            .status()
            .await
            .unwrap()
            .success());
        let range = slice_variant(variant_response(complete.clone()), Some("bytes=7-35")).await;
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            axum::body::to_bytes(range.into_body(), 100).await.unwrap(),
            complete.slice(7..36)
        );
        let tail = dir.path().join("tail-index.m4a");
        assert!(tokio::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "aac"
            ])
            .arg(&tail)
            .status()
            .await
            .unwrap()
            .success());
        let tail = tokio::fs::read(tail).await.unwrap();
        assert_eq!(pipe_format(&tail), None);
        let job = Arc::new(Job::new());
        transcode(
            Body::from(tail).into_data_stream(),
            glyph_server_core::media::AudioVariant::Standard,
            &job,
        )
        .await
        .unwrap();
        job.finish(true);
        let complete = axum::body::to_bytes(job.response().into_body(), MAX_AUDIO_BYTES as usize)
            .await
            .unwrap();
        assert!(complete.ends_with(glyph_server_core::media::VARIANT_COMPLETE_MARKER));
        let output = dir.path().join("tail-output.m4a");
        tokio::fs::write(&output, complete).await.unwrap();
        assert!(tokio::process::Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(output)
            .args(["-f", "null", "-"])
            .status()
            .await
            .unwrap()
            .success());
    }
}
