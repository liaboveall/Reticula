ALTER TABLE branches
    ADD COLUMN title TEXT NOT NULL DEFAULT '',
    ADD COLUMN est_bytes BIGINT NOT NULL DEFAULT 0 CHECK (est_bytes >= 0),
    ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN merged_at TIMESTAMPTZ,
    ADD COLUMN merge_seq BIGINT CHECK (merge_seq IS NULL OR merge_seq >= 0),
    ADD COLUMN agent_replica INTEGER,
    ADD CONSTRAINT branches_kind_check CHECK (kind IN ('main', 'ai_draft', 'user')),
    ADD CONSTRAINT branches_status_check CHECK (status IN ('active', 'merged', 'abandoned'));

CREATE INDEX branches_workbook_created_idx ON branches (workbook_id, created_at DESC);

CREATE TABLE agent_runs (
    id UUID PRIMARY KEY,
    workbook_id UUID NOT NULL REFERENCES workbooks (id) ON DELETE CASCADE,
    branch_id UUID NOT NULL,
    user_id UUID NOT NULL REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'waiting_review', 'merged', 'rejected', 'failed', 'cancelled')),
    model TEXT NOT NULL,
    provider TEXT NOT NULL,
    step_count INTEGER NOT NULL DEFAULT 0 CHECK (step_count >= 0),
    resume_count INTEGER NOT NULL DEFAULT 0 CHECK (resume_count >= 0),
    prompt_tokens BIGINT NOT NULL DEFAULT 0 CHECK (prompt_tokens >= 0),
    cached_prompt_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cached_prompt_tokens >= 0),
    completion_tokens BIGINT NOT NULL DEFAULT 0 CHECK (completion_tokens >= 0),
    summary TEXT NOT NULL DEFAULT '',
    error_message TEXT,
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    FOREIGN KEY (workbook_id, branch_id) REFERENCES branches (workbook_id, id) ON DELETE CASCADE
);

CREATE INDEX agent_runs_workbook_created_idx ON agent_runs (workbook_id, created_at DESC);
CREATE INDEX agent_runs_active_status_idx ON agent_runs (status) WHERE status IN ('queued', 'running');

CREATE TABLE agent_steps (
    run_id UUID NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,
    idx INTEGER NOT NULL CHECK (idx >= 0),
    kind TEXT NOT NULL CHECK (kind IN ('user_prompt', 'assistant_thought', 'tool_call', 'tool_result', 'txn_commit', 'error', 'finish')),
    payload JSONB NOT NULL,
    prompt_tokens INTEGER NOT NULL DEFAULT 0 CHECK (prompt_tokens >= 0),
    cached_prompt_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cached_prompt_tokens >= 0),
    completion_tokens INTEGER NOT NULL DEFAULT 0 CHECK (completion_tokens >= 0),
    latency_ms INTEGER NOT NULL DEFAULT 0 CHECK (latency_ms >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, idx)
);

CREATE TABLE quotas (
    user_id UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    day DATE NOT NULL,
    llm_tokens BIGINT NOT NULL DEFAULT 0 CHECK (llm_tokens >= 0),
    agent_runs INTEGER NOT NULL DEFAULT 0 CHECK (agent_runs >= 0),
    txn_count INTEGER NOT NULL DEFAULT 0 CHECK (txn_count >= 0),
    PRIMARY KEY (user_id, day)
);
