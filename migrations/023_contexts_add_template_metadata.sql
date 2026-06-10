-- Add template-version metadata columns to contexts so default seeds can be
-- reconciled in the same way as default views.
--
-- origin: 'builtin' (seeded by server) or 'user' (created via API)
-- user_modified: true if user has customised a builtin context
-- hidden: true if user explicitly deleted a builtin context (tombstone)
-- template_version: tracks which contextset version created/updated this context

ALTER TABLE contexts ADD COLUMN origin TEXT NOT NULL DEFAULT 'user' CHECK(origin IN ('builtin', 'user'));
ALTER TABLE contexts ADD COLUMN user_modified INTEGER NOT NULL DEFAULT 0;
ALTER TABLE contexts ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
ALTER TABLE contexts ADD COLUMN template_version INTEGER NOT NULL DEFAULT 0;
