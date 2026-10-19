# Glyph Server 0.2.5

- Add persistent native r-dlp workers through GLYPH_RDLP_PATH, retaining the existing yt-dlp extraction fallback.
- Route YouTube downloads through the same companion streaming, quality and stale-URL recovery paths as playback.
- Honor the client youtube_only flag, allow bounded additional search variants, and reject other sources and previews in this mode.
- Accept both legacy track bodies and resolve request bodies at the download endpoint.
- Build against desktop protocol v0.2.4.

Cloudflare deployments require D1 migrations through 0013_audio_variant_jobs.sql.
