CREATE TABLE audio_variants (
 track_id TEXT NOT NULL,
 quality TEXT NOT NULL CHECK(quality IN ('low','standard','high')),
 manifest TEXT NOT NULL,
 created INTEGER NOT NULL,
 PRIMARY KEY(track_id,quality)
);
CREATE INDEX audio_variants_created ON audio_variants(created);
