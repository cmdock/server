-- Add origin tracking columns to views for default view management.
--
-- origin: 'builtin' (seeded by server) or 'user' (created via API)
-- user_modified: true if user has customised a builtin view
-- hidden: true if user explicitly deleted a builtin view (tombstone)
-- template_version: tracks which viewset version created/updated this view

ALTER TABLE views ADD COLUMN origin TEXT NOT NULL DEFAULT 'user' CHECK(origin IN ('builtin', 'user'));
ALTER TABLE views ADD COLUMN user_modified INTEGER NOT NULL DEFAULT 0;
ALTER TABLE views ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
ALTER TABLE views ADD COLUMN template_version INTEGER NOT NULL DEFAULT 0;
