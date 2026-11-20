# Glyph server 0.11.7 - 2026-11-20

- **In-app server admin dashboard & account admin role**: grant accounts admin access (`glyph remote accounts admin <handle> [--off]`) for the in-app Server dashboard with account/storage overview, 30-day history charts, device/ban/limit controls and cache management (keys, updates, logs and admin grants stay owner-only).
- **Server-pinned app versions (`stable_app_version`, `latest_app_version`)**: pin the Stable and Latest app update channels via CLI (`glyph remote limits --stable-version` / `--latest-version`), config or the in-app dashboard.
- **Unified glyph.my site design**: refreshed `/`, `/download`, `/plugins`, `/changelog` and docs with the shared panel look, titlebar and mobile menu.
- **Protocol & changelog snapshots**: updated `release-protocol` to `0.12.13` and refreshed embedded changelogs.

See CHANGELOG.md for details.
