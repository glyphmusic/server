# Glyph Server 0.2.4

- Retry higher-priority full sources when a Deezer preview or a previous miss is cached; refresh outdated resolver mappings.
- Read artists from YouTube video titles, tolerate nullable channel metadata, and continue text searches after transient ISRC lookup failures.
- Retry empty YouTube searches instead of keeping temporary extraction failures cached.
- Reduce Worker overhead by batching abuse-limit checks, reusing request-local account lookups and avoiding redundant cache requests.
- Associate batched rate-limit results by their stable key.
- Reduce repeated search-result normalization and avoid copying full audio bodies through Wasm for supported range responses.
- Keep full-song quality warming out of the foreground streaming request CPU budget.

Cloudflare deployments require D1 migrations through 0013_audio_variant_jobs.sql.
