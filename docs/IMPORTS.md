# Playlist imports and limits

The server operator sets the maximum number of songs per playlist. The default
is 5,000. Change it through the authenticated management API or CLI:

```sh
glyph-server remote limits --playlist-max-tracks 8000
```

The setting accepts 1–50,000 songs. For a native server it is also available as
`playlist_max_tracks` in `server.toml`. Existing management clients that omit
the setting preserve its current value. Existing request and sync byte limits
still apply independently of the song count.

`GET /v1/health` advertises `playlist_max_tracks`. The app uses that policy for
new additions, imports and playlist merges. Lowering the setting does not
delete songs from an existing local playlist. Sync validates playlist arrays
and their lengths on the server, regardless of client behavior.

## Companion processing

Public service imports use `POST /v1/import/spotify` with a URL and an optional
integer `offset`. The Worker forwards each page to authenticated
`POST /internal/import` on the Companion. Configure the provider credentials
on the Companion; Worker-only credentials are not copied to it.

Each response contains at most 100 songs and an optional `next_offset`.
Spotify, Deezer, YouTube, Last.fm and SoundCloud are read in bounded provider
pages. Continuation URLs/tokens for SoundCloud and YouTube remain in the
Companion's short-lived cache; clients cannot supply upstream cursor URLs.
Cache entries are scoped by the operator's limit and canonical collection URL.
Restart an import if its continuation expires.

The Worker never retries a failed import by traversing provider pages itself.
The app retries transient network/server errors and can fall back to its local
provider readers. Service authentication, private playlists and provider
availability still determine which collections can be read.

Lists with more than 100 songs are imported as ordered metadata locally.
Matching happens when a song is played, avoiding thousands of resolve requests
at import time. Smaller imports retain matching with bounded concurrency and
a per-song timeout. Public Spotify embed pages expose only the first 100
songs; reaching that boundary requires credentials on the Companion or a
complete playlist file instead of silently saving a partial playlist.

Older clients that do not send an offset receive an explicit update error for
multi-page lists. Deploy the updated server/Companion and app together to make
large imports available.

Spotify's current API uses `/playlists/{id}/items`, pages of at most 50 items,
and the renamed `item` field. Both row formats are accepted. Spotify's API
mode may require user authorization for playlist contents; app credentials
alone do not guarantee access. If access is denied, use the app's existing
Transfer music paste/file flow for a complete local import. See the
[Spotify API migration notes](https://developer.spotify.com/documentation/web-api/references/changes/february-2026).
