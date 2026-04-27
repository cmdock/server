-- Add context_id to views for binding project-scoped named views to a ContextDefinition.
ALTER TABLE views ADD COLUMN context_id TEXT;
