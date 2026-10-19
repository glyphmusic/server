# Glyph CLI / Server 0.3.0

- Install with `npm install --global glyph-server`; run `glyph help`.
- Inspect `glyph version all`, `glyph check companion` and `glyph check r-dlp`.
- Update individual components, or pull/recreate/verify your Docker companion with `--compose-dir`.
- Self-host with `glyph setup cloudflare`: prebuilt Worker, migrations and your own resources. R2 is optional; Rust and a desktop checkout are not needed.
- Source builds include the protocol snapshot and Docker builds use matching builder/runtime distributions.
- Include streaming audio performance improvements and optional account-only API access.

Native binary replacement reports when a restart remains necessary. Docker and npm-managed processes use their deployment/package update path. Existing companions need this server release before the new management endpoints are available.

Cloudflare requires D1 migrations through 0013_audio_variant_jobs.sql. The npm tarball and native archives are attached with checksums; npm publication is a separate authenticated release step.
