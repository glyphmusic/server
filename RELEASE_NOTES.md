# Glyph CLI / Server 0.6.1

- Device states live in each account's Durable Object: other devices see changes at once, without D1 writes.
- Abuse limits count ordinary requests in the Cache API; D1 keeps bans, strikes and exact limits.
- Fixed: when D1 stopped accepting writes, the Worker answered everything with 429. It now falls back to the Cache API limiter.
- glyph.my/changelog includes Glyph 0.6.1.

No API changes and no new D1 migrations (Cloudflare still requires migrations through 0014). The npm tarball and native archives are attached with checksums; npm publication is a separate authenticated release step.
