# glyph-server

See [CHANGELOG.md](CHANGELOG.md) for version history.

Open-source metadata server of the [Glyph](https://github.com/glyphmusic/apps) music app.
It holds the API keys, merges search results from many sources, resolves each song to a
playback source, and caches metadata. The Cloudflare Worker is the app's central
API and streams/caches audio too. A private native companion runs yt-dlp when
YouTube search/audio is enabled; the app only talks to the Worker.

Two ways to run the same code (`glyph-server-core`):

| Target | Crate | Where |
|---|---|---|
| Cloudflare Worker | `glyph-server-worker` | Central API, audio proxy and edge cache |
| Standalone binary | `glyph-server-native` | Linux, macOS, Windows, Docker |

**Run your own:** [docs/SELF_HOSTING.md](docs/SELF_HOSTING.md) walks through
the Cloudflare setup, your own domain, accounts and sync, the admin CLI, the
device limit and every server-side limit.

## Install the Glyph CLI

```sh
npm install --global glyph-server
glyph help
glyph version all
glyph check companion
```

Node.js 22+; no Rust toolchain is required. `glyph-server` remains an alias.
Use `glyph setup cloudflare` to create **your own** deployment with the bundled
Worker, migrations and your Cloudflare account. R2 storage is optional. To host a
native server instead, use `glyph setup` and `glyph serve`. See
[npm/README.md](npm/README.md) for all version, check, Docker and update commands.

## API (v1)

| Endpoint | Description |
|---|---|
| `GET /v1/health` | `{server_version, protocol_version, providers, capabilities}` (not rate limited) |
| `GET /v1/search?q=` | NDJSON stream of `SearchEvent` (`results`, `provider_error`, `done`) |
| `POST /v1/resolve` | `ResolveRequest` -> `ResolveResponse` (ISRC first, then title+artist, duration +-3 s) |
| `GET /v1/lyrics?title=&artist=&album=&duration=` | LRCLIB; `duration` in seconds |
| `GET /v1/similar?artist=&title=` | Last.fm `track.getSimilar` |
| `POST /v1/import/spotify` | `{url}` of a public Spotify, Deezer or YouTube playlist |
| `POST /v1/cover` | Track -> cover URL (provider metadata lookup stays on the server) |
| `GET /v1/upstream?url=` | Worker-only allowlisted cover/artist metadata and image proxy |
| `GET /v1/audio/:provider/:id` | Streaming audio, byte ranges; edge-cached on Workers, direct on the binary |
| `POST /v1/download` | Track or ResolveRequest -> download stream reference, or null |
| `GET /v1/download/:provider/:id` | Streaming proxy for permitted downloads |
| `POST /v1/share` | Signed in: `{code, name?, songs?, kind?}` -> `{id, url}` short playlist link |
| `GET /v1/share/:id` | `{code, name, songs, kind, author}`; `DELETE` (owner) removes it |
| `GET /p/:id` | Landing page of a short link; links are deleted with their account |

Types come from the shared `glyph-protocol` crate. Errors are `ApiError` JSON; rate limits
answer `429` with `retry_after_s` and a `Retry-After` header. Clients send an anonymous
`x-glyph-install-id` header; limits apply per install id and (4x looser) per IP. Default 60 requests/min.

Search streaming note: a track keeps its identity (`source`, `id`) once sent. If another
provider returns the same song later, the updated track (with more `external_ids`) is sent
again; clients replace it by (`source`, `id`).

Resolver embed ids: YouTube = video id, SoundCloud = numeric track id. Audius/Jamendo/Archive
return stream references; Deezer returns the official 30 s preview. With `proxy_audio`
(on by default, always on Workers) every stream/preview reference points to
`/v1/audio/...`: the server resolves the provider URL itself — yt-dlp for YouTube —
and streams the bytes with byte-range support, so the app never sees a short-lived
provider URL and can seek normally. SoundCloud keeps its official embed.
Deezer preview signatures are refreshed from the track API on every resolve,
including cached mappings; cached search previews are never used for playback.
Preview resolution is bounded by `provider_timeout_ms`.

For YouTube, download lookup uses the same resolver and companion stream as playback;
only the client stores the offline file. `quality=low|standard|high` works on
`/v1/download/youtube/<id>` as on the playback endpoint. Other providers retain
their existing explicit download checks.

`ResolveRequest.youtube_only` (optional, default false) requires YouTube for
playback and download lookup. It uses a separate mapping cache, adds targeted
search variants, and allows up to 45 seconds for the companion search (55 seconds
for the Worker's remote call). A miss returns no source, never another provider
or a short preview. Older clients can continue sending a plain Track to download lookup.

## Providers and keys

| Provider | Needs | Secret name(s) |
|---|---|---|
| Audius, Internet Archive, Deezer, LRCLIB | nothing | - |
| YouTube search/audio | native yt-dlp or the Worker's private companion | `YTDLP_SERVICE_URL`, `YTDLP_SERVICE_TOKEN` (Worker); `YTDLP_PATH` (native) |
| YouTube playlist import | API key | `YOUTUBE_API_KEY` |
| Jamendo | client id | `JAMENDO_CLIENT_ID` |
| SoundCloud | client credentials | `SOUNDCLOUD_CLIENT_ID`, `SOUNDCLOUD_CLIENT_SECRET` |
| Spotify (metadata only) | client credentials | `SPOTIFY_CLIENT_ID`, `SPOTIFY_CLIENT_SECRET` |
| Last.fm | API key | `LASTFM_API_KEY` |

Providers without keys are simply disabled (`/v1/health` lists the active ones). Keys are
never returned in responses or error messages. A provider that errors or runs out of quota
is skipped for 10 minutes.

### Quotas

* **YouTube search uses yt-dlp, no key and no quota.** The server runs
  `yt-dlp --dump-json` locally, so nothing is downloaded: only ids, titles,
  channels, durations and thumbnails are read. Worker audio uses the private
  companion and is streamed through the Worker. Install yt-dlp (`pip install -U yt-dlp`, a package
  manager, or a release binary from https://github.com/yt-dlp/yt-dlp); point
  `ytdlp_path` in `server.toml` at it if it is not in `PATH`. Without yt-dlp
  YouTube is simply not offered. Cloudflare Workers use the companion because
  the Worker runtime cannot spawn processes.
* **YouTube Data API v3 (playlist import): 10,000 units/day.** The server stops
  importing after `YOUTUBE_DAILY_BUDGET_UNITS` (default 9000) units per day.
  Prefer other sources first (default resolver order is Audius, Jamendo,
  SoundCloud, YouTube).
* Spotify may restrict apps in development mode and some playlist endpoints; check Spotify's
  developer policy before enabling the import for the public.

## Option A: Cloudflare Worker

1. Fork this repo. You also need `rustup target add wasm32-unknown-unknown` and Node 18+.
2. `cd crates/glyph-server-worker`
3. Create the KV namespace and put its id into `wrangler.toml`:
   `npx wrangler kv namespace create GLYPH_KV`
4. Set the secrets you have: `npx wrangler secret put YOUTUBE_API_KEY` (repeat per name).
5. Local test: copy `.dev.vars.example` to `.dev.vars`, fill it, run `npx wrangler dev`.
6. Deploy: `npx wrangler deploy`. Put the resulting URL into the app's `server_url`.

Cloudflare free plan notes: 100,000 requests/day, 10 ms CPU per request, 50 subrequests per
request (the playlist import is capped at `import_max_pages` = 5 pages for that reason) and
1,000 KV writes/day. To stay inside the KV limit only long-lived entries (resolver mappings,
lyrics, 1 h or more) are written to KV; short-lived metadata uses
the free Cache API, which is per data center. Rate limits are therefore approximate.

### YouTube companion for the Worker

1. On a host with process support, install `glyph-server`, current yt-dlp and a
   supported JavaScript runtime. See the [yt-dlp installation instructions](https://github.com/yt-dlp/yt-dlp#installation).
2. Generate a random secret of at least 32 characters. Set `YTDLP_SERVICE_TOKEN`
   on that host, and `YTDLP_PATH` if the executable is not in PATH.
3. Run `glyph-server --bind 127.0.0.1:8787` behind an HTTPS reverse proxy/tunnel
   that forwards `/internal/*`. With the token set, the binary acts as a companion:
   ordinary public API routes are disabled except `/v1/health`.
4. Set the Worker's secrets with `wrangler secret put YTDLP_SERVICE_URL` and
   `wrangler secret put YTDLP_SERVICE_TOKEN`. Use the HTTPS base URL of the
   companion and the same secret. Deploy the Worker after configuring both.
5. For `glyph remote companion status|update`, generate a second, different
   secret and set it as `COMPANION_ADMIN_TOKEN` on the companion host and the
   Worker. Without it, management is off and the service secret cannot
   install anything.

The Worker never sends either secret: it signs each request (HMAC-SHA256 over
method, path, timestamp and body hash, `companion_auth.rs`). The companion
refuses changed requests, timestamps more than a minute off and signatures it
has already seen. Older Workers' plain bearer requests are accepted for search
and audio until `GLYPH_COMPANION_SIGNED_ONLY=1` is set on the companion; update
the companion first, then the Worker. Optional Worker secrets
`COMPANION_ACCESS_CLIENT_ID` and `COMPANION_ACCESS_CLIENT_SECRET` add a
Cloudflare Access service token, so only the Worker reaches the companion's
hostname.

The app keeps the Worker's URL in `server_url`. It never receives the companion
secret. The helper accepts only Glyph's fixed YouTube metadata/URL operations,
validates IDs, limits extraction to 20 seconds and allows four concurrent jobs.
It fetches audio on the extraction host (signed URLs may depend on that host's IP)
and forwards bytes to the Worker. No paid service or account is provisioned by
these code changes.

The Worker serves audio as it arrives and writes a full copy to the Cloudflare
Cache API in parallel. Completed objects support range requests; cold range
requests are sliced while the full object fills the cache. Cache keys use the
provider/track ID rather than an expiring signed URL. Objects are limited to
32 MiB, with a one-hour edge-cache TTL; caches are per data center and best effort,
not durable offline storage. Failed/partial responses are not cached. Unknown
length streams are bounded but not cached. Background fills can be interrupted
by Cloudflare's request-lifecycle limits. Audio is never written to Workers KV.

Abuse protection treats every client as untrusted. `RATE_LIMIT_PER_MIN` is the
anonymous metadata budget per IP and installation. Only an unexpired server-side
account session grants 4x that budget per account and registered device. Every
caller also shares a fixed 8x IP ceiling; choosing an installation ID never raises
it. Media has separate budgets multiplied by 20 for seeks and cover loads.
Twelve rejected quota checks within 15 minutes trigger a 15-minute temporary ban
of the offending IP, account, or device/installation. Responses use HTTP 429 and
`Retry-After`; bans apply across metadata and media routes. Guests behind a NAT
share an IP budget. Installation IDs are resettable signals, not hardware proof;
Glyph does not collect hardware serial numbers. Health and OPTIONS are exempt.

Native quotas/bans live in bounded process memory and reset on restart. Worker
quotas/bans use atomic D1 updates shared across instances and locations. Before
rolling out this version, bind `GLYPH_DB` and apply migration `0009_abuse_limits.sql`
with `npx wrangler d1 migrations apply glyph --remote`. Missing/unavailable storage
fails closed for protected requests. D1 operations consume database quotas, so
edge request filtering remains useful for large floods. Expired rows are cleaned
in bounded batches, and stored principal keys are SHA-256 hashes.

Metadata work uses eight active slots and at most 64 waiting requests, with a
15-second queue timeout. Under contention three account requests are admitted
per guest request; FIFO applies within each class. Streaming searches retain
their slot until completion or disconnect. The queue is per native server or
Worker isolate, not a global Cloudflare queue. Existing native audio concurrency
limits remain separate. Account sessions never grant admin access.

The proxy only follows HTTPS redirects to
known provider hosts and never forwards browser cookies or authorization headers.
The companion secret is sent only to its configured HTTPS endpoint, without redirects.

With a server configured, the app sends search, resolve, lyrics, similar tracks,
imports, cover/artist lookups and download lookups through it. Provider failures
do not silently send requests directly. Explicit `server_url = "off"` retains
direct mode; existing local files/caches can still be read offline. SoundCloud's
official embed and application updates remain separate from the audio proxy.

## Option B: native binary (Linux, macOS, Windows)

Download an archive from the releases page, or build: `cargo build --release -p glyph-server-native`.

```
export YOUTUBE_API_KEY=...        # any of the secrets above
glyph-server --bind 0.0.0.0:8787
```

Configuration: environment variables, or `server.toml` (see `server.example.toml`; env wins).
Optional disk cache: `GLYPH_CACHE_DIR` / `cache_dir`. Behind a reverse proxy set
`GLYPH_TRUST_PROXY=1` only with a reverse proxy on loopback. Forwarded headers
from remote peers are ignored. The local proxy must overwrite incoming client-IP
and HTTPS headers; for `X-Forwarded-For` it must supply the actual client IP first.

Updates: `glyph-server --check-update` and `glyph update` use the GitHub releases
of `GLYPH_SERVER_REPO` (default `glyphmusic/server`), expect the asset
`glyph-server-{version}-{target}.zip` (Windows) or `.tar.gz` plus `SHA256SUMS.txt`
and `SHA256SUMS.txt.minisig`, check the signature by the Glyph release key
(`release-key.pub`, trusted comment `glyph-release repo=<repo> tag=<tag>`), then the
checksum, and replace the running binary. Unsigned releases are refused. Release
workflows sign with the repository secret `GLYPH_RELEASE_KEY`
(`scripts/sign-release.sh`). No telemetry.

Docker: `docker build -t glyph-server .` then
`docker run -p 8787:8787 -e YOUTUBE_API_KEY=... glyph-server`.

## Development

### Optional account-only API

API access is public by default. On a server with accounts enabled (currently
the Cloudflare Worker with `GLYPH_DB` and `PUBLIC_ORIGIN`), enable the signed-in
account requirement through the authenticated admin CLI:

```
glyph remote limits --require-account true
glyph remote limits --require-account false
```

The setting is persisted with the other server limits. Worker locations pick up
remote changes within 30 seconds. Alternatively set Worker variable
`REQUIRE_ACCOUNT="true"`; explicitly saved admin settings take precedence.
Native configuration accepts `require_account` / `GLYPH_REQUIRE_ACCOUNT`, but
without an account backend protected requests fail closed with HTTP 503; the
admin API refuses to enable this mode on such a server.

When enabled, every `/v1/*` API (including audio, downloads and warm caches)
requires a valid, unexpired account access token in `Authorization: Bearer ...`.
Guests, forged tokens and revoked sessions receive HTTP 401 `account_required`.
Account login/registration/refresh, GET health and OPTIONS remain reachable;
public website/share pages retain their existing behavior. Admin and companion
endpoints keep their own authentication. The independent shared server access
token still applies when configured. Health advertises `account_required: true`.

To keep only YouTube for accounts (guests keep every other source), use
`glyph remote limits --youtube-require-account true` or the Worker variable
`YOUTUBE_REQUIRE_ACCOUNT="true"`. Guests then get HTTP 401 `account_required`
on `/v1/audio/youtube/*` and `/v1/download/youtube/*`; health lists the
capability `youtube_account_required`.

### Tests

```
cargo test --workspace
cargo check -p glyph-server-worker --target wasm32-unknown-unknown
```

The real encoder regression test additionally requires `GLYPH_FFMPEG_PATH`
pointing to an FFmpeg executable:

```
cargo test -p glyph-server-native real_encoder_streams -- --ignored
```

It verifies audio delivery before source EOF, decoding with Glyph's Symphonia
decoder, completed byte ranges, and the fallback for tail-indexed MP4 input.

Worker streaming integration tests (local workerd, mocked providers):

```
cd crates/glyph-server-worker
worker-build --dev --no-opt
npm ci
npm test
```

The shared `glyph-protocol` snapshot is included in `release-protocol/`: this
repository builds by itself. Maintainers can refresh the snapshot from an adjacent
desktop checkout with `node scripts/snapshot-protocol.mjs`.

Provider metadata uses official APIs; YouTube extraction uses the separately
configured yt-dlp helper. Provider keys and companion tokens stay in secrets/env.

## License

MIT OR Apache-2.0, see `LICENSE-MIT` and `LICENSE-APACHE`.


## Storage ceilings and scheduled maintenance

D1 and R2 have independent storage budgets. Configure both with the native CLI:

```sh
glyph remote limits --server https://api.glyph.my --d1-max-mb 500 --r2-max-gb 10
glyph remote limits --server https://api.glyph.my
```

`--audio-max-gb` remains an alias for `--r2-max-gb`. The JSON field
`audio_cache_max_gb` is retained for compatibility, but now covers all objects in
this Glyph R2 bucket, including protected user images. Only `audio/` and `covers/`
are automatically evictable. `users/` and unknown prefixes are never deleted for
space. Values are decimal MB/GB. The R2 working target is 500 MB below the ceiling
(or half the ceiling for smaller programmatic caps): 10 GB means 9.5 GB usable.
D1 reserves 10 percent, capped at 50 MB: 500 MB means a 450 MB working target.
Cloudflare's plan/account limits still apply independently of these app settings.

A once-per-hour R2 inventory lists actual objects with pagination and reconciles
sizes, including objects missing from the old ledger. Minute cron ticks resume
bounded inventory/eviction batches after interruptions. Expired cache objects go
first; otherwise less-requested objects are deleted first, with last access as a
tiebreaker. Counts are incremented on actual R2 reads. No new objects are admitted
while inventory is incomplete. Atomic D1 reservations charge actual upload bytes
before R2 writes, including concurrent uploads; failed R2 deletions never release
bytes. If only protected data or in-flight reservations remain, further writes
are refused instead of deleting user data. Lower ceilings can require several
maintenance batches to drain an existing cache. Existing reads continue.

D1 checks use the database's actual `size_after` metadata, plus conservative
reservations for concurrent SQLite page/index growth. Periodic cleanup removes
expired abuse counters, pending logins and handle holds. It never discards live
accounts, credentials, libraries or sync tombstones. Deleting rows does not
necessarily shrink the physical database immediately. If cleanup cannot restore
headroom, writes stop; authenticated admin login/refresh/limit changes remain
available through a separate limiter. R2 reservations and D1 envelopes left by
uncertain failures/canceled writes remain charged conservatively and are logged;
manual reconciliation may be needed rather than automatically risking orphaned
uploads or deleting personal data.

Before deploying, apply migrations through `0013_audio_variant_jobs.sql`:

```sh
cd crates/glyph-server-worker
npx wrangler d1 migrations apply glyph --remote
```

The Worker cron is configured in `wrangler.toml`; deploying activates it. These
checks cover this Glyph database and bucket, not other projects in the account,
Workers KV, local application downloads, billing, or network egress. Native
servers do not store R2 audio and reject non-default cloud storage settings.
The YouTube Data API budget is now checked before every supported API read and
resets at UTC midnight; yt-dlp search is outside that API quota.


## Progressive audio quality variants

New app builds offer Auto for streaming and downloads. Auto uses only recent
measured transfers (no speed-test traffic), begins at Standard when unknown,
downgrades quickly, and requires repeated fast samples before choosing High.
Manual quality is binding. Auto streaming can switch during a song: stalled
reads downgrade after a minimum five-second interval, while upgrades wait at
least 45 seconds. Each change reopens the decoder and seeks by timestamp, with
a possible brief buffering pause. Auto skips switches near the end of a song.
Downloads choose once when the job starts. Streaming Range chunks adapt during playback: 64 KiB at
startup, then roughly two seconds of measured throughput, bounded to 32–384 KiB.
On slow/unknown connections the app avoids a second full background stream-cache
download competing with playback. Locally downloaded files still play offline.

For Worker/companion YouTube audio, `?quality=low|standard|high` selects 48 kbps AAC,
96 kbps AAC, or the best source AAC (normally around 128 kbps). High remuxes M4A
without another lossy encode; other supported containers are converted to AAC.
FFmpeg is required on the companion (`GLYPH_FFMPEG_PATH` can override its path).
The companion Dockerfile already includes it. Three encoders maximum (at most two
background jobs), one FFmpeg thread each, a 90-second overall deadline and a
32 MiB media ceiling bound the work. Requests for the same track and quality
share one encode, including its already-produced prefix. Low/Standard output is
fragmented MP4: download, encoding and playback overlap rather than waiting for
the complete song. MP4 inputs with their index at the end require a bounded
temporary input file; encoding output still streams. Only successful complete
representations enter the memory/R2 caches. Cold streams have no Content-Length;
completed cached representations support exact byte ranges.

The requested variant is generated and cached as content-addressed 1 MiB R2
objects; another quality is prepared when requested. Cold tracks may need
preparation time; a busy companion, full storage or the Worker's bounded
background lifetime can defer cache persistence until a later request. Direct
requests for another quality work independently. Every variant byte counts toward the
same R2 ceiling. A D1 manifest is published
only after all chunks have uploaded. Each playback range reads only the needed
part of the relevant chunks from R2. Different qualities never mix MP4 byte
positions in one stream. Missing/expired chunks trigger a companion refresh;
failed/incomplete writes retain reservations conservatively. Retention and
popularity-based eviction apply to these chunks too. This is progressive byte-
range playback with timestamp-based decoder replacement, rather than a seamless
HLS/DASH player. A provider that offers no alternate encoding retains its own
format. Standalone native servers honor the same quality parameter, using a
bounded 64 MiB, eight-entry, ten-minute memory cache rather than R2 for encoded
Low/Standard audio. Native High retains the original source and works without
FFmpeg.

Resolver misses use a shared 23-second deadline (50 seconds for YouTube-only
requests), including any companion call. Provider lookups overlap while answers
are considered in configured priority order. Non-YouTube providers wait at most
four seconds; normal YouTube fallback waits at most 18 seconds, leaving time for
the preview fallback. YouTube-only lookups retain a 45-second provider budget.
Audio proxy connections are pooled across requests. Cache popularity statistics
are written in the Worker background rather than delaying cached playback.

D1 limits currently accept 64–4000 MB; the Rust Worker's size metadata uses a
32-bit `usize`, so larger app ceilings would be misleading on wasm32. Provider
plan limits may be lower. D1 size and R2 size are measured separately.
