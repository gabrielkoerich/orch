-- Add dedicated ci_recovery_count column for CI-based auto-recovery tracking.
-- Previously, handle_review_changes incorrectly used auto_unblock_count for this purpose,
-- conflating two independent mechanisms (recoverable-failure auto-unblock and CI auto-recovery).
ALTER TABLE tasks ADD COLUMN ci_recovery_count INTEGER DEFAULT 0;
