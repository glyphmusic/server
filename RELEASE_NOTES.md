# Glyph CLI / Server 0.4.0

- Relay AI assistants (MCP) to signed-in desktop apps: `/mcp` with OAuth 2.1, in-app approval and `glyph remote mcp on|off`. Requires D1 migration 0014.
- `/v1/import/spotify` also reads SoundCloud links, so the app's "Transfer music" needs no key of its own.
- Documentation at `docs.glyph.my` (search, `llms.txt`, Markdown pages), a `/changelog` page and a questions section on the landing page.
- Search YouTube through InnerTube directly and accept the app's YouTube ID hint.
- New `glyph config list|get|set|unset|path` and `glyph cloudflare list|set|unset|secret|domain` commands.

Cloudflare requires D1 migrations through 0014. The npm tarball and native archives are attached with checksums; npm publication is a separate authenticated release step.
