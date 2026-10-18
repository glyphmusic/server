# Changelog

## Unreleased

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
