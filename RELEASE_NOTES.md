# Glyph CLI / Server 0.5.0

- Account devices: presence and playback commands between apps of the same account.
- Live jams over WebSocket (`/v1/jam/{code}/live`, Durable Object `JAM_ROOM`), within the Cloudflare free plan.
- Lighter jam polling and position-only updates.

No new D1 migrations (Cloudflare still requires migrations through 0014). Cloudflare deployments get the `JamRoom` Durable Object (SQLite class) on their next deploy. The npm tarball and native archives are attached with checksums; npm publication is a separate authenticated release step.
