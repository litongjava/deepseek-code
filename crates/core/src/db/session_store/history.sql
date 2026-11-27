CREATE TABLE execution_turns (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES manifest(id) ON DELETE CASCADE,
    source_turn_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    elapsed_ms INTEGER CHECK(elapsed_ms IS NULL OR elapsed_ms >= 0),
    status TEXT NOT NULL CHECK(status IN ('running','completed','failed','cancelled')),
    UNIQUE(conversation_id, source_turn_id),
    CHECK(status <> 'running' OR (ended_at IS NULL AND elapsed_ms IS NULL))
);

CREATE TABLE message_turns (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL REFERENCES execution_turns(id) ON DELETE CASCADE
);

CREATE INDEX idx_message_turns_turn ON message_turns(turn_id);

CREATE TABLE session_permission_authority (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL REFERENCES manifest(id) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK (source IN ('user', 'approved_plan')),
    origin_id TEXT NOT NULL,
    content_json TEXT NOT NULL,
    UNIQUE(conversation_id, origin_id)
);

CREATE INDEX idx_permission_authority_session ON session_permission_authority(conversation_id, id);

CREATE TABLE message_trajectory (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    requests_json TEXT NOT NULL
);

CREATE TABLE request_blobs (
    hash TEXT PRIMARY KEY,
    body TEXT NOT NULL
);

CREATE TABLE mcp_tool_outputs (
    conversation_id TEXT NOT NULL REFERENCES manifest(id) ON DELETE CASCADE,
    tool_use_id TEXT NOT NULL,
    content_json TEXT NOT NULL,
    model_content_json TEXT,
    metadata_json TEXT NOT NULL,
    display_text TEXT NOT NULL,
    is_error INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, tool_use_id)
);

CREATE TABLE model_requests (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL REFERENCES manifest(id) ON DELETE CASCADE,
    request_number INTEGER,
    provider_id TEXT,
    model TEXT,
    response_model TEXT,
    api_format TEXT,
    status TEXT,
    started_at_ms INTEGER,
    first_token_at_ms INTEGER,
    ended_at_ms INTEGER,
    total_duration_ms INTEGER,
    ttft_ms INTEGER,
    generation_ms INTEGER,
    throughput_tokens_per_second REAL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    reasoning_tokens INTEGER,
    content_tokens INTEGER,
    total_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    max_output_tokens INTEGER,
    effort TEXT,
    finish_reason TEXT,
    response_id TEXT,
    timing_source TEXT,
    error TEXT,
    response_blocks_json TEXT NOT NULL DEFAULT '[]',
    usage_json TEXT
);

CREATE INDEX idx_model_requests_conversation ON model_requests(conversation_id, request_number);

CREATE INDEX idx_model_requests_provider_model ON model_requests(provider_id, model, started_at_ms);

CREATE INDEX idx_model_requests_message ON model_requests(message_id);

CREATE TABLE tool_calls (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL,
    name TEXT NOT NULL,
    input TEXT,
    model_checked INTEGER NOT NULL DEFAULT 0,
    model_output TEXT,
    model_preview TEXT,
    model_recovery TEXT,
    model_search TEXT,
    output TEXT,
    is_error INTEGER DEFAULT 0,
    sort_order INTEGER NOT NULL,
    request_id TEXT REFERENCES model_requests(id) ON DELETE SET NULL,
    provider_call_id TEXT,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE INDEX idx_tool_calls_message_id ON tool_calls(message_id);
CREATE INDEX idx_tool_calls_request_id ON tool_calls(request_id);

CREATE TABLE tool_call_origins (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    parent_tool_call_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    origin TEXT NOT NULL DEFAULT 'toolscript'
);

CREATE INDEX idx_tool_call_origins_parent
    ON tool_call_origins(parent_tool_call_id);

CREATE TABLE tool_call_timings (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    duration_ms INTEGER NOT NULL CHECK(duration_ms >= 0)
);

CREATE TABLE tool_call_notes (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    note TEXT NOT NULL
);

CREATE TABLE tool_queued_inputs (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    inputs_json TEXT NOT NULL
);

CREATE TABLE attachments (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL,
    file_name TEXT,
    file_type TEXT,
    mime_type TEXT,
    file_size INTEGER,
    source TEXT,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE INDEX idx_attachments_message_id ON attachments(message_id);

CREATE TABLE permission_approvals (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    action TEXT NOT NULL,
    risk_level TEXT NOT NULL DEFAULT 'medium',
    status TEXT NOT NULL DEFAULT 'pending',
    user_decision TEXT,
    decision_reason TEXT,
    created_at TEXT NOT NULL,
    decided_at TEXT,
    FOREIGN KEY (conversation_id) REFERENCES manifest(id) ON DELETE CASCADE,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE INDEX idx_permission_approvals_conversation ON permission_approvals(conversation_id);

CREATE INDEX idx_permission_approvals_status ON permission_approvals(status);

CREATE TABLE code_diffs (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    file_path TEXT NOT NULL,
    original_content TEXT,
    modified_content TEXT,
    diff_text TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    applied_at TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES manifest(id) ON DELETE CASCADE,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE INDEX idx_code_diffs_conversation ON code_diffs(conversation_id);

CREATE INDEX idx_code_diffs_status ON code_diffs(status);

CREATE INDEX messages_order ON messages(conversation_id,sort_order);
CREATE TABLE delivery_receipts (
    delivery_id TEXT PRIMARY KEY NOT NULL, digest TEXT NOT NULL, effect_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('message','tool','peer'))
);
CREATE TABLE session_agents (conversation_id TEXT PRIMARY KEY REFERENCES manifest(id), definition TEXT NOT NULL);
CREATE TABLE native_boundaries(message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE, value TEXT NOT NULL);
CREATE TABLE child_invocations(attempt_key TEXT PRIMARY KEY, parent_message TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE, child_incarnation TEXT NOT NULL, invocation TEXT NOT NULL);
CREATE TRIGGER remove_occurrence_output AFTER DELETE ON tool_calls BEGIN
 DELETE FROM mcp_tool_outputs WHERE tool_use_id=OLD.id;
END;
CREATE TABLE delivery_generations(
    delivery_id TEXT PRIMARY KEY REFERENCES delivery_receipts(delivery_id), turn_id TEXT UNIQUE,
    FOREIGN KEY(delivery_id) REFERENCES messages(id) ON DELETE CASCADE
);
CREATE TABLE compaction_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1), local_count INTEGER NOT NULL DEFAULT 0);
INSERT INTO compaction_state(singleton) VALUES(1);
