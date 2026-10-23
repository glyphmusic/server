-- Every sync push purges old tombstones (purge_tombstones). Without this index
-- both statements scan all of a user's sync_items; the partial index only holds
-- deleted rows, so the purge reads just the tombstones it removes.
CREATE INDEX IF NOT EXISTS sync_items_tombstones ON sync_items(user_id, updated_at) WHERE deleted = 1;
