-- Short leases deduplicate proactive quality preparation across Worker isolates.
CREATE TABLE IF NOT EXISTS audio_variant_jobs (
 track_id TEXT PRIMARY KEY,
 expires INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS audio_variant_jobs_expiry ON audio_variant_jobs(expires);
