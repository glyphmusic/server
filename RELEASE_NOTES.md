# Glyph Server 0.2.6

- Serve lyrics from the companion first (`POST /internal/lyrics`) and fall back to the Worker's own lookup, so LRCLIB's rate limit on data-centre addresses no longer breaks lyrics.
- Answer `/v1/lyrics` with "no lyrics" (404) when the upstream is rate-limited or down, instead of a 502/503; a failed lookup is retried after five minutes, a confirmed miss after an hour.
- Answer `/v1/similar` with an empty list (200) when Last.fm is not configured or fails, instead of a 503/502.
- Build against desktop protocol v0.2.4.

Cloudflare deployments require D1 migrations through 0013_audio_variant_jobs.sql.
