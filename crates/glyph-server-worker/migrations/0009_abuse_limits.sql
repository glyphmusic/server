-- Atomic quotas and temporary bans, shared across Worker instances/locations.
CREATE TABLE IF NOT EXISTS abuse_limits (
    key TEXT PRIMARY KEY,
    count INTEGER NOT NULL,
    expires INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS abuse_limits_expiry ON abuse_limits(expires);
