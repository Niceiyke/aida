CREATE TABLE projects (
    project_id TEXT PRIMARY KEY NOT NULL,
    current_revision INTEGER NOT NULL CHECK (current_revision >= 1),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE revisions (
    project_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    timeline_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (project_id, revision),
    FOREIGN KEY (project_id) REFERENCES projects(project_id) ON DELETE CASCADE
);

CREATE TABLE idempotency_records (
    project_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_sha256 TEXT NOT NULL,
    result_revision INTEGER NOT NULL CHECK (result_revision >= 1),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (project_id, idempotency_key),
    FOREIGN KEY (project_id) REFERENCES projects(project_id) ON DELETE CASCADE,
    FOREIGN KEY (project_id, result_revision) REFERENCES revisions(project_id, revision)
);

CREATE TRIGGER revisions_are_immutable_on_update
BEFORE UPDATE ON revisions
BEGIN
    SELECT RAISE(ABORT, 'project revisions are immutable');
END;

CREATE TRIGGER revisions_are_immutable_on_delete
BEFORE DELETE ON revisions
BEGIN
    SELECT RAISE(ABORT, 'project revisions are immutable');
END;
