-- Short playlist links (/p/<id>). Each belongs to the account that made it
-- and is deleted with it.
CREATE TABLE IF NOT EXISTS shares (
  id         TEXT PRIMARY KEY,
  owner_id   TEXT NOT NULL,
  code       TEXT NOT NULL,
  name       TEXT NOT NULL DEFAULT '',
  songs      INTEGER NOT NULL DEFAULT 0,
  kind       TEXT NOT NULL DEFAULT 'playlist',
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS shares_owner ON shares (owner_id);
