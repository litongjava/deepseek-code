ALTER TABLE session_input_queue ADD COLUMN prompt_origin TEXT NOT NULL DEFAULT 'legacy_queue';
-- Pending entries predate explicit provenance. Only the CLI could reserve
-- this namespace for task notifications. All other queue producers were human.
UPDATE session_input_queue SET prompt_origin=CASE
    WHEN request_id GLOB 'internal-cli:notice:*' THEN 'task_notification'
    ELSE 'typed' END;
