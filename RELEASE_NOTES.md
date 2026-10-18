# Glyph Server 0.2.2

- Run three reusable YouTube audio resolvers concurrently and start them before playback requests arrive. Duplicate requests share extraction; searches have independent capacity.
- Forward media revision metadata for background download refresh checks.

- Keep resolved YouTube URLs in companion memory so seeking and refilling reuse the source instead of rerunning yt-dlp.

- Signed-in sharing copies short Glyph links for songs, artists and playlists, with rich previews and desktop deep links. Shared playlists open as a preview with an explicit Add to library action.

- Stream YouTube audio in byte ranges immediately and find full songs through the companion, including ISRC matching.
- Prepare audio qualities concurrently in the background, including after partial responses or client disconnects.
- Fix Unicode cache keys that could terminate the Worker isolate.
- Publish a single SHA256SUMS.txt checksum manifest.

Cloudflare deployments require D1 migrations through 0013_audio_variant_jobs.sql.
