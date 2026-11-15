ALTER TABLE render_jobs ADD COLUMN output_asset_id TEXT;
ALTER TABLE render_jobs ADD COLUMN retry_of_job_id TEXT REFERENCES render_jobs(job_id);
ALTER TABLE render_jobs ADD COLUMN retry_root_id TEXT NOT NULL DEFAULT '';
ALTER TABLE render_jobs ADD COLUMN retry_number INTEGER NOT NULL DEFAULT 0
    CHECK (retry_number BETWEEN 0 AND 2);

UPDATE render_jobs
SET retry_root_id = job_id
WHERE retry_root_id = '';

UPDATE render_jobs
SET output_asset_id = lower(
    substr(hex(randomblob(16)), 1, 8) || '-' ||
    substr(hex(randomblob(16)), 9, 4) || '-' ||
    substr(hex(randomblob(16)), 13, 4) || '-' ||
    substr(hex(randomblob(16)), 17, 4) || '-' ||
    substr(hex(randomblob(16)), 21, 12)
)
WHERE status = 'succeeded' AND result_json IS NOT NULL;

CREATE UNIQUE INDEX render_jobs_output_asset_idx
    ON render_jobs(output_asset_id)
    WHERE output_asset_id IS NOT NULL;
CREATE UNIQUE INDEX render_jobs_retry_sequence_idx
    ON render_jobs(retry_root_id, retry_number);
CREATE INDEX render_jobs_retry_parent_idx ON render_jobs(retry_of_job_id);

CREATE TABLE render_job_retries (
    source_job_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_sha256 TEXT NOT NULL,
    result_job_id TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (source_job_id, idempotency_key),
    FOREIGN KEY (source_job_id) REFERENCES render_jobs(job_id),
    FOREIGN KEY (result_job_id) REFERENCES render_jobs(job_id)
);
