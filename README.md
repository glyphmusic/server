# glyph-server

Open-source metadata server of the [Glyph](https://github.com/glyphmusic/glyph) music app.
It holds the API keys, merges search results from many sources, resolves each song to a
legal full-playback source, and caches everything. It never proxies audio: responses
contain only metadata, IDs and official stream/embed references.

Two ways to run the same code (`glyph-server-core`):

| Target | Crate | Where |
|---|---|---|
| Cloudflare Worker | `glyph-server-worker` | Cloudflare free plan is enough |
| Standalone binary | `glyph-server-native` | Linux, macOS, Windows, Docker |

## API (v1)

| Endpoint | Description |
|---|---|
| `GET /v1/health` | `{server_version, protocol_version, providers}` (not rate limited) |
| `GET /v1/search?q=` | NDJSON stream of `SearchEvent` (`results`, `provider_error`, `done`) |
| `POST /v1/resolve` | `ResolveRequest` -> `ResolveResponse` (ISRC first, then title+artist, duration +-3 s) |
| `GET /v1/lyrics?title=&artist=&album=&duration=` | LRCLIB; `duration` in seconds |
| `GET /v1/similar?artist=&title=` | Last.fm `track.getSimilar` |
| `POST /v1/import/spotify` | `{url}` of a public Spotify, Deezer or YouTube playlist |

Types come from the shared `glyph-protocol` crate. Errors are `ApiError` JSON; rate limits
answer `429` with `retry_after_s` and a `Retry-After` header. Clients send an anonymous
`x-glyph-install-id` header; limits apply per install id and (4x looser) per IP. Default 60 requests/min.

Search streaming note: a track keeps its identity (`source`, `id`) once sent. If another
provider returns the same song later, the updated track (with more `external_ids`) is sent
again; clients replace it by (`source`, `id`).

Resolver embed ids: YouTube = video id, SoundCloud = numeric track id. Audius/Jamendo/Archive
return direct official stream URLs; Deezer returns the official 30 s preview.

## Providers and keys

| Provider | Needs | Secret name(s) |
|---|---|---|
| Audius, Internet Archive, Deezer, LRCLIB | nothing | - |
| YouTube Data API v3 | API key | `YOUTUBE_API_KEY` |
| Jamendo | client id | `JAMENDO_CLIENT_ID` |
| SoundCloud | client credentials | `SOUNDCLOUD_CLIENT_ID`, `SOUNDCLOUD_CLIENT_SECRET` |
| Spotify (metadata only) | client credentials | `SPOTIFY_CLIENT_ID`, `SPOTIFY_CLIENT_SECRET` |
| Last.fm | API key | `LASTFM_API_KEY` |

Providers without keys are simply disabled (`/v1/health` lists the active ones). Keys are
never returned in responses or error messages. A provider that errors or runs out of quota
is skipped for 10 minutes.

### Quotas

* **YouTube: 10,000 units/day.** One search costs 100 units (+1 for durations). Results are
  cached for 6 hours and the server stops calling YouTube after `YOUTUBE_DAILY_BUDGET_UNITS`
  (default 9000) units per day. Prefer other sources first (default resolver order is
  Audius, Jamendo, SoundCloud, YouTube) so YouTube is rarely hit.
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

`glyph-protocol` is a path dependency on `../glyph` for now; switch it in the workspace
`Cargo.toml` to `git = "https://github.com/glyphmusic/glyph", tag = "protocol-v1"` for releases.

Hard rules: official APIs only, no scraping, no audio through the server, keys only from
secrets/env.

## License

MIT OR Apache-2.0, see `LICENSE-MIT` and `LICENSE-APACHE`.
