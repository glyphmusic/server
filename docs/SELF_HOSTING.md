# Self-hosting glyph-server

## Quick start with the npm CLI

```sh
npm install --global glyph-server
glyph setup cloudflare
```

This creates a separate deployment project using your own Cloudflare login,
D1 database and optional R2 bucket. It includes the prebuilt Worker and migrations;
Node.js 22+ is enough. Your config is `glyph-cloudflare/wrangler.json`.
Use `glyph deploy --config glyph-cloudflare/wrangler.json` for later deployments.
The checked-in `crates/glyph-server-worker/wrangler.toml` describes the maintainer's
production instance; the setup wizard does not reuse its resource IDs or domains.
See [the CLI guide](../npm/README.md) for versions, diagnostics and updates.


This guide sets up your own Glyph server with accounts and library sync, on
your own Cloudflare account and your own domain. Everything below uses free
Cloudflare plans unless noted. The [README](../README.md) covers the music side
(sources, API keys, audio proxy, yt-dlp companion); this file covers accounts,
domain, limits and operating the server.

Point the Glyph app at your server under Settings -> Server (`server_url`).
Nothing in the app is tied to the public instance at `api.glyph.my`.

## 1. Trust model

The Glyph app is open source, so anyone can change it. **The server treats every
client as untrusted.** Everything that protects the server or other users is
decided and enforced on the server only:

| Rule | Where it is enforced |
| --- | --- |
| Devices per account | Server (operator setting, section 6). The app only displays it; `PATCH /v1/account` with `device_limit` is refused with 403 `server_setting`. A new device is inserted with one SQL statement that checks the count, so parallel sign-ins cannot pass the limit. |
| Rate limits | Server, per IP / install id / handle / account (section 7). |
| Request and upload sizes | Server, per route, before the body is parsed. |
| Handle rules and reserved names | Server (`normalize_handle`). |
| Who may read or change which data | Server: every account route checks the access token; there is no route that returns another user's data. |
| Passkey checks (origin, RP ID, challenge, signature, sign count) | Server (`accounts/webauthn.rs`). |
| Cover cache hosts | Server allowlist (`glyph_protocol::cover_cache_allowed`), so it is no open proxy. |

The only check that lives in the client is the minimum password length (10
characters): the password never reaches the server (section 5), so the server
cannot see it. A modified client can only weaken its own user's password.

Limits that exist inside the app (cache sizes, download size caps, toast count)
protect the user's own device, not the server.

## 2. What you need

* A Cloudflare account (free). R2 asks for a payment method once, even on the free tier.
* A domain added to that Cloudflare account (section 4). `*.workers.dev` also
  works for testing, but passkeys are bound to the domain, so pick the final one
  before real users sign up.
* Rust with `rustup target add wasm32-unknown-unknown`, Node 18+.
* The `glyph-server` binary (release download or `cargo build --release -p glyph-server-native`)
  for the admin CLI. It does not need to run as a server for this.

## 3. Cloudflare resources

All commands run in `crates/glyph-server-worker`. Log in with the Cloudflare
account that should own the server:

```
npx wrangler login
npx wrangler whoami            # check it is the right account
```

Create the storage:

```
npx wrangler kv namespace create GLYPH_KV
npx wrangler d1 create glyph
npx wrangler r2 bucket create glyph-covers     # optional, cover cache
```

Then edit `wrangler.toml`. **The checked-in file contains the IDs and domain of
the public instance; replace every one of them:**

| Setting | Set it to |
| --- | --- |
| `[[kv_namespaces]] id` | id printed by `kv namespace create` |
| `[[d1_databases]] database_id` | id printed by `d1 create` |
| `[[r2_buckets]]` | keep if you created the bucket, delete the block otherwise (covers are then off) |
| `routes` | your API host, e.g. `{ pattern = "api.example.com", custom_domain = true }`; delete it to use only workers.dev |
| `PUBLIC_ORIGIN` | `https://api.example.com` (the address the login page is served from) |
| `ACCOUNT_RP_ID` | the domain passkeys belong to, e.g. `example.com` (see section 4) |
| `RATE_LIMIT_PER_MIN` | general API limit per client and minute (default 60) |
| `YOUTUBE_DAILY_BUDGET_UNITS` | YouTube Data API budget (default 9000) |

Create the tables and deploy:

```
npx wrangler d1 migrations apply glyph --remote
npx wrangler deploy
```

Check it: `curl https://api.example.com/v1/health` should report
`"accounts":true` (and `"covers":true` with R2). Accounts switch on only when
the `GLYPH_DB` binding and `PUBLIC_ORIGIN` both exist; otherwise the app hides
the account page.

### Updating

Pull the new code, then always run the migrations before the deploy (new code
may read new columns):

```
npx wrangler d1 migrations apply glyph --remote
npx wrangler deploy
```

Migrations live in `crates/glyph-server-worker/migrations/` and are applied in
order; D1 remembers which ones already ran.

## 4. Domain

1. Register a domain anywhere (the registrar does not matter).
2. In the Cloudflare dashboard: *Add a site* -> **Connect a domain** (not
   "Transfer"), Free plan. Cloudflare shows two nameservers.
3. At your registrar, replace the nameservers with those two. Turn DNSSEC off at
   the registrar first if it is on. Wait until the zone shows **Active**.
4. Put the `routes` entry from section 3 into `wrangler.toml` and deploy.
   Cloudflare creates the DNS record and the TLS certificate for the Worker
   Custom Domain by itself; you do not add DNS records by hand.

Use a subdomain for the API (`api.example.com`) so the bare domain stays free
for a website.

**Passkey domain (`ACCOUNT_RP_ID`).** A passkey only works for the domain it was
created on. Set `ACCOUNT_RP_ID` to the bare domain (`example.com`): passkeys then
work on every subdomain, and later for mobile apps. It must be the host of
`PUBLIC_ORIGIN` or a parent of it; anything else is ignored and the host is
used. **Changing it later invalidates every passkey.** Users still get in with
their password, a pairing code or their recovery code, then add a new passkey.

Mobile apps need their WebAuthn origins (`android:apk-key-hash:...`) in the
`ACCOUNT_EXTRA_ORIGINS` variable (comma-separated).

## 5. How accounts work (short version)

Full design: `docs/ACCOUNTS.md` in the app repository.

* **Handle**: `@name`, 3-20 characters `a-z 0-9 _`, unique, changeable once per
  30 days; a released handle stays blocked for 30 days.
* **Password**: the device computes `key = Argon2id(password, salt)` (19 MiB,
  t=2, p=1, 32 bytes; the login page does the same in the browser). The server
  stores the salt and SHA-256 of the key. The password itself never reaches the
  server, and the Worker never runs Argon2 for user logins (CPU limit).
* **Passkeys**: WebAuthn ES256/RS256, user verification required; only the
  public key is stored.
* **Pairing code**: a new device shows a code, a signed-in device approves it.
* **Recovery code**: shown once at sign-up, stored as a hash.
* **Tokens**: per device an access token (1 h) and a refresh token (90 days,
  rotated on every use). Only SHA-256 hashes are stored.
* **Sync**: last writer wins per item, deletions are tombstones kept for 180
  days.

## 6. Admin CLI and the device limit

The admin API (`/admin/v1/*`) is off until you set an admin password hash:

```
glyph hash-password                 # asks for a password, prints a hash
npx wrangler secret put ADMIN_PASSWORD_HASH  # paste the hash
```

The password itself is stored nowhere. Then, from any machine:

```
glyph remote login https://api.example.com
glyph remote status
```

The login keeps only a refresh token in the OS keychain. Admin logins are rate
limited and locked for 15 minutes after 5 wrong passwords; the admin API only
answers over HTTPS.

Note: the admin login verifies the Argon2id hash on the Worker. That is heavier
than the 10 ms CPU of the Workers free plan allows per request; if the login
fails with a CPU error there, it needs the Workers Paid plan.

### Device limit

| Command | Effect |
| --- | --- |
| `glyph remote accounts default-limit` | show the default |
| `glyph remote accounts default-limit 5` | set the default for every account without its own limit (1-100) |
| `glyph remote accounts limit alice 10` | give `@alice` her own limit |
| `glyph remote accounts limit alice default` | back to the default |
| `glyph remote accounts show alice` | limit in force, whether it is her own, her devices |

Until you set one, the default is 5. It is stored in the D1 table `settings`,
per-account limits in `users.device_limit` (0 = default), so changes apply at
once without a deploy. Lowering a limit keeps existing devices signed in; no new
device can sign in until the account is below it. At the limit the login page
and the pairing flow let the user remove a device first.

### Other limits

`glyph remote limits` shows and changes the general limits, e.g.
`glyph remote limits --rate 120 --import-pages 3`.
`glyph remote companion off|on` switches the yt-dlp companion
(YouTube); while off, YouTube search is empty and YouTube audio answers 403.
On the Worker, `--audio-days 15 --audio-max-gb 10` set how long and how much
companion audio stays in R2 (oldest songs go first). The R2 lifecycle rule
`expire-audio` on `audio/` (15 days) is the backstop; raise it too if you set
more days.

On the native server they are saved in its config file. On the Worker they
are saved in D1 (table `server_settings`, migration 0007) and override the
`wrangler.toml` variables; each Cloudflare location picks a change up within
30 seconds.

## 7. Built-in limits

Fixed in code (`glyph-server-core/src/accounts/mod.rs`); change them there if
your instance needs other values.

| What | Limit |
| --- | --- |
| Handle availability checks, salt lookups | 30 / min per IP |
| Sign-in starts | 10 / 10 min per IP |
| Sign-ups | 5 / h per IP |
| Password sign-ins | 20 / 10 min per IP and 10 / h per handle |
| Passkey ceremonies | 20 / 10 min per IP |
| Recovery-code sign-ins | 5 / 10 min per IP and 5 / h per handle |
| Pairing approvals | 10 / min per account |
| Adding a passkey to an account | 10 / 10 min per account |
| Token refresh | 30 / min per IP |
| Pending sign-in | expires after 10 min |
| Sync push | 500 items and 2 MB per request, 1 MB per item |
| Own playlist covers | 512 KB, JPEG/PNG/WebP |
| Cached provider covers | 2 MB, allow-listed hosts only |
| Devices per account | operator setting, 1-100 (section 6) |

Rate limits use the Cloudflare Cache API, which is per data center, so they are
approximate. Cloudflare's own free-plan limits also apply: 100,000 requests a
day, D1 5 GB and 100,000 row writes a day, R2 10 GB.

## 8. Privacy and your duties as operator

Stored per account: handle, password salt and key hash, passkey public keys,
recovery-code hash, device name, platform, first and last seen, and the synced
library items (liked songs, playlists, followed and hidden artists, saved
albums, recent searches, the download list), app settings, and the user's API
keys as an end-to-end encrypted blob (encrypted on the device with a key from
the account password; the server cannot open it). Not stored: passwords, IP
addresses, user agents, listening history, audio.

Users can export their data (`GET /v1/sync/export`, API only for now) and
delete their account (Settings -> Account), which removes all rows and their R2 folder
at once.

If you open your instance to other people, you are the operator of their
personal data: depending on your country you may need a privacy notice and an
imprint. Back up D1 if accounts matter to you (`npx wrangler d1 export glyph
--remote --output backup.sql`).

## 9. Native server

The standalone binary (`glyph-server-native`) serves the music API but no
accounts yet: `/v1/health` reports `accounts: false` and the app hides the
account page. Accounts need the Worker with D1 for now.

## Optional shared server access

A self-hosted server stays public by default. To require a separate shared bearer
for app access, run `glyph access --config /path/to/server.toml` and enter
a random 32-256 character token twice. Only its SHA-256 hash is stored in the
private config. Restart the server to apply. `glyph access --disable`
removes this requirement (environment overrides must also be removed).

For Docker/scripts set `GLYPH_SERVER_BEARER` in the server environment. For a
Worker use `npx wrangler secret put GLYPH_SERVER_BEARER`, then deploy. Do not put
the token in wrangler.toml or shell arguments. Use HTTPS, except on loopback.

`GET /v1/health` stays public and advertises `server_auth_required: true`.
In the app, enter the server URL: its health response reveals the access-token
field only when required. Paste the token and choose Apply. It is stored per
server URL in the OS keychain, excluded from settings sync/backup, and sent as
`X-Glyph-Server-Authorization: Bearer <token>`. This independent header leaves
`Authorization: Bearer <account-token>` available for personal account sessions.
The shared bearer never grants admin access or access to another user's account.
Custom clients need the server header on every API, media, image and download
request; query strings and cookies are not accepted. The server checks access
before reading media caches or contacting providers. Browser account pages on a
private server also need this header (e.g. via an operator-managed access proxy).
The app uses its direct account/password endpoints; it does not bypass the gate.

To rotate: configure a new bearer, restart/redeploy, then enter the new token in
the app. The public Glyph service does not enable this optional gate.

Device lists store only an approximate region/country from Cloudflare edge metadata, refreshed with device activity. Raw IPs and coordinates are not persisted. Other hosts without trusted geographic metadata display location unavailable.

Profile avatar/banner uploads are decoded on the server with format, byte, dimension and allocation limits and always re-encoded as static WebP before storage. Client conversion is only an upload optimization; MIME headers and image signatures alone are never trusted. The server strips metadata, caps avatar dimensions at 512 pixels and banners at 1500x500, and reduces dimensions further if its encoded output exceeds 512 KiB.
