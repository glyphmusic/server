# Changelog

## 0.12.6 - 2026-11-26

### Fixed

- **Multi-source Explicit & unknown ratings**: songs now resolve a three-state explicit rating (`Explicit`, `Clean`, `Unknown`) combining Deezer explicit codes, Apple Music/iTunes metadata and LRCLIB lyrics scanning (with `/v1/lyrics` feeding back into the shared rating cache), so mislabeled explicit songs are caught while genuinely unrated uploads keep `?`.
- **Service-based artist verification (`POST /v1/artists/meta`, capability `artists_meta`)**: replaced the arbitrary fan-count cutoff with service-level verification signals (Audius, SoundCloud, Deezer, Apple Music/iTunes, Spotify and Wikipedia), served in batched lookups with 14-day caching and without caching transient errors as unverified.
- **Complete server-side artist catalog caching & incremental album updates (`/v1/artist`)**: on first visit, the server indexes popular songs plus discography tracklists, caches total unique song and release counts, genre tags, links and related artist metadata for 30 days, and automatically checks every 12 hours to incrementally merge newly released albums and singles on the server.

### Changed

- **Protocol & changelog snapshots**: updated `release-protocol` to `0.14.1` and refreshed embedded desktop (`0.14.1`) and server (`0.12.6`) changelogs.

## 0.12.5 - 2026-11-25

### Fixed

- **App versions in the admin dashboard**: the list of app releases loads again. When GitHub's API refuses the server (it allows only a few requests per hour without a key, shared by many Cloudflare Workers), the server reads GitHub's release feed instead, and otherwise shows the last list that worked.

## 0.12.4 - 2026-11-25

### Fixed

- **AI assistant sign-in**: an approved sign-in request can no longer be moved to another account by typing a different @handle afterwards, so nobody can get an assistant token for someone else's account.
- **Rate limits for IPv6 clients**: all addresses of one IPv6 /64 network now count as one client, so changing addresses inside it no longer skips the sign-up, sign-in, admin login and API limits.
- **Cover cache**: fetching new covers is limited per client, upstream answers over 32 MB are refused instead of being held in memory, and the self-hosted server no longer follows redirects to this machine or its local network.
- **Admin dashboard**: an admin account can no longer act on another admin when the account lookup fails for a reason other than "not found".
- **Taste suggestions**: artist names sent by apps lose control characters before other users see them.

## 0.12.3 - 2026-11-23

### Added

- **Server-side Taste Engine** (`POST /v1/taste/recommend`, capability `taste_engine`): intent-aware recommendations for Home, Daily Mixes and Radio. It is purely additive: existing endpoints and the wire protocol version are unchanged, so apps that do not know about Taste keep working as before.

## 0.12.2 - 2026-11-22

### Fixed

- **Faster & more reliable music video resolution (`r-dlp` workers & Worker validation)**: the native server now resolves H.264 MP4 video streams and audio together via persistent `r-dlp` workers (when `r-dlp` advertises `video` capability), caching all returned resolutions (`ytvideo:*:v2:*`) in one call with `yt-dlp` fallback; `/v1/video/resolve` defaults to 720p HD with paired audio; and the Cloudflare Worker validates upstream `content-type` (`video/*` / `application/octet-stream`) before streaming.

### Changed

- **Changelog & web player snapshots**: refreshed embedded desktop (`0.13.3`), r-dlp (`0.2.3`) and server (`0.12.2`) changelogs and updated the web player bundle.

## 0.12.1 - 2026-11-22

### Added

- **Music video search, resolution & streaming (`/v1/video/*`, `video` capability)**: added `/v1/video/search`, `/v1/video/resolve` and `/v1/video/youtube/*` stream proxying on both the native server and Cloudflare Worker so desktop, mobile and web clients can search and play music videos with quality selection.

### Changed

- **Protocol & changelog snapshots**: updated `release-protocol` to `0.13.1` and refreshed embedded desktop (`0.13.1`) and server (`0.12.1`) changelogs.

## 0.12.0 - 2026-11-21

### Added

- **Wrong-song feedback (`POST /v1/match/alternatives`, `POST /v1/match/report`)**: apps can ask for ranked replacement versions of a song with 30-second previews and report the right one. Votes are stored per hashed listener and the global match only changes once enough distinct listeners agree and the server verifies the candidate itself against its own provider lookup (`wrong_song` capability, `match_feedback.rs`).
- **Active Jams in the admin API**: the admin status lists running Jam rooms (listeners, queue length, running time, host online) for the in-app Server dashboard.

### Changed

- **Site refresh**: new `/` and `/download` pages, and the web player is served with install-as-app support (manifest and icons).
- **Protocol & changelog snapshots**: updated `release-protocol` to `0.13.0` and refreshed embedded desktop (`0.13.0`) and server (`0.12.0`) changelogs.

## 0.11.10 - 2026-11-21

### Fixed

- **No abuse bans during multi-playlist transfers**: raised the per-minute `import-start` pace limit (20/min for signed-in accounts, 8/min for guests) and return a soft `429 rate_limited` instead of handing out 15-minute `temporarily_blocked` abuse bans during batch playlist transfers.

### Changed

- **Protocol & changelog snapshots**: updated `release-protocol` to `0.12.16` and refreshed embedded desktop (`0.12.16`) and server (`0.11.10`) changelogs.

## 0.11.9 - 2026-11-21

### Fixed

- **Faster playlist imports & YouTube misses**: cached SoundCloud resolve metadata across paginated imports, included total track counts in import pagination, and skipped slow `yt-dlp` fallbacks when native YouTube search returns a valid empty result set (e.g. unindexed ISRCs).
- **R2 cache for `quality=high` audio**: `quality=high` companion audio requests now read from and populate the Cloudflare R2 audio cache instead of bypassing it.

### Changed

- **Native server & snapshots**: improved native server handling (`serve.rs`), updated `release-protocol` to `0.12.15` (`sync_kind::LIKED_CLEAR`), and refreshed embedded desktop (`0.12.15`) and server (`0.11.9`) changelogs.

## 0.11.8 - 2026-11-20

### Added

- **Device app version tracking**: records each device's last reported Glyph app version (`app_version` via `User-Agent` / `X-Glyph-App-Version`, D1 migration `0017_device_app_version.sql`) and surfaces it in the admin dashboard and CLI.
- **Admin API blocks & profile reset**: added `/admin/v1/blocks` and `/admin/v1/blocks/clear` to inspect and lift active abuse blocks, rate limits, YouTube daily limits and login lockouts, plus `/admin/v1/accounts/<handle>/profile/reset` to remove a user's profile picture or banner.

### Changed

- **Protocol & changelog snapshots**: updated `release-protocol` to `0.12.14` and refreshed embedded desktop (`0.12.14`) and server (`0.11.8`) changelogs.

## 0.11.7 - 2026-11-20

### Added

- **In-app server admin dashboard & account admin role**: server owners can grant accounts the admin role (`glyph remote accounts admin <handle> [--off]`, cleared on ban) so they can open the **Server** dashboard in the Glyph app to view accounts, storage usage, 30-day daily history snapshots, device limits, bans, server limits and cache stats (keys, updates, logs and granting admin remain owner-only; admins cannot modify other admins).
- **Server-pinned app versions (`stable_app_version`, `latest_app_version`)**: pin which release the app's Stable and Latest update channels offer via `glyph remote limits --stable-version <ver|none> --latest-version <ver|none>`, `server.toml`, environment variables or the in-app dashboard, exposed in `/v1/health`.

### Changed

- **Unified glyph.my site design**: refreshed the landing page (`/`), `/download`, `/plugins`, `/changelog` and docs pages under the shared app-panel stylesheet, titlebar and mobile menu.
- **Protocol & changelog snapshots**: updated `release-protocol` to `0.12.13` and refreshed embedded desktop (`0.12.13`) and server (`0.11.7`) changelogs.

## 0.11.6 - 2026-11-20

### Added

- **Oldest working app version (`min_app_version`)**: after a change that breaks older apps, set `MIN_APP_VERSION` (Worker variable), `min_app_version` in `server.toml`, `GLYPH_MIN_APP_VERSION` (native server), or live via `glyph remote limits --min-app-version <ver|none>` / `glyph config set min_app_version <ver> --server <url>`. `/v1/health` names it, and API calls of older Glyph apps (by their `User-Agent`) get `426` with the code `app_outdated`, so apps from this version on show "Update Glyph". Health, pages and the web player stay open; browsers and other clients are not affected.
- **Account storage limit (`account_max_mb`)**: each account has a total space cap for synced data and pictures (default 50 MB, configurable via admin limits / `--account-max-mb` / `glyph remote accounts storage-limit <mb>` / `ACCOUNT_MAX_MB`), with no separate playlist count cap. `GET /v1/account/storage` reports `{ used_bytes, limit_bytes }`; writes that would exceed the cap return `413` (`account_full`), while deletes always go through.
- **List accounts and cap total accounts in the CLI**: `GET /admin/v1/accounts` and `glyph remote accounts [list]` (`--search`, `--limit`, `--offset`) list all accounts with device count, sync size, last seen, creation date and ban state; `glyph remote accounts show <handle>` now also reports storage used and ban state. Operators can cap total accounts with `max_accounts` (`glyph remote accounts max-accounts <n>`, `glyph remote limits --max-accounts <n>`, `MAX_ACCOUNTS` / `GLYPH_MAX_ACCOUNTS`, `0` = no limit; sign-ups past the cap get `403 account_limit_reached`). `glyph cloudflare` and `glyph config` now also support `MIN_APP_VERSION`, `ACCOUNT_MAX_MB`, `MAX_ACCOUNTS`, `rdlp_path` and `web_dir`.

### Changed

- **User pictures stored as WebP**: playlist covers and profile pictures saved on the server are stored as WebP (`to_webp`), keeping plain lossy WebP uploads as sent and converting other formats to WebP.
- **Remote MCP plugin tools**: updated the server's protocol snapshot so AI assistants connected through the server relay can also use the new plugin management tools (`list_plugins`, `browse_plugins`, `install_plugin`, `uninstall_plugin`, `enable_plugin`).
- **Share links open the web player**: a `/p/<id>` link opened by a person in a browser now goes on to the web player (`/web/p/<id>`) with the list ready to play. Link previews (bots) still get the page with its card, and a server without the web player keeps showing the page. The page no longer tries to open the app by itself; "Open in Glyph" stays as a button.
- **Album names are remembered for everyone**: when an app asks for a song's album (`/v1/cover`), the server keeps the answer (60 days; "nothing found" 6 hours) and gives it to every app that asks for the same song, without asking Deezer or Spotify again. Album names show up faster, and the server makes far fewer lookups.

## 0.11.5 - 2026-11-19

### Changed

- **Whole artist pages (`GET /v1/artist`)**: the answer now also holds the artist's popular songs (`top`, Deezer top 25), the discography (`releases`: id, title, cover, release day, type, newest first) and related artists (`similar`, which was always empty before). It stays in the cache 14 days (key `artist:v2:...`), so one lookup serves every app: the apps no longer search and ask Deezer and Last.fm themselves for each artist page. Deezer's three extra calls run beside the Wikipedia lookup. Older apps ignore the new fields.

## 0.11.4 - 2026-11-16

### Fixed

- **Playback Poll & Room Routing**: skips contacting the Durable Object on `/v1/devices/playback/poll` when the account only has a single online device (`playback.rs`), and routes `playback:` scopes explicitly to `https://playback-room/*` (`jam_room.rs`).
- **Cover & Album Lookup (`POST /v1/cover`)**: extended cover lookup (`find_cover_meta`) to return both `{ url, album }` metadata so clients can backfill missing album names alongside cover art.

### Changed

- **Worker Storage Split (`GLYPH_CACHE` & `GLYPH_USERS`)**: separated cached covers/audio (`glyph-cache` via `GLYPH_CACHE`) from user uploads (`glyph-users` via `GLYPH_USERS`, retaining `GLYPH_COVERS` fallback) and renamed the Worker config to `server` with `preview_urls = true` (`wrangler.toml`, `docs/SELF_HOSTING.md`).
- **Web player (`/web`)**: embeds the `0.12.4` web player (`glyph-web/`) with Web Connect remote device playback, Home quick-pick cards (`ListCard`), album metadata backfill, and reconnect fixes, plus `glyph-protocol` `0.12.4`.

## 0.11.3 - 2026-11-16

### Fixed

- **Artwork & Deduplication**: prefer non-YouTube cover artwork over YouTube thumbnails when merging search/resolve tracks (`prefer_cover`) and when looking up track covers (keeping YouTube only as a last-resort fallback).
- **Sync Rate Limiting**: gave authenticated `/v1/sync/*` endpoints a dedicated quota without triggering temporary 15-minute global lockout strikes (`admit_rules_checked`), keeping normal authenticated API routes unaffected while rejecting forged tokens with standard limits.

### Changed

- **Web player (`/web`)**: embeds the `0.12.3` web player (`glyph-web/`) with PWA manifest/icons, improved background playback recovery, `429`/sync retry improvements, solid `NowPlayingSheet` background, and startup/canvas guards.
- Updated embedded `glyph-protocol` (`0.12.3`) and repository changelogs.

## 0.11.2 - 2026-11-16

### Changed

- **Web player (`/web`)**: embeds the `0.12.2` web player (`glyph-web/`) with `/v1/cover` lookup for Last.fm/synced tracks, clean `401` token refresh & stale-session recovery on `/v1/account` and `/v1/devices`, progressive playlist/queue loading & offscreen cover unloading, hidden mobile scrollbars, solid mobile bottom bar, and focus/tap box removal.
- Updated embedded protocol (`glyph-protocol` `0.12.2`) and repository changelogs.

## 0.11.1 - 2026-11-16

### Fixed

- **Accounts & Profiles (`/v1/profiles/{handle}/images/{sha256}`)**: allows unauthenticated `<img src>` fetches of stored profile/playlist artwork while keeping private profile JSON and playlists protected behind authentication (`401`).
- **Web player (`/web`)**: embeds the `0.12.1` web player bundle (`glyph-web/`) with YouTube embed compositing fix, Document PiP mini player, queue preloading, batched sync pushes, `/web#mcp-connect=` pairing, and updated landing page screenshots (`?v=0.12.1`).
- **Worker CORS**: allows `PATCH` and `DELETE` preflight methods for MCP session management endpoints.

### Changed

- **Artist metadata & MCP**: adds cached 14-day `GET /v1/artist` endpoint and MCP settings/session protocol updates (`glyph-protocol` 0.12.1).

## 0.11.0 - 2026-11-15

### Added

- **Web player (`/web`)**: serves the Svelte web player (`glyph-web/` static assets on the Cloudflare Worker and optional `--web-dir` / `GLYPH_WEB_DIR` on the native server) with single-page route fallback, a CSP tailored to direct provider playback and the official YouTube embed, and a login link/redirect from the home page.
- **Public charts & accounts**: adds Deezer public chart lookups and `/accounts` password-link and delete endpoints (`LinkPasswordReq`, `LinkDeleteReq`), plus updated profile section ordering (`playlists`, `recent`) and expanded MCP relay schemas (`glyph-protocol` 0.11.0).

### Changed

- Updated landing-page screenshots, plugin/share page SVG icons, and embedded changelogs (including Glyph 0.12.0).
- Dependencies: `lru` 0.19.0 and `toml` 1.2.0+spec-1.1.0; CI and release caching workflows updated (`release-cache.yml`).

## 0.10.0 - 2026-11-14

### Security

- `zip` 9.5.1 for the self-updater (critical advisory GHSA-m665-v7pm-7grj in 9.0.4) and `lru` 0.16.4. The Worker's test tooling uses patched `undici` and `sharp`; neither ships in the Worker.
- CI runs with a read-only token, and the Worker tests check URLs exactly instead of by substring.

### Added

- **Audio behind short links**: `/p/<id>/audio/<n>` (also with a file name or extension, like `/audio/3.m4a`) serves song `n` of a shared link as a plain audio file, named "Artist - Title", so browsers play it and `yt-dlp <link>`, `curl -OJ` and other players can download it. The link page lists the songs as hidden `<audio>` tags for such tools. Listeners need no account of their own: the account that shared the link vouches for them, so its bans and YouTube quota apply. Servers that hand out provider links instead of streaming redirect to the listen page.

### Changed

- Site pages (`/`, `/download`, `/changelog`, `/plugins`) asked for on another host than the share origin (such as `api.glyph.my`) move there, and plugin pages name their canonical address, so search engines index each page once.
- The Worker now answers every path on `glyph.my` (real 404s instead of Cloudflare's 522) and moves `www.glyph.my` to `glyph.my`. `robots.txt` leaves out `/cdn-cgi/`.
- Dependencies: `axum` 0.8, `clap_mangen` 0.3, `dirs` 7, current `tokio`. Release builds pin `worker-build` 0.8.7, which matches the `worker` crate.

## 0.9.0 - 2026-10-25

### Added

- **MCP server for the docs**: `https://docs.glyph.my/mcp` (and `/docs/mcp` on every server) lets AI agents search and read the documentation without signing in. Tools: `search_docs` (best-matching sections with snippet, address and path), `read_doc` (a page or one section as Markdown) and `list_docs` (every page with its description). It reads the same GitHub cache as the docs site, so a docs push shows up within about a minute. `llms.txt` names the endpoint.

## 0.8.0 - 2026-10-23

### Security

- Release files are verified and signed together before publication. Concurrent builds cannot overwrite archives or checksums of a published release.

### Added

- **Explicit rating for YouTube results**: before a search ends, YouTube results no rating source knows are looked up at Deezer and sent again with the Deezer id and its explicit flag. Answers are cached (found: 90 days, not found: 7 days), at most 25 lookups and 6 seconds per search. `/v1/health` lists `ratings`, so the apps stop asking Deezer themselves.

- **Pairing that hands over the library's key**: `/v1/account/login/start` takes an optional `pake` message from the new device, and `/v1/account/login/approve` returns it to the approving device, so the apps can pass the end-to-end encryption key between each other. Pairing codes may have four more characters that only the apps know; the server reads the first eight. `/v1/health` lists `pair_keys`.
- **End-to-end encrypted sync**: the apps send `sealed`, `sealed_playlist` and `keyring` items the server stores without reading. Encrypted playlists get the same revision check as plain ones, so edits from two devices are merged instead of one being lost; `/v1/health` lists `e2e_sync`. No migration is needed.
- **YouTube only for accounts**: `glyph remote limits --youtube-require-account true` (or the Worker variable `YOUTUBE_REQUIRE_ACCOUNT`) keeps YouTube audio and downloads for signed-in accounts, which carry quotas and bans; guests keep every other source.
- `/v1/health` lists `capabilities` by name (`jam`, `mcp_relay`, `youtube_audio`, `youtube_account_required`, ...), so apps test for a feature instead of a version.
- AI assistants connected through the server see only the tools the user allows in the app (Settings › AI assistants); the app refuses the rest.

### Changed

- **Signed companion requests**: the Worker signs each companion request (HMAC with a timestamp) instead of sending `YTDLP_SERVICE_TOKEN`; the companion refuses changed, old and repeated requests. Status and updates need a second secret, `COMPANION_ADMIN_TOKEN`, on the Worker and the companion. Optional `COMPANION_ACCESS_CLIENT_ID`/`_SECRET` let the Worker pass Cloudflare Access in front of the companion. Update the companion before the Worker.
- **Signed releases**: `glyph update`, r-dlp updates and the npm installer install a release only when `SHA256SUMS.txt.minisig` is a valid signature by the Glyph release key for that repository and version.
- **YouTube hints**: the server looks the song up in its own catalog (Deezer) and checks the reported video against it, not against the client's title or length. The artist must own the channel and both lengths must be known; an artist's "- Topic" upload replaces an earlier confirmed one. Hints confirmed under the old rules are ignored.

### Fixed

- **Shared resolver cache**: a client could name any provider id for a song (directly, through a shared playlist or a jam), and the server cached that match for every other user for 30 days, together with the client's cover, link and ids. Matches on client-named ids now reach only that client, cached answers keep only what the server's own search found, and older cache entries are no longer read.
- `/v1/cover` and the cover of `/p/<id>` links answer only covers from known image hosts.
- Requests with malformed provider ids, ISRCs, links or overlong texts are cleaned before any lookup.

## 0.7.0 - 2026-10-23

A plugin page on glyph.my, bans for server operators and a CLI that scripts and AI agents can drive.

### Added

- **Plugin page**: glyph.my/plugins lists every Glyph plugin, listed ones from glyphmusic/plugins and community ones with the GitHub topic glyph-plugin. Your browser loads the list straight from GitHub. Search it (forgiving, so typos still match), order it by most stars, recently updated or name, and see each plugin's GitHub stars. Each plugin has its own page (/plugins/<id>) with its colours and details, the background glows in the theme's colours, and **Install in Glyph** opens the app on that plugin, ready to install. The page says clearly that plugins are made by other people and installed at your own risk. Extensions show what they may do (their code, permissions and the web hosts they contact, settings and menus they add); plugins with code offer **Install in Glyph** only once they are listed.
- **Bans**: `glyph remote ban account|device|ip|hardware|install`, `glyph remote ban list` and `glyph remote unban`. Banning an account also bans the last address and the hardware of its devices; `--days` lifts a ban on its own, `--reason` keeps a note. Banned clients get 403 on every request. Devices keep only a salted hash of their last address (migration `0016_device_ban_marks.sql`), never the address.
- `glyph remote accounts revoke <handle> [device]` signs out one device or all of them; `accounts show` lists device ids and hardware hashes.
- `glyph remote limits --youtube-per-day <n>`: YouTube songs per account (guests: per address) and day.
- `glyph remote stats` (accounts, active users over 1/7/30 days, bans, cache) and `glyph status` (CLI, selected server and its state at a glance).
- `glyph remote list` and `glyph remote use <url>` switch between servers you are logged in to.
- `--json` on `glyph status`, `glyph remote ...`, `glyph config ... --server` and `glyph companion` for scripts and AI agents (errors too, with any command), `--dry-run` to see a change first, `--yes` to skip the question changes ask on a terminal.
- `glyph completions bash|zsh|fish|powershell|elvish` and `glyph man`.

### Changed

- `glyph help` and every `glyph <command> --help` end with a link to the CLI reference (https://docs.glyph.my/self-hosting/cli) and its Markdown version for AI agents. The reference now lists every command and flag and has a table of common tasks.
- **CLI, easier to follow**: `glyph companion off --server <url>` switches whether a server uses its companion; plain `glyph companion off` still switches the companion machine itself (`glyph remote companion` keeps working). `glyph config ... --server <url>` changes a running server's live settings and `--worker-config wrangler.json` a Cloudflare project, next to `server.toml`. The help groups the commands by where they act, lists the npm-only Cloudflare commands as `[npm package]` and hides `self-update`, `check-update` and `hash-password` (they still work).
- `glyph access` applies within a few seconds on a running server; no restart.
- Errors name the next step (`help: ...`) and exit with a code per kind of problem: 3 not logged in, 4 unreachable, 5 denied, 6 not found, 7 cancelled.

### Fixed

- **Far fewer D1 reads**: every sync push cleaned up old deleted items by reading all of an account's synced items, which used up the D1 free tier's daily reads. An index (migration `0015_sync_tombstone_index.sql`) makes the cleanup read only the items it removes.

## 0.6.1 - 2026-10-22

### Changed

- Device states (what plays where) live in each account's Durable Object instead of D1. Changes reach the other devices over their live connection at once, and reports no longer write to the database.
- Abuse limits count ordinary requests in the Cache API; D1 only stores bans, strikes and exact limits such as sign-ins and budgets. A Cloudflare deployment now writes a small fraction of what it did to D1.
- glyph.my/changelog lists Glyph 0.6.1.

### Fixed

- When D1 stopped accepting writes (for example the free plan's daily limit), the Worker answered every request, site and docs included, with 429. It now falls back to the Cache API limiter.

## 0.6.0 - 2026-10-21

### Changed

- The download page explains that a new Android APK installs over the old one; uninstalling first is no longer needed.
- glyph.my/changelog lists Glyph 0.6.0.

## 0.5.0 - 2026-10-21

Live jams, device control and the Android app.

### Added

- **Your devices**: apps signed in to the same account can see each other and send playback commands. Commands only reach devices of the same account and expire on their own; offline or busy devices turn them down.
- **Live jams**: `GET /v1/jam/{code}/live` (WebSocket) pushes changes to everyone in a jam the moment they happen, through one Durable Object per jam (`JAM_ROOM`, created on deploy). It stores nothing and stays within the Cloudflare free plan (at most 70,000 pushes and connections per day); past that, the apps fall back to asking every few seconds.
- Jam invite links at glyph.my/jam/... for the QR code in the app.

### Changed

- Jams use much less Worker CPU: waiting for changes reads a small revision key instead of the whole jam, and `POST /v1/jam/{code}/now` sends only the host's position for seeks and regular updates.
- The download page and changelog include the Android app.

## 0.4.1 - 2026-10-20

### Changed

- The sign-in page for AI assistants now matches the look of the desktop app.

## 0.4.0 - 2026-10-20

Documentation, a public changelog and easier configuration.

### Added

- **Documentation** at docs.glyph.my, with search (Ctrl+K or /) and versions of every page for AI tools. Changes to the docs go live within a minute.
- **This changelog**: every release of the app, server, companion and r-dlp in one place.
- **AI assistants** can reach your desktop app through your server, with a secure sign-in that you confirm in the app. Owners switch it with `glyph remote mcp on|off`.
- `glyph config` reads and changes `server.toml` with checks and defaults; API keys are never printed.
- `glyph cloudflare` changes a Cloudflare deployment's settings, secrets and custom domain.
- A questions section on the landing page.

### Changed

- "Transfer music" in the app can read SoundCloud links through the server, so the app needs no SoundCloud key.
- YouTube search is faster: the server asks YouTube directly first and accepts a hint from the app to skip the search entirely.

## 0.3.0 - 2026-10-19

Glyph on npm, and self-hosting made simple.

### Added

- Install the `glyph` command line tool with npm (`glyph-server` package).
- `glyph` can check and update every part of Glyph: CLI, server, companion and r-dlp.
- A guided setup for hosting your own Glyph on your own Cloudflare account.
- An option to allow only signed-in accounts on your server.
- Safe updates for the Docker companion, checked before they go live.

### Changed

- Faster audio streaming with less waiting on the first bytes.
- Building from source no longer needs other repositories.

## 0.2.6 - 2026-10-19

### Fixed

- Lyrics broke when the lyrics service rate-limited the server. They now come from your companion first.
- Missing lyrics or radio suggestions return an empty answer instead of an error, and failed lookups are retried later.

## 0.2.5 - 2026-10-19

### Added

- Support for the faster r-dlp YouTube resolver, with yt-dlp as fallback.
- The "Only YouTube" option from the app is respected: no other sources, no previews.

### Changed

- YouTube downloads take the same route as playback, including quality choice and recovery from expired links.

## 0.2.4 - 2026-10-19

### Fixed

- Songs stuck on a 30-second preview: the server tries full sources again before falling back to a cached preview.
- Wrong artists for some YouTube videos, and searches that gave up after a temporary error.

### Changed

- Less work per request on the Worker, so it stays fast and well within its limits.

## 0.2.3 - 2026-10-18

### Changed

- The companion now does the heavy searching, so the Worker only handles one small answer per song.

### Fixed

- Requests that hung and hit Cloudflare's CPU limits.
- Audius songs failing to play or download.

## 0.2.2 - 2026-10-18

Faster YouTube and smoother seeking.

### Added

- **Share links**: short Glyph links for songs, artists and playlists, with rich previews.
- Matching by ISRC on YouTube, so the right song is found more often (for example Blender's "LUVME" no longer falls back to a preview).

### Changed

- Several YouTube lookups run in parallel, and duplicate lookups are shared.
- High-quality YouTube audio starts streaming right away instead of after a full download.
- Seeking only fetches the part of the song you need.
- Downloaded songs can be checked for updates without delaying playback.
- Releases come with a single checksum file.

### Fixed

- A crash with some long, non-English cache keys.
- Wrong or mismatched videos picked for a song.
- One network error no longer turns off YouTube for ten minutes.

## 0.2.0 - 2026-10-18

### Added

- Builds for five platforms, with checksums.
- Protection against abuse and limits on heavy work per account.
- Several audio qualities per song, and a persistent audio cache.

### Changed

- The download page offers the Windows installer when there is one.

### Fixed

- The updater looked in the wrong repository.
- Missing images, favicon and download page on the website.

## 0.1.0

### Added

- The first server: search across providers, streaming audio, optional accounts and sync, running on Cloudflare with a companion on your own machine.
