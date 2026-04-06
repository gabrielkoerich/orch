-- Add counter for how many times a NeedsReview task has been re-fired by the sync catch-up
ALTER TABLE tasks ADD COLUMN needs_review_refires INTEGER DEFAULT 0;
