# Glyph server 0.12.2 - 2026-11-22

- **Faster & more reliable music video resolution**: native server resolves H.264 MP4 video streams and audio in one pass via persistent `r-dlp` workers (with `yt-dlp` fallback), defaults `/v1/video/resolve` to 720p HD with paired audio, caches returned resolutions (`v2` keys), and validates upstream video `content-type` in the Cloudflare Worker.
- **Snapshots**: refreshed embedded desktop (`0.13.3`), r-dlp (`0.2.3`) and server (`0.12.2`) changelogs and web player assets.

See CHANGELOG.md for details.
