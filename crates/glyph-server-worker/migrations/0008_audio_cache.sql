-- Companion audio kept in R2 (`audio/youtube/<id>`): size and age, so the
-- Worker can hold the total under `audio_cache_max_gb` (oldest go first).
CREATE TABLE IF NOT EXISTS audio_cache (
  key TEXT PRIMARY KEY,
  size INTEGER NOT NULL,
  created INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS audio_cache_created ON audio_cache (created);
