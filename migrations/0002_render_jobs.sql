CREATE TABLE render_jobs (
    job_id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    preset_id TEXT NOT NULL CHECK (length(preset_id) BETWEEN 1 AND 64),
    idempotency_key TEXT NOT NULL,
    request_sha256 TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    worker_id TEXT,
    lease_expires_at INTEGER,
    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0, 1)),
    result_json TEXT,
    error TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (project_id, idempotency_key),
    FOREIGN KEY (project_id, revision) REFERENCES revisions(project_id, revision)
);

CREATE INDEX render_jobs_queue_idx ON render_jobs(status, created_at);
CREATE INDEX render_jobs_lease_idx ON render_jobs(status, lease_expires_at);
