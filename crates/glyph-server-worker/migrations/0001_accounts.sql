-- Accounts & sync (see docs/ACCOUNTS.md in the app repo).
-- Apply with: npx wrangler d1 migrations apply glyph --remote

CREATE TABLE IF NOT EXISTS users (
  id                TEXT PRIMARY KEY,
  handle            TEXT NOT NULL UNIQUE,
  created_at        INTEGER NOT NULL,
  device_limit      INTEGER NOT NULL DEFAULT 5,
  rev               INTEGER NOT NULL DEFAULT 0,
  purged_rev        INTEGER NOT NULL DEFAULT 0,
  recovery_hash     TEXT NOT NULL,
  handle_changed_at INTEGER NOT NULL DEFAULT 0
);

-- Released handles stay blocked for a while.
CREATE TABLE IF NOT EXISTS handle_holds (
  handle TEXT PRIMARY KEY,
  until  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS passkeys (
  cred_id    TEXT PRIMARY KEY,
  user_id    TEXT NOT NULL,
  alg        INTEGER NOT NULL,
  public_key TEXT NOT NULL,
  sign_count INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS passkeys_user ON passkeys(user_id);

CREATE TABLE IF NOT EXISTS devices (
  id           TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL,
  name         TEXT NOT NULL,
  platform     TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL,
  access_hash  TEXT NOT NULL UNIQUE,
  access_exp   INTEGER NOT NULL,
  refresh_hash TEXT NOT NULL UNIQUE,
  refresh_exp  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS devices_user ON devices(user_id);

CREATE TABLE IF NOT EXISTS logins (
  id              TEXT PRIMARY KEY,
  poll_hash       TEXT NOT NULL,
  user_code       TEXT NOT NULL UNIQUE,
  device_name     TEXT NOT NULL,
  platform        TEXT NOT NULL,
  challenge       TEXT,
  pending_handle  TEXT,
  pending_user_id TEXT,
  user_id         TEXT,
  page_hash       TEXT,
  expires         INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_items (
  user_id    TEXT NOT NULL,
  kind       TEXT NOT NULL,
  key        TEXT NOT NULL,
  value      TEXT,
  updated_at INTEGER NOT NULL,
  device_id  TEXT NOT NULL,
  deleted    INTEGER NOT NULL DEFAULT 0,
  rev        INTEGER NOT NULL,
  PRIMARY KEY (user_id, kind, key)
);
CREATE INDEX IF NOT EXISTS sync_items_rev ON sync_items(user_id, rev);
