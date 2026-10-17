# glyph-server

Open-source metadata server of the [Glyph](https://github.com/glyphmusic/desktop) music app.
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

## API (v1)

| Endpoint | Description |
|---|---|
| `GET /v1/health` | `{server_version, protocol_version, providers}` (not rate limited) |
| `GET /v1/search?q=` | NDJSON stream of `SearchEvent` (`results`, `provider_error`, `done`) |
| `POST /v1/resolve` | `ResolveRequest` -> `ResolveResponse` (ISRC first, then title+artist, duration +-3 s) |
| `GET /v1/lyrics?title=&artist=&album=&duration=` | LRCLIB; `duration` in seconds |
| `GET /v1/similar?artist=&title=` | Last.fm `track.getSimilar` |
| `POST /v1/import/spotify` | `{url}` of a public Spotify, Deezer or YouTube playlist |
| `POST /v1/cover` | Track -> cover URL (provider metadata lookup stays on the server) |
| `GET /v1/upstream?url=` | Worker-only allowlisted cover/artist metadata and image proxy |
| `GET /v1/audio/:provider/:id` | Streaming audio, byte ranges; edge-cached on Workers, direct on the binary |
| `POST /v1/download` | Track -> explicitly permitted download reference, or null |
| `GET /v1/download/:provider/:id` | Streaming proxy for permitted downloads |

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
lyrics, 1 h or more) are written to KV; everything else, rate-limit counters included, uses
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

Media requests have a separate per-IP budget of 20 times `RATE_LIMIT_PER_MIN`,
so cover loads and range requests do not consume the metadata budget. The Cache
API rate limiter remains approximate. The proxy only follows HTTPS redirects to
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
`GLYPH_TRUST_PROXY=1` so the client IP comes from `X-Forwarded-For`.

Updates: `glyph-server --check-update` and `glyph-server self-update` use the GitHub releases
of `GLYPH_SERVER_REPO` (default `glyphmusic/glyph-server`), expect the asset
`glyph-server-{version}-{target}.zip` (Windows) or `.tar.gz` plus its `.sha256`, verify the
checksum, and replace the running binary. No telemetry.

Docker: `docker build -t glyph-server .` then
`docker run -p 8787:8787 -e YOUTUBE_API_KEY=... glyph-server`.

## Development

```
cargo test --workspace
cargo check -p glyph-server-worker --target wasm32-unknown-unknown
```

Worker streaming integration tests (local workerd, mocked providers):

```
cd crates/glyph-server-worker
worker-build --dev --no-opt
npm ci
npm test
```

`glyph-protocol` is a path dependency on `../glyph` for now; switch it in the workspace
`Cargo.toml` to `git = "https://github.com/glyphmusic/glyph", tag = "protocol-v1"` for releases.

Provider metadata uses official APIs; YouTube extraction uses the separately
configured yt-dlp helper. Provider keys and companion tokens stay in secrets/env.

## License

MIT OR Apache-2.0, see `LICENSE-MIT` and `LICENSE-APACHE`.
