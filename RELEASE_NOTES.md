# Glyph server 0.11.3 - 2026-11-16

- **Artwork & Deduplication**: prefers non-YouTube album covers over YouTube thumbnails when merging tracks and resolving `/v1/cover` metadata.
- **Sync Rate Limiting**: isolates `/v1/sync/*` quota so bursty sync traffic returns soft `429 rate_limited` without locking out the rest of the account's API requests.
- **Web player (`/web`)**: embeds `glyph-web` `0.12.3` (PWA manifest/icons, background audio recovery, sync/rate-limit handling, and startup/UI guards) and `glyph-protocol` `0.12.3`.

See CHANGELOG.md for details.
