# Glyph CLI / Server 0.7.0

- glyph.my/plugins lists every Glyph plugin with search, sorting, detail pages and **Install in Glyph**.
- Bans: `glyph remote ban account|device|ip|hardware|install`, `ban list`, `unban`; banned clients get 403.
- New CLI commands: `remote accounts revoke`, `remote stats`, `status`, `remote list|use`, `completions`, `man`, plus `--json`, `--dry-run` and `--yes` for scripts and AI agents. Errors name the next step and exit with a code per kind of problem.
- Fixed: the sync cleanup read every synced item of an account on each push and used up the D1 free tier's daily reads.
- glyph.my/changelog includes Glyph 0.7.0.

Two new D1 migrations: `0015_sync_tombstone_index.sql` and `0016_device_ban_marks.sql` (Cloudflare: `npx wrangler d1 migrations apply glyph --remote`). The npm tarball and native archives are attached with checksums; npm publication is a separate authenticated release step.
