-- Profiles: picture, banner and what other users may see (all off by default).
-- playlists: JSON array of shown playlist ids; recent: JSON array of songs.
CREATE TABLE IF NOT EXISTS profiles (
  user_id     TEXT PRIMARY KEY,
  public      INTEGER NOT NULL DEFAULT 0,
  show_recent INTEGER NOT NULL DEFAULT 0,
  playlists   TEXT NOT NULL DEFAULT '[]',
  avatar      TEXT,
  banner      TEXT,
  recent      TEXT,
  updated_at  INTEGER NOT NULL DEFAULT 0
);
