"""Exercise the actual Worker SQL and complete migrations using SQLite."""
import concurrent.futures
from contextlib import closing
import json
from pathlib import Path
import re
import sqlite3
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
def sql(module, name):
    source = (ROOT / "src" / module).read_text()
    return re.search(r"pub const " + name + r': &str = r#"([\s\S]*?)"#;', source).group(1)

def setup(db):
    for migration in sorted((ROOT / "migrations").glob("*.sql")):
        db.executescript(migration.read_text())
    db.execute("UPDATE audio_cache_inventory SET ready=1")
    db.execute("INSERT INTO server_settings VALUES ('limits',?)", (json.dumps({"audio_cache_max_gb": 1, "d1_max_mb": 500}),))
    db.commit()

class Storage(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        setup(self.db)
    def tearDown(self): self.db.close()
    def reserve(self, key, size, generation="gen"):
        return self.db.execute(sql("audio_cache.rs", "RESERVE"), (key, size, 1000, generation, 10_000_000_000)).fetchone()
    def add(self, key, size, hits=0, created=1000):
        self.db.execute("INSERT INTO audio_cache(key,size,created,hits) VALUES (?,?,?,?)", (key,size,created,hits))
    def test_r2_headroom_and_authoritative_cap(self):
        self.assertIsNotNone(self.reserve("audio/one", 500_000_000))
        self.assertIsNone(self.reserve("audio/two", 1))
    def test_incomplete_inventory_and_duplicate_keys(self):
        self.assertIsNotNone(self.reserve("audio/one", 100))
        self.assertIsNone(self.reserve("audio/one", 100))
        self.db.execute("UPDATE audio_cache_inventory SET ready=0")
        self.assertIsNone(self.reserve("audio/two", 100))
    def test_protected_uploads_count_but_never_evict(self):
        self.add("users/private/avatar", 400_000_000)
        self.add("audio/hot", 100_000_000, 100)
        self.add("covers/cold", 100_000_000, 1)
        row = self.db.execute(sql("audio_cache.rs", "CLAIM"), (0,500_000_000)).fetchone()
        self.assertEqual(row[0], "covers/cold")
        # Claiming/deletion failure never releases charged storage.
        self.assertEqual(self.db.execute("SELECT SUM(size) FROM audio_cache").fetchone()[0], 600_000_000)
        self.db.execute("DELETE FROM audio_cache WHERE key NOT GLOB 'users/*'")
        self.assertIsNone(self.db.execute(sql("audio_cache.rs", "CLAIM"), (999999,1)).fetchone())
    def test_expiry_before_popularity_and_lru_tiebreak(self):
        self.add("audio/expired", 10, 10000, 1)
        self.add("audio/cold", 10, 0, 1000)
        self.assertEqual(self.db.execute(sql("audio_cache.rs", "CLAIM"), (100,0)).fetchone()[0], "audio/expired")
    def test_inventory_accounts_for_unknown_and_protected_files(self):
        self.add("audio/old", 100, 42)
        payload = json.dumps([{"key":"audio/old","size":150,"created":1000},{"key":"users/image","size":200,"created":1000}])
        self.db.execute(sql("audio_cache.rs", "OBSERVE"), (payload, 9000))
        self.assertEqual(self.db.execute("SELECT SUM(size) FROM audio_cache").fetchone()[0], 350)
        self.assertEqual(self.db.execute("SELECT hits FROM audio_cache WHERE key='audio/old'").fetchone()[0], 42)
    def test_d1_is_independent_and_reserves_sqlite_headroom(self):
        query = sql("storage.rs", "RESERVE")
        self.assertIsNotNone(self.db.execute(query, ("a",32_000_000,1,400_000_000)).fetchone())
        self.assertIsNone(self.db.execute(query, ("b",32_000_000,1,400_000_000)).fetchone())
        # A stale measurement cannot bypass committed growth after release.
        self.db.execute("UPDATE d1_storage_watermark SET bytes=440000000")
        self.db.execute("DELETE FROM d1_storage_reservations")
        self.assertIsNone(self.db.execute(query, ("c",32_000_000,1,0)).fetchone())
        self.assertIsNotNone(self.reserve("audio/unrelated-r2", 10))
    def test_variant_group_reserves_all_parts_or_none_and_deduplicates(self):
        query = sql("variants.rs", "RESERVE_GROUP")
        parts = [{"key":f"audio/video/low/hash/{i}","size":200_000_000} for i in range(3)]
        self.db.execute(query,(json.dumps(parts),1000,"group",10_000_000_000))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM audio_cache").fetchone()[0],0)
        parts = parts[:2]
        self.db.execute(query,(json.dumps(parts),1000,"group",10_000_000_000))
        self.assertEqual(self.db.execute("SELECT SUM(size) FROM audio_cache").fetchone()[0],400_000_000)
        self.db.execute(query,(json.dumps(parts),1000,"duplicate",10_000_000_000))
        self.assertEqual(self.db.execute("SELECT SUM(size) FROM audio_cache").fetchone()[0],400_000_000)
    def test_inventory_recovers_only_confirmed_upload_generations(self):
        self.reserve("audio/confirmed",100,"mine")
        payload=json.dumps([{"key":"audio/confirmed","size":100,"created":1000,"generation":"other"}])
        self.db.execute(sql("audio_cache.rs","OBSERVE"),(payload,9000))
        self.assertEqual(self.db.execute("SELECT state FROM audio_cache").fetchone()[0],"pending")
        payload=json.dumps([{"key":"audio/confirmed","size":100,"created":1000,"generation":"mine"}])
        self.db.execute(sql("audio_cache.rs","OBSERVE"),(payload,9000))
        self.assertEqual(self.db.execute("SELECT state FROM audio_cache").fetchone()[0],"ready")
    def test_quality_warming_lease_deduplicates_and_expires(self):
        query=sql("variants.rs","LEASE")
        self.assertIsNotNone(self.db.execute(query,("video",91000,1000)).fetchone())
        self.assertIsNone(self.db.execute(query,("video",92000,2000)).fetchone())
        self.assertIsNotNone(self.db.execute(query,("video",182000,92000)).fetchone())
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM audio_variant_jobs").fetchone()[0],1)
    def test_parallel_reservations_cannot_exceed_r2_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(Path(tmp)/"db.sqlite")
            with closing(sqlite3.connect(path)) as db: setup(db)
            query = sql("audio_cache.rs", "RESERVE")
            def hit(i):
                with closing(sqlite3.connect(path, timeout=30)) as db:
                    with db:
                        return db.execute(query,(f"audio/{i}",32_000_000,1000,str(i),10_000_000_000)).fetchone()
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                rows = list(pool.map(hit, range(64)))
            self.assertEqual(sum(row is not None for row in rows), 15)
            with closing(sqlite3.connect(path)) as db:
                self.assertEqual(db.execute("SELECT SUM(size) FROM audio_cache").fetchone()[0], 480_000_000)

if __name__ == "__main__": unittest.main()
