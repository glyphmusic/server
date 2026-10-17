-- Handle + password sign-in. The client sends an Argon2id-derived key; the
-- server keeps its salt and a SHA-256 of the key, never the password.
ALTER TABLE users ADD COLUMN pw_salt TEXT;
ALTER TABLE users ADD COLUMN pw_hash TEXT;

-- A login row can add a passkey to an existing account instead of signing in.
ALTER TABLE logins ADD COLUMN link INTEGER NOT NULL DEFAULT 0;
