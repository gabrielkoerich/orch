-- The node that runs the task, NULL until a node starts it
ALTER TABLE tasks ADD COLUMN owner_node TEXT;
