# Changelog

## Unreleased

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
