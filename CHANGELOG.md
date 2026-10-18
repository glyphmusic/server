# Changelog

## 0.2.0 — 2026-10-18

- Prefer the Windows setup executable on the download page when a release includes one. Keep the portable ZIP as the fallback for earlier releases.

- Account-aware request admission, abuse limits and bounded media work.
- Persistent audio cache inventory, storage reservations and audio quality variants.
- Updated account, admin and native companion behavior.
- Corrected the native updater repository to `glyphmusic/server`.
- Published native archives for five platform targets with SHA-256 checksums.
- Deployed the Cloudflare Worker and companion, including database migrations through `0013_audio_variant_jobs.sql`.
- Fixed missing Cloudflare routes for the download page, landing screenshots, favicon, app icons, web manifest and crawler files.

## 0.1.0

- Initial server implementation, provider search and resolution, streaming audio, optional accounts and synchronization, Cloudflare Worker and native companion.
