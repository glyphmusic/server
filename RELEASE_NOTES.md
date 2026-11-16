# Glyph server 0.11.1 - 2026-11-16

- Fixes unauthenticated `<img src>` fetches on `/v1/profiles/{handle}/images/{sha256}` while keeping private profiles and playlists authenticated (`401`).
- Ships the `glyph-web` 0.12.1 web player update (hidden off-screen YouTube embed, Document PiP mini player, queue preloading, batched sync pushes, `/web#mcp-connect=`).
- Adds cached 14-day `GET /v1/artist` lookup, Worker `PATCH`/`DELETE` CORS for MCP session management, and `glyph-protocol` 0.12.1 snapshot.

See CHANGELOG.md for details.
