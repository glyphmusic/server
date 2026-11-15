# glyph-web

The built web player (glyph.my/web). `node scripts/snapshot-web.mjs --build`
fills this folder from `glyph-apps/glyph-ui` before `wrangler deploy`; the
Worker serves it at `/web` (see `glyph-apps/docs/WEB.md`). Only this README
and the ignore files are committed.
