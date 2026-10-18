# Glyph Server 0.2.3

- The companion now resolves songs for the Worker, which keeps Worker requests within Cloudflare's CPU limit. Without a companion the Worker resolves as before. Add `JAMENDO_CLIENT_ID` and `SOUNDCLOUD_CLIENT_ID`/`SOUNDCLOUD_CLIENT_SECRET` to the companion's `.env` so it searches the same sources as the Worker.
- Fix hung Worker requests and exceeded CPU limits under concurrent load.
- Fix Audius playback and downloads (HTTP 502) after Audius moved audio to presigned R2 URLs.

Cloudflare deployments require D1 migrations through 0013_audio_variant_jobs.sql.
