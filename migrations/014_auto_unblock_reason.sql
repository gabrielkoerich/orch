-- Add reason tracking for auto-unblock (split from 012 which was incorrectly modified).
ALTER TABLE tasks ADD COLUMN auto_unblock_last_reason TEXT DEFAULT '';
