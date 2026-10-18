-- Approximate region/country only; no IP address, GPS or precise coordinates.
ALTER TABLE devices ADD COLUMN approximate_location TEXT;
