-- Device limits are operator settings (glyph-server remote accounts …):
-- users.device_limit 0 = server default, kept in settings.default_device_limit.
CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- Limits were picked by clients until now; reset them to the default.
UPDATE users SET device_limit = 0;
