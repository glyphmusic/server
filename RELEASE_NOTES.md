# Glyph server 0.11.4 - 2026-11-16

- **Storage Split**: separates cached covers/audio (`GLYPH_CACHE` -> `glyph-cache`) from user uploads (`GLYPH_USERS` -> `glyph-users`, keeping `GLYPH_COVERS` fallback) and updates Worker config name to `server`.
- **Playback Poll & Cover Metadata**: avoids unnecessary Durable Object calls on single-device playback polls, routes `playback:` room scopes cleanly, and returns `{ url, album }` from `POST /v1/cover`.
- **Web player (`/web`)**: embeds `glyph-web` `0.12.4` (Web Connect remote playback, Home quick-pick cards, album backfill, UI fixes) and `glyph-protocol` `0.12.4`.

See CHANGELOG.md for details.
