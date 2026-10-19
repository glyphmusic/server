# Glyph CLI

```sh
npm install --global glyph-server
glyph help
glyph version
```

Node.js 22+ is required. Installation downloads the exact matching native release,
checks its SHA-256 and version, and supports Windows x64, macOS x64/arm64 and glibc
Linux x64/arm64. Rust is not required. `glyph-server` remains a compatibility alias.

## Your own server

```sh
glyph setup cloudflare
glyph deploy --config glyph-cloudflare/wrangler.json
```

The guided setup uses **your** Cloudflare account, creates a D1 database, applies
the included migrations and deploys the prebuilt Worker. R2 media/image storage
is optional and off unless selected; enable R2 in your own account first. It
never copies Glyph's production domains, database IDs or secrets. The password
is hashed locally; Wrangler stores the hash as a secret. Sign in to your own
Cloudflare account when prompted. No Rust, Git checkout or separate desktop
repository is required.

The setup uses the documented [D1 creation/migration commands](https://developers.cloudflare.com/d1/wrangler-commands/)
and optional [R2 bucket commands](https://developers.cloudflare.com/workers/wrangler/commands/r2/).
For YouTube, deploy your own [companion](https://github.com/glyphmusic/companion)
and set `YTDLP_SERVICE_URL` / `YTDLP_SERVICE_TOKEN` with Wrangler's secret commands
against your generated config. Without a companion the key-free sources still work.

To host a native server instead: `glyph setup`, then `glyph serve` or
`glyph service install`. Install yt-dlp and FFmpeg for YouTube/encoded audio;
native r-dlp can be selected through `GLYPH_RDLP_PATH`. Cloudflare accounts/sync
use D1; the native server currently has no account storage backend.

## Manage and inspect

```sh
glyph remote login https://your-server.example
glyph version all
glyph version server
glyph version companion
glyph version r-dlp
glyph check companion
glyph check r-dlp
glyph check all
glyph remote limits --require-account true
```

The remembered server is used automatically; pass `--server <url>` to select
another server, or `--local` to inspect this machine's companion and tools.
Companion checks include the running Glyph version, enabled/ready state and the
installed r-dlp, yt-dlp and FFmpeg versions. Installed tool versions are distinct
from an image tag or an already-loaded persistent worker.

## Updates

```sh
glyph update                       # npm CLI update
glyph update cli --check
glyph update server --check
glyph update server                # authenticated native-server update
glyph update companion --check
glyph update companion
glyph update r-dlp --check
glyph update r-dlp
glyph update server --worker-config glyph-cloudflare/wrangler.json
glyph check companion --compose-dir /path/to/companion-project
glyph update companion --compose-dir /path/to/companion-project
```

The Worker update applies your configured migrations and deploys the bundled
Worker using your Cloudflare login. A remote native update replaces its verified
binary and reports when a restart is required; verify the running version after
restarting. Docker images should be updated/recreated through their deployment
instead of modifying binaries inside an immutable container. The CLI does not
claim a running process was updated merely because a file was replaced.

r-dlp updates use its own release/checksum manifest and reload the extractor pool
after installation. An old companion without management endpoints must first be
updated through its deployment. npm-managed native processes are upgraded with
`glyph update cli` and restarted through the updated CLI, preserving the old
versioned executable while it is running.

Source, licenses and releases: https://github.com/glyphmusic/server
