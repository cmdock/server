-- Backfill legacy migrated-device secrets from the canonical sync identity.
--
-- Migration 012 auto-created one active "Migrated device" row per existing
-- replica using the replica client_id. Migration 013 added
-- devices.encryption_secret_enc as nullable with the intent that existing rows
-- would be repaired later. Those legacy rows still authenticate with the
-- canonical client_id + canonical encryption secret, so the correct repair is
-- to copy replicas.encryption_secret_enc onto the matching device row.
--
-- This only repairs the legacy migrated-device shape where:
-- - the device currently has no stored secret
-- - the device client_id matches the canonical replica id for the same user
--
-- It does not mask genuinely broken per-device rows with unrelated client_ids.

UPDATE devices
SET encryption_secret_enc = (
    SELECT replicas.encryption_secret_enc
    FROM replicas
    WHERE replicas.user_id = devices.user_id
      AND replicas.id = devices.client_id
)
WHERE devices.encryption_secret_enc IS NULL
  AND EXISTS (
      SELECT 1
      FROM replicas
      WHERE replicas.user_id = devices.user_id
        AND replicas.id = devices.client_id
  );
