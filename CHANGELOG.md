# Changelog

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
