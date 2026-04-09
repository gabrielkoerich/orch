-- Enforce that estimate is a Fibonacci value (0 means not provided).
-- SQLite does not support ALTER TABLE ... ADD CONSTRAINT CHECK, so triggers
-- are used instead to reject invalid values at the database level.
CREATE TRIGGER tasks_estimate_insert_check
BEFORE INSERT ON tasks
FOR EACH ROW
WHEN NEW.estimate NOT IN (0, 1, 2, 3, 5, 8, 13, 21)
BEGIN
    SELECT RAISE(ABORT, 'estimate must be a Fibonacci value: 0,1,2,3,5,8,13,21');
END;

CREATE TRIGGER tasks_estimate_update_check
BEFORE UPDATE ON tasks
FOR EACH ROW
WHEN NEW.estimate NOT IN (0, 1, 2, 3, 5, 8, 13, 21)
BEGIN
    SELECT RAISE(ABORT, 'estimate must be a Fibonacci value: 0,1,2,3,5,8,13,21');
END;
