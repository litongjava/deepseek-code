pub const SCHEMA_SQL: &str = concat!(r#"
CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    title TEXT,
    model TEXT,
    provider TEXT,
    workspace_path TEXT,
    project_id TEXT,
    workspace_mode TEXT NOT NULL DEFAULT 'chat' CHECK (workspace_mode IN ('chat', 'work', 'study', 'code')),
    research_mode INTEGER DEFAULT 0,
    pinned INTEGER DEFAULT 0,
    archived INTEGER DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    message_count INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS conversation_private_workspace_bindings (
    child_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    -- Retain an invalid binding after parent deletion: never silently redirect a child.
    parent_id TEXT NOT NULL,
    CHECK(child_id <> parent_id)
);

CREATE TABLE IF NOT EXISTS conversation_worktrees (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL,
    source_path TEXT NOT NULL,
    repo_path TEXT NOT NULL,
    path TEXT NOT NULL,
    branch TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conversation_worktree_backends (
    conversation_id TEXT PRIMARY KEY REFERENCES conversation_worktrees(conversation_id) ON DELETE CASCADE,
    backend TEXT NOT NULL CHECK (backend = 'hook'),
    metadata TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    thinking TEXT,
    created_at TEXT NOT NULL,
    is_compact_boundary INTEGER DEFAULT 0,
    sort_order INTEGER NOT NULL,
    model_content TEXT,
    model_recovery TEXT,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tool_queued_inputs (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    inputs_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS execution_turns (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    source_turn_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    elapsed_ms INTEGER CHECK(elapsed_ms IS NULL OR elapsed_ms >= 0),
    status TEXT NOT NULL CHECK(status IN ('running','completed','failed','cancelled')),
    UNIQUE(conversation_id, source_turn_id),
    CHECK(status <> 'running' OR (ended_at IS NULL AND elapsed_ms IS NULL))
);
-- Within a conversation this index also orders turn admissions by SQLite rowid.
CREATE INDEX IF NOT EXISTS idx_execution_turns_conversation ON execution_turns(conversation_id);
CREATE TABLE IF NOT EXISTS message_turns (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL REFERENCES execution_turns(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_message_turns_turn ON message_turns(turn_id);

CREATE TABLE IF NOT EXISTS session_permission_authority (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK (source IN ('user', 'approved_plan')),
    origin_id TEXT NOT NULL,
    content_json TEXT NOT NULL,
    UNIQUE(conversation_id, origin_id)
);
CREATE INDEX IF NOT EXISTS idx_permission_authority_session ON session_permission_authority(conversation_id, id);

CREATE TABLE IF NOT EXISTS message_trajectory (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    requests_json TEXT NOT NULL
);

-- Content-addressed bodies for request headers repeated across snapshots.
CREATE TABLE IF NOT EXISTS request_blobs (
    hash TEXT PRIMARY KEY,
    body TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS message_user_content (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    content_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_tool_outputs (
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    tool_use_id TEXT NOT NULL,
    content_json TEXT NOT NULL,
    model_content_json TEXT,
    metadata_json TEXT NOT NULL,
    display_text TEXT NOT NULL,
    is_error INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, tool_use_id)
);

CREATE TABLE IF NOT EXISTS model_requests (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
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
CREATE INDEX IF NOT EXISTS idx_model_requests_conversation ON model_requests(conversation_id, request_number);
CREATE INDEX IF NOT EXISTS idx_model_requests_provider_model ON model_requests(provider_id, model, started_at_ms);
CREATE INDEX IF NOT EXISTS idx_model_requests_message ON model_requests(message_id);

CREATE TABLE IF NOT EXISTS tool_calls (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL,
    name TEXT NOT NULL,
    input TEXT,
    output TEXT,
    is_error INTEGER DEFAULT 0,
    sort_order INTEGER NOT NULL,
    model_checked INTEGER NOT NULL DEFAULT 0,
    model_output TEXT,
    model_preview TEXT,
    model_recovery TEXT,
    model_search TEXT,
    request_id TEXT REFERENCES model_requests(id) ON DELETE SET NULL,
    provider_call_id TEXT,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_tool_calls_request_id ON tool_calls(request_id);

-- Nested calls made by an embedded ToolScript run. The parent is the script's
-- own occurrence; `sequence` is the script's internal admission order. The
-- cascade is load-bearing: deleting a message cascades to tool_calls, and
-- connections run PRAGMA foreign_keys=ON.
CREATE TABLE IF NOT EXISTS tool_call_origins (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    parent_tool_call_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    origin TEXT NOT NULL DEFAULT 'toolscript'
);
CREATE INDEX IF NOT EXISTS idx_tool_call_origins_parent
    ON tool_call_origins(parent_tool_call_id);

-- Messages typed while a turn runs, read after its next tool results.
CREATE TABLE IF NOT EXISTS session_input_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_session_input_queue ON session_input_queue(session_id, id);

-- What the user added when approving a call, replayed after its result as
-- the user's own words.
CREATE TABLE IF NOT EXISTS tool_call_timings (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    duration_ms INTEGER NOT NULL CHECK(duration_ms >= 0)
);

CREATE TABLE IF NOT EXISTS tool_call_notes (
    tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE,
    note TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL,
    file_name TEXT,
    file_type TEXT,
    mime_type TEXT,
    file_size INTEGER,
    source TEXT,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    instructions TEXT,
    workspace_path TEXT,
    is_archived INTEGER DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS project_folders (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    trusted INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL,
    PRIMARY KEY (project_id, path)
);

CREATE TABLE IF NOT EXISTS permission_approvals (
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
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS code_diffs (
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
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_messages_conversation_id ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_conversations_updated_at ON conversations(updated_at);
CREATE INDEX IF NOT EXISTS idx_conversations_model ON conversations(model);
CREATE INDEX IF NOT EXISTS idx_tool_calls_message_id ON tool_calls(message_id);
CREATE INDEX IF NOT EXISTS idx_attachments_message_id ON attachments(message_id);
CREATE INDEX IF NOT EXISTS idx_permission_approvals_conversation ON permission_approvals(conversation_id);
CREATE INDEX IF NOT EXISTS idx_permission_approvals_status ON permission_approvals(status);
CREATE INDEX IF NOT EXISTS idx_code_diffs_conversation ON code_diffs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_code_diffs_status ON code_diffs(status);

-- Durable activity, shared by CLI, child transcripts and Desktop. Keep the
-- timestamp in the same statement/transaction as the content. Replaying the
-- same snapshot is not activity; reads and worker heartbeats have no trigger.
CREATE TRIGGER IF NOT EXISTS activity_messages_insert AFTER INSERT ON messages
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=NEW.conversation_id
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_messages_update AFTER UPDATE OF content,thinking,role,is_compact_boundary ON messages
WHEN NEW.content IS NOT OLD.content OR NEW.thinking IS NOT OLD.thinking OR NEW.role IS NOT OLD.role OR NEW.is_compact_boundary IS NOT OLD.is_compact_boundary
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=NEW.conversation_id
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_tool_calls_insert AFTER INSERT ON tool_calls
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=(SELECT conversation_id FROM messages
    WHERE id=NEW.message_id)
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_tool_calls_update AFTER UPDATE OF name,input,output,is_error ON tool_calls
WHEN NEW.name IS NOT OLD.name OR NEW.input IS NOT OLD.input OR NEW.output IS NOT OLD.output OR NEW.is_error IS NOT OLD.is_error
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=(SELECT conversation_id FROM messages
    WHERE id=NEW.message_id)
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_message_user_content_insert AFTER INSERT ON message_user_content
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=(SELECT conversation_id FROM messages
    WHERE id=NEW.message_id)
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_message_user_content_update AFTER UPDATE OF content_json ON message_user_content
WHEN NEW.content_json IS NOT OLD.content_json
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=(SELECT conversation_id FROM messages
    WHERE id=NEW.message_id)
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_messages_delete AFTER DELETE ON messages
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=OLD.conversation_id
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_tool_calls_delete AFTER DELETE ON tool_calls
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=(SELECT conversation_id FROM messages WHERE id=OLD.message_id)
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_message_user_content_delete AFTER DELETE ON message_user_content
BEGIN
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=(SELECT conversation_id FROM messages WHERE id=OLD.message_id)
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
CREATE TRIGGER IF NOT EXISTS activity_turn_finished AFTER UPDATE OF status,ended_at ON execution_turns
WHEN NEW.status <> 'running' AND (NEW.status IS NOT OLD.status OR NEW.ended_at IS NOT OLD.ended_at)
BEGIN
    UPDATE conversations
    SET updated_at=NEW.ended_at
    WHERE id=NEW.conversation_id
      AND julianday(updated_at) < julianday(NEW.ended_at);
    UPDATE conversations
    SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')
    WHERE id=NEW.conversation_id
      AND (julianday(updated_at) IS NULL OR julianday(updated_at) < julianday(strftime('%Y-%m-%dT%H:%M:%f000000+00:00','now')));
END;
"#, include_str!("session_store/shared_domains.sql"));

#[cfg(test)]
#[path = "activity_tests.rs"]
mod activity_tests;
