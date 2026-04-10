ALTER TABLE control_messages
    ADD COLUMN input_tokens INTEGER;

ALTER TABLE control_messages
    ADD COLUMN output_tokens INTEGER;
