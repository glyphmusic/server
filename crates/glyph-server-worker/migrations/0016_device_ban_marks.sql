-- Operator bans (`glyph remote ban account <handle>`): each device keeps a
-- salted hash of its last IP and the server-side hash of the app's hardware
-- id, so banning an account can also ban its addresses and devices. Never the
-- address itself. The ban list lives in server_settings (key 'bans').
ALTER TABLE devices ADD COLUMN ip_hash TEXT;
ALTER TABLE devices ADD COLUMN hw_hash TEXT;
