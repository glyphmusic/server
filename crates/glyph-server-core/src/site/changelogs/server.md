# Changelog

## 0.4.1 - 2026-10-20

- Restyle the AI assistant (MCP) sign-in page to match the desktop app.

## 0.4.0 - 2026-10-20

- `/v1/import/spotify` also reads SoundCloud links (sets, a profile's likes or tracks, single songs) with the server's SoundCloud key, so the app's "Transfer music" needs no key of its own.
- Serve the documentation at `docs.glyph.my` (and `/docs` on other hosts), rendered from the Markdown in github.com/glyphmusic/docs at request time: a push to `main` is live within about a minute without a deploy. `glyph.my/docs` redirects there. Pages have a search (Ctrl+K or /, index at `search.json`); for LLMs there are `llms.txt`, `llms-full.txt` and every page as Markdown at its `.md` address.
- Relay AI assistants (MCP) to signed-in desktop apps: Streamable HTTP endpoint `/mcp`, OAuth 2.1 with PKCE and dynamic client registration, a sign-in page confirmed in the app, and `glyph remote mcp on|off` for owners (migration 0014).
- Search YouTube through InnerTube directly before falling back to yt-dlp, and accept the app's YouTube ID hint to skip the search.
- Add `/changelog`: every release of the desktop app, server, companion and r-dlp, newest first, filterable by part (`scripts/snapshot-changelogs.mjs`).
- Add a questions section to the landing page, with FAQ structured data.
- Add `glyph config list|get|set|unset|path` to read and change `server.toml` with validation, sources and defaults; API keys are never printed.
- Add `glyph cloudflare list|set|unset|secret|domain` to change a Cloudflare deployment's values, secrets and custom domain.

## 0.3.0 - 2026-10-19

- Distribute the `glyph` CLI through the `glyph-server` npm package, with verified native downloads and a legacy command alias.
- Add component version/check/update commands for CLI, server, companion and r-dlp, plus authenticated companion maintenance.
- Ship the prebuilt Worker and migrations; add guided self-hosted Cloudflare setup and deployments using the owner's resources.
- Support scoped Docker companion updates with pulled-image and health verification.
- Make source/Docker/CI builds self-contained with a shared protocol snapshot.
- Stream and coalesce encoded audio, reuse connections, move cache stats off the playback path and bound parallel resolution.
- Add optional account-only API/media access while preserving login and refresh.

## 0.2.6 - 2026-10-19

- Serve lyrics from the companion first (`POST /internal/lyrics`) and fall back to the Worker's own lookup, so LRCLIB's rate limit on data-centre addresses no longer breaks lyrics.
- Answer `/v1/lyrics` with "no lyrics" (404) when the upstream is rate-limited or down, instead of a 502/503; a failed lookup is retried after five minutes, a confirmed miss after an hour.
- Answer `/v1/similar` with an empty list (200) when Last.fm is not configured or fails, instead of a 503/502.
- Build against desktop protocol v0.2.4.

## 0.2.5 - 2026-10-19

- Add persistent native r-dlp workers through GLYPH_RDLP_PATH, retaining the existing yt-dlp extraction fallback.
- Route YouTube downloads through the same companion streaming, quality and stale-URL recovery paths as playback.
- Honor the client youtube_only flag, allow bounded additional search variants, and reject other sources and previews in this mode.
- Accept both legacy track bodies and resolve request bodies at the download endpoint.
- Build against desktop protocol v0.2.4.

## 0.2.4 — 2026-10-19

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
