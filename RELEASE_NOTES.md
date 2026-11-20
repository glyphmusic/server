# Glyph server 0.11.6 - 2026-11-20

- **Oldest working app version (`min_app_version`)**: servers can require a minimum Glyph app version (`MIN_APP_VERSION` / `min_app_version` / `glyph remote limits --min-app-version`); older apps receive `426 app_outdated` and show "Update Glyph" while health, site pages and the web player stay open.
- **Account storage limit (`account_max_mb`) & account caps (`max_accounts`)**: per-account storage cap (default 50 MB) with `GET /v1/account/storage` and `413 account_full`, plus total account cap and `GET /admin/v1/accounts` / `glyph remote accounts list`.
- **Share links open in the web player**: `/p/<id>` links opened in a browser now redirect to `/web/p/<id>` ready to play (bots still get link preview cards).
- **Shared album name caching & WebP pictures**: `/v1/cover` caches album names for all apps, and user playlist covers / profile pictures are stored as WebP.
- **Remote MCP plugin tools**: updated protocol snapshot (`0.12.12`) with plugin management tools.

See CHANGELOG.md for details.
