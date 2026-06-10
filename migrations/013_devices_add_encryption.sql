-- Add per-device encryption secret to devices table.
-- Each device gets its own encryption_secret derived via HKDF from the user's
-- master secret. Encrypted with the server's master key before storage.
-- Nullable for migration: existing devices get secrets backfilled on first sync.

ALTER TABLE devices ADD COLUMN encryption_secret_enc TEXT;
