ALTER TABLE render_jobs ADD COLUMN phase TEXT NOT NULL DEFAULT 'queued'
    CHECK (phase IN ('queued', 'preparing', 'encoding', 'validating', 'publishing', 'complete'));
ALTER TABLE render_jobs ADD COLUMN progress_percent INTEGER NOT NULL DEFAULT 0
    CHECK (progress_percent BETWEEN 0 AND 100);

UPDATE render_jobs
SET phase = CASE status
        WHEN 'succeeded' THEN 'complete'
        WHEN 'running' THEN 'preparing'
        ELSE 'queued'
    END,
    progress_percent = CASE status WHEN 'succeeded' THEN 100 ELSE 0 END;
