-- Fix in-flight task_runs that have outcome='' instead of NULL.
-- Rows with completed_at IS NULL are still running; their outcome
-- should be NULL (not '') so analytics queries can reliably filter
-- on completed_at IS NOT NULL to exclude them.
UPDATE task_runs
SET outcome = NULL
WHERE completed_at IS NULL
  AND outcome = '';
