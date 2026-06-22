-- Add persona_name and persona_emoji columns to clients table.
-- Both are nullable; NULL means "no persona" (existing behaviour preserved).

ALTER TABLE clients ADD COLUMN persona_name  TEXT;
ALTER TABLE clients ADD COLUMN persona_emoji TEXT;
