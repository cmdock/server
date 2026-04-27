-- Bootstrap state for operator-managed onboarding flows.
-- Keeps idempotency and delivery-tracking metadata on the device row so the
-- first issue-36 slice does not need a separate bootstrap-attempt subsystem.

ALTER TABLE devices ADD COLUMN bootstrap_request_id TEXT;
ALTER TABLE devices ADD COLUMN bootstrap_status TEXT;
ALTER TABLE devices ADD COLUMN bootstrap_requested_username TEXT;
ALTER TABLE devices ADD COLUMN bootstrap_create_user_if_missing INTEGER;
ALTER TABLE devices ADD COLUMN bootstrap_expires_at TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_bootstrap_request_id
ON devices(bootstrap_request_id)
WHERE bootstrap_request_id IS NOT NULL;
