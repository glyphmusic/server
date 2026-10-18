CREATE TABLE d1_storage_reservations (
  token TEXT PRIMARY KEY,
  bytes INTEGER NOT NULL CHECK(bytes>0),
  created INTEGER NOT NULL
);
CREATE INDEX logins_expires ON logins(expires);
CREATE INDEX handle_holds_until ON handle_holds(until);

CREATE TABLE d1_storage_watermark (id INTEGER PRIMARY KEY CHECK(id=1), bytes INTEGER NOT NULL, revision INTEGER NOT NULL DEFAULT 0);
INSERT INTO d1_storage_watermark VALUES (1,0,0);
