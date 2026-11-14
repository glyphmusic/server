# Security policy

## Supported versions

Security fixes ship on `main` and in the latest release, and are deployed to the
public server (`api.glyph.my`, `glyph.my`, `docs.glyph.my`). Self-hosted
operators should run the latest release.

| Version | Supported |
| --- | --- |
| Latest release (`0.10.x`) / `main` | Yes |
| Public instance (`api.glyph.my`, `glyph.my`, `docs.glyph.my`) | Yes |
| Older releases | No — update to the latest release |

## Reporting a vulnerability

Report security vulnerabilities privately through
**[GitHub Security Advisories](https://github.com/glyphmusic/server/security/advisories/new)**
or the contact in [glyph.my/.well-known/security.txt](https://glyph.my/.well-known/security.txt).

Do **not** open a public issue or pull request for a security problem, and do
not run automated high-volume scans or denial-of-service tests against the
public instance — reproduce on a local or self-hosted server where possible.

Please include:

- the affected endpoint, crate (`glyph-server-core`, `glyph-server-worker`, `glyph-server-native`) or CLI command,
- request/response steps to reproduce the issue,
- what data or action an attacker can reach.

## Scope

### In scope

- Authentication, passkey, recovery-code, password-hash or session bypasses
- Cross-account access to private libraries, encrypted sync items, device lists or unlisted profile data
- Server-side request forgery (SSRF) or allowlist bypasses in `/v1/upstream`, `/v1/audio`, `/v1/download` or playlist import
- Verification or replay flaws in HMAC-signed Worker-to-companion requests (`YTDLP_SERVICE_TOKEN`) or admin endpoints (`ADMIN_PASSWORD_HASH`, `COMPANION_ADMIN_TOKEN`)
- Cross-site scripting (XSS) or Content-Security-Policy bypasses on `glyph.my`, `/p/:id` share pages or `docs.glyph.my`
- Rate-limit, quota, device-limit or ban bypasses in server logic

### Out of scope

- Volumetric traffic or denial-of-service against `api.glyph.my` or `glyph.my`
- Self-hosted deployments exposed without authentication or rate limits contrary to [docs/SELF_HOSTING.md](docs/SELF_HOSTING.md)
