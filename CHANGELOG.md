# Changelog

## 0.2.4 � 2026-10-19

- Retry higher-priority full sources when a Deezer preview or a previous miss is cached; refresh outdated resolver mappings.
- Read artists from YouTube video titles, tolerate nullable channel metadata, and continue text searches after transient ISRC lookup failures.
- Retry empty YouTube searches instead of keeping temporary extraction failures cached.
- Reduce Worker overhead by batching abuse-limit checks, reusing request-local account lookups and avoiding redundant cache requests.
- Associate batched rate-limit results by their stable key.
- Reduce repeated search-result normalization and avoid copying full audio bodies through Wasm for supported range responses.
- Keep full-song quality warming out of the foreground streaming request CPU budget.

## 0.2.3 — 2026-10-18

- Resolve Worker cache misses on the companion (`POST /internal/resolve`), so the Worker parses one small answer instead of several provider searches. Without a companion, or when it is busy or offline, the Worker resolves itself as before.
- Never share the admission queue between Worker requests. A permit handed across requests woke the waiter in the other request's context, which Cloudflare reported as hung requests and exceeded CPU limits.
- Accept Audius' presigned R2 audio redirects (`*.r2.cloudflarestorage.com`) as redirect targets only; Audius playback and downloads returned HTTP 502.

## 0.2.2 — 2026-10-18

- Reuse three prestarted yt-dlp extractors in parallel, coalesce duplicate video lookups, and separate audio transfer capacity from metadata requests. Keep a bounded CLI fallback.
- Preserve upstream ETags and modification dates so clients can check downloaded audio revisions without blocking playback.

- Reuse resolved YouTube URLs in bounded companion memory, avoiding repeated yt-dlp extraction on seeks and audio refills without storing audio on disk.

- Signed-in sharing copies short Glyph links for songs, artists and playlists, with rich previews and desktop deep links. Shared playlists open as a preview with an explicit Add to library action.

- Avoid a Worker crash when a long cache key contains Unicode characters at the truncation boundary.
- Start background quality preparation for partial audio responses and let companion encodes finish after a request disconnects.

- Match YouTube by ISRC when available, then retry artist/title without the audio suffix if necessary (fixes Blender's LUVME falling back to a preview).
- Prepare two quality variants concurrently in the background, keeping an encoder available for foreground work.

- Forward requested byte ranges through the Worker and companion to YouTube, avoiding repeated downloads of the entire prefix when seeking or refilling the decoder.

- Stream uncached high-quality YouTube audio immediately instead of waiting for a whole download and remux.
- Publish one `SHA256SUMS.txt` for the complete release; self-update matches exact filenames and retains support for old sidecars.

- Search for song audio through the configured companion when resolving a YouTube match; avoid unrelated video results and duration mismatches.
- Explicitly slice full edge-cache hits for range requests; re-fetch entries missing the byte length instead of returning inconsistent audio responses.

- Retry full playback sources before a cached Deezer preview and shorten preview caching to five minutes. Invalidate earlier resolver mappings.
- Skip per-video metadata extraction when YouTube flat results already include duration.
- Do not disable all YouTube requests for ten minutes after one network failure or timeout.

## 0.2.0 — 2026-10-18

- Prefer the Windows setup executable on the download page when a release includes one. Keep the portable ZIP as the fallback for earlier releases.

- Account-aware request admission, abuse limits and bounded media work.
- Persistent audio cache inventory, storage reservations and audio quality variants.
- Updated account, admin and native companion behavior.
- Corrected the native updater repository to `glyphmusic/server`.
- Published native archives for five platform targets with SHA-256 checksums.
- Deployed the Cloudflare Worker and companion, including database migrations through `0013_audio_variant_jobs.sql`.
- Fixed missing Cloudflare routes for the download page, landing screenshots, favicon, app icons, web manifest and crawler files.

## 0.1.0

- Initial server implementation, provider search and resolution, streaming audio, optional accounts and synchronization, Cloudflare Worker and native companion.
