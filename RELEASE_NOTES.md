# Glyph server 0.11.5 - 2026-11-19

- **Whole artist pages**: `GET /v1/artist` now also returns the artist's top songs, the discography and similar artists, cached for 14 days. One lookup serves every app, so artist pages open fast and the same for everyone.
- Older apps ignore the new fields and keep working as before.

See CHANGELOG.md for details.
