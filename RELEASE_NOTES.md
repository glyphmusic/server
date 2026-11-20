# Glyph server 0.11.8 - 2026-11-20

- **Device app version tracking**: stores each device's last reported Glyph app version (`0017_device_app_version.sql`) for the admin dashboard and CLI.
- **Admin API blocks & profile reset**: inspect and clear active API/rate-limit/login/YouTube blocks (`/admin/v1/blocks`, `/admin/v1/blocks/clear`) and reset a user's profile picture or banner (`/admin/v1/accounts/<handle>/profile/reset`).
- **Protocol & changelog snapshots**: updated `release-protocol` to `0.12.14` and refreshed embedded changelogs.

See CHANGELOG.md for details.
