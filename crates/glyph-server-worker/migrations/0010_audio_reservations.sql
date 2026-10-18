-- Account all R2 objects; only audio/ and covers/ are automatically evictable.
ALTER TABLE audio_cache ADD COLUMN state TEXT NOT NULL DEFAULT 'ready';
ALTER TABLE audio_cache ADD COLUMN generation TEXT NOT NULL DEFAULT 'legacy';
ALTER TABLE audio_cache ADD COLUMN hits INTEGER NOT NULL DEFAULT 0;
ALTER TABLE audio_cache ADD COLUMN last_access INTEGER NOT NULL DEFAULT 0;
ALTER TABLE audio_cache ADD COLUMN seen INTEGER NOT NULL DEFAULT 0;
CREATE TABLE audio_cache_inventory (
 id INTEGER PRIMARY KEY CHECK(id=1), cursor TEXT, ready INTEGER NOT NULL DEFAULT 0,
 scan_id INTEGER NOT NULL DEFAULT 0, finished INTEGER NOT NULL DEFAULT 0
);
INSERT INTO audio_cache_inventory(id) VALUES (1);
