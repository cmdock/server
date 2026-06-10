-- Add display_mode column to views for iOS ViewDisplayConfig compatibility.
-- Values: "list" (flat), "grouped" (grouped by project/tags), etc.
-- Defaults to "list" for all existing views.

ALTER TABLE views ADD COLUMN display_mode TEXT NOT NULL DEFAULT 'list';
