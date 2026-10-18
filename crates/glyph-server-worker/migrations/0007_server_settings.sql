-- Settings changed at runtime through the admin API (`glyph-server remote limits`,
-- `remote companion on|off`). One JSON value per key.
CREATE TABLE IF NOT EXISTS server_settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
