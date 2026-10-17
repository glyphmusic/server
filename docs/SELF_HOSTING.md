# Self-hosting glyph-server

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
glyph-server hash-password                 # asks for a password, prints a hash
npx wrangler secret put ADMIN_PASSWORD_HASH  # paste the hash
```

The password itself is stored nowhere. Then, from any machine:

```
glyph-server remote login https://api.example.com
glyph-server remote status
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
| `glyph-server remote accounts default-limit` | show the default |
| `glyph-server remote accounts default-limit 5` | set the default for every account without its own limit (1-100) |
| `glyph-server remote accounts limit alice 10` | give `@alice` her own limit |
| `glyph-server remote accounts limit alice default` | back to the default |
| `glyph-server remote accounts show alice` | limit in force, whether it is her own, her devices |

Until you set one, the default is 5. It is stored in the D1 table `settings`,
per-account limits in `users.device_limit` (0 = default), so changes apply at
once without a deploy. Lowering a limit keeps existing devices signed in; no new
device can sign in until the account is below it. At the limit the login page
and the pairing flow let the user remove a device first.

### Other limits

`glyph-server remote limits` shows and changes the general limits on the
native server. On the Worker these are the `wrangler.toml` variables
(`RATE_LIMIT_PER_MIN`, `YOUTUBE_DAILY_BUDGET_UNITS`); change them there and
deploy.

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
