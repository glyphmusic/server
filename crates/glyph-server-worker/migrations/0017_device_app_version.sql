-- App version each device last reported (from User-Agent `Glyph/<ver>` or
-- `X-Glyph-App-Version` from the web player), shown in the admin dashboard.
ALTER TABLE devices ADD COLUMN app_version TEXT;
