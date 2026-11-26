# Glyph server 0.12.6 - 2026-11-26

- **Reliable Explicit & `?` ratings**: multi-source rating pipeline (Deezer, Apple Music/iTunes and LRCLIB lyrics scanning) with shared cache across search, charts, similar tracks and artist pages.
- **Service-based artist verification**: new batched `POST /v1/artists/meta` endpoint using real service signals instead of fan counts, with 14-day caching.
- **Full server-side artist catalog cache**: indexes artist discographies with real song/release counts, tags and links, and automatically merges newly released albums every 12 hours.

See CHANGELOG.md for details.
