# Glyph server 0.11.9 - 2026-11-21

- **Faster imports & YouTube search**: caches SoundCloud resolve metadata during paginated imports, returns total counts in import pagination, and skips `yt-dlp` fallback on valid empty YouTube search results.
- **R2 audio cache for `quality=high`**: `quality=high` companion audio streams now use the Cloudflare R2 audio cache.
- **Protocol & changelog snapshots**: updated `release-protocol` to `0.12.15` and refreshed embedded changelogs.

See CHANGELOG.md for details.
