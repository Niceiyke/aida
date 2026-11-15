use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    RenderOptions, RenderResult,
    render::RenderPreset,
    timeline::{
        AudioClip, Clip, SourceTimestamp, TextClip, Timeline, TimelineAsset, Track, TrackKind,
        ValidatedTimeline, VideoClip, load_and_validate, source_timestamp_at_ratio, validate,
        validate_json_document,
    },
    workspace::Workspace,
};

const DATABASE_VERSION: i32 = 3;
const MAX_EDIT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_BATCH_OPERATIONS: usize = 100;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RENDER_JOB_ATTEMPTS: i64 = 3;
const MAX_EXPLICIT_RENDER_RETRIES: i64 = 2;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditRequest {
    pub operations: Vec<EditOperation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum EditOperation {
    #[serde(rename = "clip.add")]
    ClipAdd {
        #[serde(rename = "trackId")]
        track_id: String,
        clip: Clip,
        #[serde(default)]
        index: Option<usize>,
    },
    #[serde(rename = "clip.remove")]
    ClipRemove {
        #[serde(rename = "clipId")]
        clip_id: String,
    },
    #[serde(rename = "clip.move")]
    ClipMove {
        #[serde(rename = "clipId")]
        clip_id: String,
        #[serde(rename = "startFrame")]
        start_frame: u64,
    },
    #[serde(rename = "clip.reorder")]
    ClipReorder {
        #[serde(rename = "clipId")]
        clip_id: String,
        #[serde(rename = "destinationIndex")]
        destination_index: usize,
    },
    #[serde(rename = "clip.trim")]
    ClipTrim {
        #[serde(rename = "clipId")]
        clip_id: String,
        #[serde(rename = "startFrame")]
        start_frame: u64,
        #[serde(rename = "durationFrames")]
        duration_frames: u64,
        #[serde(default, rename = "sourceIn")]
        source_in: Option<SourceTimestamp>,
        #[serde(default, rename = "sourceOut")]
        source_out: Option<SourceTimestamp>,
    },
    #[serde(rename = "clip.split")]
    ClipSplit {
        #[serde(rename = "clipId")]
        clip_id: String,
        #[serde(rename = "splitFrame")]
        split_frame: u64,
    },
    #[serde(rename = "text.update")]
    TextUpdate {
        #[serde(rename = "clipId")]
        clip_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(rename = "fontSize", skip_serializing_if = "Option::is_none")]
        font_size: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        x: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        y: Option<u32>,
    },
    #[serde(rename = "audio.gain.set")]
    AudioGainSet {
        #[serde(rename = "clipId")]
        clip_id: String,
        gain: f64,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRevision {
    pub project_id: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    pub project_id: String,
    pub current_revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRevisionEntry {
    pub revision: u64,
    pub created_at: String,
    pub is_current: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRevisionHistory {
    pub project_id: String,
    pub current_revision: u64,
    pub revisions: Vec<ProjectRevisionEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResult {
    pub project_id: String,
    pub revision: u64,
    pub replayed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRestoreResult {
    pub project_id: String,
    pub revision: u64,
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RenderJobStatus {
    fn from_database_value(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => bail!("database contains an invalid render job status"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderJob {
    pub job_id: String,
    pub project_id: String,
    pub revision: u64,
    pub preset_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_of_job_id: Option<String>,
    pub retry_number: u32,
    pub status: RenderJobStatus,
    pub attempt: u32,
    pub cancel_requested: bool,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_asset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RenderJobResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderJobResult {
    pub timeline_sha256: String,
    pub output_sha256: String,
    pub output_duration_seconds: f64,
    pub output_width: u32,
    pub output_height: u32,
    pub output_frame_count: u64,
    pub output_frame_rate: String,
    pub video_codec: String,
    pub audio_codec: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RenderAssetLocation {
    pub asset_id: String,
    pub job_id: String,
    pub attempt: u32,
    pub output_sha256: String,
    pub preset_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderJobSubmission {
    pub job: RenderJob,
    pub replayed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ExpiredRenderAttempt {
    pub job_id: String,
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub struct ProjectStore {
    database: PathBuf,
}

impl ProjectStore {
    pub fn open(database: impl Into<PathBuf>) -> Result<Self> {
        let store = Self {
            database: database.into(),
        };
        let mut connection = store.connect()?;
        store.ensure_schema(&mut connection)?;
        Ok(store)
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.database
    }

    pub fn create_from_timeline(
        &self,
        workspace_path: &Path,
        timeline_path: &Path,
    ) -> Result<ProjectRevision> {
        let workspace = Workspace::open(workspace_path)?;
        let validated = load_and_validate(timeline_path, &workspace)?;
        self.create_initial_revision(validated)
    }

    pub fn create_from_timeline_json(
        &self,
        workspace_path: &Path,
        document: Value,
    ) -> Result<ProjectRevision> {
        let workspace = Workspace::open(workspace_path)?;
        let validated = validate_json_document(document, &workspace)?;
        self.create_initial_revision(validated)
    }

    fn create_initial_revision(&self, validated: ValidatedTimeline) -> Result<ProjectRevision> {
        if validated.timeline.revision != 0 {
            bail!("project creation requires an unpersisted timeline with revision 0");
        }

        let project_id = validated.timeline.project_id.clone();
        let revision = 1_u64;
        let mut timeline = validated.timeline;
        timeline.revision = revision;
        let timeline_json = serde_json::to_string(&timeline)?;
        let revision_sql = to_sql_revision(revision)?;

        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let already_exists = transaction
            .query_row(
                "SELECT 1 FROM projects WHERE project_id = ?1",
                [&project_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if already_exists {
            bail!("PROJECT_EXISTS: project {project_id} already exists");
        }
        transaction
            .execute(
                "INSERT INTO projects (project_id, current_revision) VALUES (?1, ?2)",
                rusqlite::params![project_id, revision_sql],
            )
            .with_context(|| format!("could not create project {project_id}"))?;
        transaction.execute(
            "INSERT INTO revisions (project_id, revision, timeline_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![project_id, revision_sql, timeline_json],
        )?;
        transaction.commit()?;

        Ok(ProjectRevision {
            project_id,
            revision,
        })
    }

    pub fn apply_edit_batch(
        &self,
        workspace_path: &Path,
        project_id: &str,
        expected_revision: u64,
        idempotency_key: &str,
        operations: &[EditOperation],
    ) -> Result<EditResult> {
        validate_project_id(project_id)?;
        validate_idempotency_key(idempotency_key)?;
        if operations.is_empty() || operations.len() > MAX_BATCH_OPERATIONS {
            bail!("edit batch must contain between 1 and 100 operations");
        }

        let workspace = Workspace::open(workspace_path)?;
        let request_sha256 = edit_request_digest(expected_revision, operations)?;
        to_sql_revision(expected_revision)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((stored_digest, result_revision)) = transaction
            .query_row(
                "SELECT request_sha256, result_revision FROM idempotency_records \
                 WHERE project_id = ?1 AND idempotency_key = ?2",
                rusqlite::params![project_id, idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            if stored_digest != request_sha256 {
                bail!("IDEMPOTENCY_CONFLICT: key was already used for a different edit request");
            }
            let revision = from_sql_revision(result_revision)?;
            transaction.commit()?;
            return Ok(EditResult {
                project_id: project_id.to_owned(),
                revision,
                replayed: true,
            });
        }

        let current_sql = transaction
            .query_row(
                "SELECT current_revision FROM projects WHERE project_id = ?1",
                [project_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("PROJECT_NOT_FOUND: project {project_id} does not exist"))?;
        let current_revision = from_sql_revision(current_sql)?;
        if current_revision != expected_revision {
            bail!(
                "REVISION_CONFLICT: expected revision {expected_revision}; current revision is {current_revision}"
            );
        }

        let current_json: String = transaction.query_row(
            "SELECT timeline_json FROM revisions WHERE project_id = ?1 AND revision = ?2",
            rusqlite::params![project_id, current_sql],
            |row| row.get(0),
        )?;
        let mut timeline: Timeline =
            serde_json::from_str(&current_json).context("stored project timeline is invalid")?;
        apply_operations(&mut timeline, operations, &workspace)?;

        let next_revision = current_revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("project revision overflow"))?;
        timeline.revision = next_revision;
        let validated = validate(timeline, &workspace)?;
        let timeline_json = serde_json::to_string(&validated.timeline)?;
        let next_sql = to_sql_revision(next_revision)?;

        transaction.execute(
            "INSERT INTO revisions (project_id, revision, timeline_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![project_id, next_sql, timeline_json],
        )?;
        let updated = transaction.execute(
            "UPDATE projects SET current_revision = ?1 WHERE project_id = ?2 AND current_revision = ?3",
            rusqlite::params![next_sql, project_id, current_sql],
        )?;
        if updated != 1 {
            bail!("REVISION_CONFLICT: project changed while applying the edit batch");
        }
        transaction.execute(
            "INSERT INTO idempotency_records \
             (project_id, idempotency_key, request_sha256, result_revision) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![project_id, idempotency_key, request_sha256, next_sql],
        )?;
        transaction.commit()?;

        Ok(EditResult {
            project_id: project_id.to_owned(),
            revision: next_revision,
            replayed: false,
        })
    }

    pub fn load_revision_json(&self, project_id: &str, revision: u64) -> Result<String> {
        validate_project_id(project_id)?;
        let revision_sql = to_sql_revision(revision)?;
        self.connect()?
            .query_row(
                "SELECT timeline_json FROM revisions WHERE project_id = ?1 AND revision = ?2",
                rusqlite::params![project_id, revision_sql],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                anyhow!("REVISION_NOT_FOUND: revision {revision} does not exist for project {project_id}")
            })
    }

    pub fn get_project_revision_history(&self, project_id: &str) -> Result<ProjectRevisionHistory> {
        validate_project_id(project_id)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let current_sql = transaction
            .query_row(
                "SELECT current_revision FROM projects WHERE project_id = ?1",
                [project_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("PROJECT_NOT_FOUND: project {project_id} does not exist"))?;
        let current_revision = from_sql_revision(current_sql)?;
        let mut revisions = Vec::new();
        {
            let mut statement = transaction.prepare(
                "SELECT revision, created_at FROM revisions WHERE project_id = ?1 ORDER BY revision DESC",
            )?;
            let rows = statement.query_map([project_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (revision, created_at) = row?;
                let revision = from_sql_revision(revision)?;
                revisions.push(ProjectRevisionEntry {
                    revision,
                    created_at,
                    is_current: revision == current_revision,
                });
            }
        }
        transaction.commit()?;
        Ok(ProjectRevisionHistory {
            project_id: project_id.to_owned(),
            current_revision,
            revisions,
        })
    }

    pub fn restore_revision(
        &self,
        workspace_path: &Path,
        project_id: &str,
        target_revision: u64,
        expected_revision: u64,
        idempotency_key: &str,
    ) -> Result<ProjectRestoreResult> {
        validate_project_id(project_id)?;
        validate_idempotency_key(idempotency_key)?;
        let target_sql = to_sql_revision(target_revision)?;
        to_sql_revision(expected_revision)?;
        let request_sha256 = restore_request_digest(target_revision, expected_revision)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((stored_digest, result_revision)) = transaction
            .query_row(
                "SELECT request_sha256, result_revision FROM idempotency_records \
                 WHERE project_id = ?1 AND idempotency_key = ?2",
                rusqlite::params![project_id, idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            if stored_digest != request_sha256 {
                bail!(
                    "IDEMPOTENCY_CONFLICT: key was already used for a different project mutation"
                );
            }
            let revision = from_sql_revision(result_revision)?;
            transaction.commit()?;
            return Ok(ProjectRestoreResult {
                project_id: project_id.to_owned(),
                revision,
                replayed: true,
            });
        }

        let current_sql = transaction
            .query_row(
                "SELECT current_revision FROM projects WHERE project_id = ?1",
                [project_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("PROJECT_NOT_FOUND: project {project_id} does not exist"))?;
        let current_revision = from_sql_revision(current_sql)?;
        if current_revision != expected_revision {
            bail!(
                "REVISION_CONFLICT: expected revision {expected_revision}; current revision is {current_revision}"
            );
        }

        let target_json = transaction
            .query_row(
                "SELECT timeline_json FROM revisions WHERE project_id = ?1 AND revision = ?2",
                rusqlite::params![project_id, target_sql],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                anyhow!("REVISION_NOT_FOUND: revision {target_revision} does not exist for project {project_id}")
            })?;
        let next_revision = current_revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("project revision overflow"))?;
        let mut timeline: Timeline =
            serde_json::from_str(&target_json).context("stored project timeline is invalid")?;
        timeline.revision = next_revision;
        let workspace = Workspace::open(workspace_path)?;
        let validated = validate(timeline, &workspace)?;
        let timeline_json = serde_json::to_string(&validated.timeline)?;
        let next_sql = to_sql_revision(next_revision)?;

        transaction.execute(
            "INSERT INTO revisions (project_id, revision, timeline_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![project_id, next_sql, timeline_json],
        )?;
        let updated = transaction.execute(
            "UPDATE projects SET current_revision = ?1 WHERE project_id = ?2 AND current_revision = ?3",
            rusqlite::params![next_sql, project_id, current_sql],
        )?;
        if updated != 1 {
            bail!("REVISION_CONFLICT: project changed while restoring the revision");
        }
        transaction.execute(
            "INSERT INTO idempotency_records \
             (project_id, idempotency_key, request_sha256, result_revision) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![project_id, idempotency_key, request_sha256, next_sql],
        )?;
        transaction.commit()?;
        Ok(ProjectRestoreResult {
            project_id: project_id.to_owned(),
            revision: next_revision,
            replayed: false,
        })
    }

    pub fn get_project_summary(&self, project_id: &str) -> Result<ProjectSummary> {
        validate_project_id(project_id)?;
        let summary = self
            .connect()?
            .query_row(
                "SELECT projects.current_revision, projects.created_at, revisions.created_at \
                 FROM projects JOIN revisions \
                   ON revisions.project_id = projects.project_id \
                  AND revisions.revision = projects.current_revision \
                 WHERE projects.project_id = ?1",
                [project_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("PROJECT_NOT_FOUND: project {project_id} does not exist"))?;
        Ok(ProjectSummary {
            project_id: project_id.to_owned(),
            current_revision: from_sql_revision(summary.0)?,
            created_at: summary.1,
            updated_at: summary.2,
        })
    }

    pub fn list_project_summaries(&self) -> Result<Vec<ProjectSummary>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT projects.project_id, projects.current_revision, projects.created_at, revisions.created_at \
             FROM projects JOIN revisions \
               ON revisions.project_id = projects.project_id \
              AND revisions.revision = projects.current_revision \
             ORDER BY revisions.created_at DESC, projects.project_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut projects = Vec::new();
        for row in rows {
            let (project_id, current_revision, created_at, updated_at) = row?;
            projects.push(ProjectSummary {
                project_id,
                current_revision: from_sql_revision(current_revision)?,
                created_at,
                updated_at,
            });
        }
        Ok(projects)
    }

    pub fn load_current_revision_json(&self, project_id: &str) -> Result<(u64, String)> {
        validate_project_id(project_id)?;
        let row = self
            .connect()?
            .query_row(
                "SELECT projects.current_revision, revisions.timeline_json \
                 FROM projects JOIN revisions \
                   ON revisions.project_id = projects.project_id \
                  AND revisions.revision = projects.current_revision \
                 WHERE projects.project_id = ?1",
                [project_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("PROJECT_NOT_FOUND: project {project_id} does not exist"))?;
        Ok((from_sql_revision(row.0)?, row.1))
    }

    pub fn enqueue_render_job(
        &self,
        project_id: &str,
        revision: u64,
        preset_id: &str,
        idempotency_key: &str,
    ) -> Result<RenderJobSubmission> {
        validate_project_id(project_id)?;
        RenderPreset::parse(preset_id)?;
        validate_idempotency_key(idempotency_key)?;
        let revision_sql = to_sql_revision(revision)?;
        let request_sha256 = render_request_digest(project_id, revision, preset_id)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((job_id, stored_digest)) = transaction
            .query_row(
                "SELECT job_id, request_sha256 FROM render_jobs \
                 WHERE project_id = ?1 AND idempotency_key = ?2",
                rusqlite::params![project_id, idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if stored_digest != request_sha256 {
                bail!("IDEMPOTENCY_CONFLICT: key was already used for a different render request");
            }
            let job = load_render_job(&transaction, &job_id)?;
            transaction.commit()?;
            return Ok(RenderJobSubmission {
                job,
                replayed: true,
            });
        }

        let project_exists = transaction
            .query_row(
                "SELECT 1 FROM projects WHERE project_id = ?1",
                [project_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !project_exists {
            bail!("PROJECT_NOT_FOUND: project {project_id} does not exist");
        }
        let revision_exists = transaction
            .query_row(
                "SELECT 1 FROM revisions WHERE project_id = ?1 AND revision = ?2",
                rusqlite::params![project_id, revision_sql],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !revision_exists {
            bail!(
                "REVISION_NOT_FOUND: revision {revision} does not exist for project {project_id}"
            );
        }

        let job_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO render_jobs (job_id, project_id, revision, preset_id, idempotency_key, request_sha256, status, retry_root_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?1)",
            rusqlite::params![job_id, project_id, revision_sql, preset_id, idempotency_key, request_sha256],
        )?;
        let job = load_render_job(&transaction, &job_id)?;
        transaction.commit()?;
        Ok(RenderJobSubmission {
            job,
            replayed: false,
        })
    }

    pub fn retry_render_job(
        &self,
        job_id: &str,
        idempotency_key: &str,
    ) -> Result<RenderJobSubmission> {
        validate_project_id(job_id).context("jobId must be a UUID")?;
        validate_idempotency_key(idempotency_key)?;
        let request_sha256 = retry_request_digest(job_id)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((result_job_id, stored_digest)) = transaction
            .query_row(
                "SELECT result_job_id, request_sha256 FROM render_job_retries \
                 WHERE source_job_id = ?1 AND idempotency_key = ?2",
                rusqlite::params![job_id, idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if stored_digest != request_sha256 {
                bail!("IDEMPOTENCY_CONFLICT: retry key was used for a different request");
            }
            let job = load_render_job(&transaction, &result_job_id)?;
            transaction.commit()?;
            return Ok(RenderJobSubmission {
                job,
                replayed: true,
            });
        }

        let source = transaction
            .query_row(
                "SELECT project_id, revision, preset_id, status, retry_root_id, retry_number \
                 FROM render_jobs WHERE job_id = ?1",
                [job_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("JOB_NOT_FOUND: render job {job_id} does not exist"))?;
        if source.3 != "failed" {
            bail!("JOB_NOT_RETRYABLE: only failed render jobs may be retried");
        }
        let latest = transaction.query_row(
            "SELECT job_id, retry_number FROM render_jobs \
             WHERE retry_root_id = ?1 ORDER BY retry_number DESC LIMIT 1",
            [&source.4],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        if latest.0 != job_id || latest.1 != source.5 {
            bail!("JOB_NOT_RETRYABLE: a newer retry already exists for this render");
        }
        let retry_number = source
            .5
            .checked_add(1)
            .ok_or_else(|| anyhow!("render retry count overflow"))?;
        if retry_number > MAX_EXPLICIT_RENDER_RETRIES {
            bail!("JOB_RETRY_LIMIT: render job reached its explicit retry limit");
        }

        let revision = from_sql_revision(source.1)?;
        let job_id_new = Uuid::new_v4().to_string();
        let internal_key = format!("retry-{job_id_new}");
        let render_digest = render_request_digest(&source.0, revision, &source.2)?;
        transaction.execute(
            "INSERT INTO render_jobs (job_id, project_id, revision, preset_id, idempotency_key, request_sha256, status, retry_of_job_id, retry_root_id, retry_number) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?8, ?9)",
            rusqlite::params![
                job_id_new,
                source.0,
                source.1,
                source.2,
                internal_key,
                render_digest,
                job_id,
                source.4,
                retry_number,
            ],
        )?;
        transaction.execute(
            "INSERT INTO render_job_retries (source_job_id, idempotency_key, request_sha256, result_job_id) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![job_id, idempotency_key, request_sha256, job_id_new],
        )?;
        let job = load_render_job(&transaction, &job_id_new)?;
        transaction.commit()?;
        Ok(RenderJobSubmission {
            job,
            replayed: false,
        })
    }

    pub fn get_render_job(&self, job_id: &str) -> Result<RenderJob> {
        validate_project_id(job_id).context("jobId must be a UUID")?;
        load_render_job(&self.connect()?, job_id)
    }

    pub(crate) fn get_render_asset(&self, asset_id: &str) -> Result<RenderAssetLocation> {
        validate_project_id(asset_id).context("assetId must be a UUID")?;
        let row = self
            .connect()?
            .query_row(
                "SELECT job_id, attempt, preset_id, result_json FROM render_jobs \
                 WHERE output_asset_id = ?1 AND status = 'succeeded'",
                [asset_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("ASSET_NOT_FOUND: render output {asset_id} does not exist"))?;
        let result: RenderResult =
            serde_json::from_str(&row.3).context("stored render job result is invalid")?;
        Ok(RenderAssetLocation {
            asset_id: asset_id.to_owned(),
            job_id: row.0,
            attempt: u32::try_from(row.1).context("database contains an invalid job attempt")?,
            output_sha256: result.output_sha256,
            preset_id: row.2,
        })
    }

    pub fn cancel_render_job(&self, job_id: &str) -> Result<RenderJob> {
        validate_project_id(job_id).context("jobId must be a UUID")?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE render_jobs SET \
                status = CASE WHEN status = 'queued' THEN 'cancelled' ELSE status END, \
                cancel_requested = CASE WHEN status IN ('queued', 'running') THEN 1 ELSE cancel_requested END, \
                worker_id = CASE WHEN status = 'queued' THEN NULL ELSE worker_id END, \
                lease_expires_at = CASE WHEN status = 'queued' THEN NULL ELSE lease_expires_at END, \
                finished_at = CASE WHEN status = 'queued' THEN CURRENT_TIMESTAMP ELSE finished_at END, \
                updated_at = CURRENT_TIMESTAMP \
             WHERE job_id = ?1 AND status IN ('queued', 'running')",
            [job_id],
        )?;
        let job = load_render_job(&transaction, job_id)?;
        transaction.commit()?;
        Ok(job)
    }

    pub(crate) fn claim_next_render_job(
        &self,
        worker_id: &str,
        lease_seconds: u64,
    ) -> Result<Option<RenderJob>> {
        let now = unix_time_seconds()?;
        let lease_seconds = i64::try_from(lease_seconds).context("render job lease is too long")?;
        let lease_expires_at = now
            .checked_add(lease_seconds)
            .ok_or_else(|| anyhow!("render job lease timestamp overflow"))?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job_id = transaction
            .query_row(
                "SELECT job_id FROM render_jobs WHERE status = 'queued' \
                 ORDER BY created_at, job_id LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(job_id) = job_id else {
            transaction.commit()?;
            return Ok(None);
        };
        let updated = transaction.execute(
            "UPDATE render_jobs SET status = 'running', attempt = attempt + 1, \
                worker_id = ?1, lease_expires_at = ?2, cancel_requested = 0, \
                started_at = COALESCE(started_at, CURRENT_TIMESTAMP), updated_at = CURRENT_TIMESTAMP \
             WHERE job_id = ?3 AND status = 'queued'",
            rusqlite::params![worker_id, lease_expires_at, job_id],
        )?;
        if updated != 1 {
            bail!("render job queue changed while claiming work");
        }
        let job = load_render_job(&transaction, &job_id)?;
        transaction.commit()?;
        Ok(Some(job))
    }

    pub(crate) fn heartbeat_render_job(
        &self,
        job_id: &str,
        worker_id: &str,
        lease_seconds: u64,
    ) -> Result<Option<bool>> {
        let now = unix_time_seconds()?;
        let lease_seconds = i64::try_from(lease_seconds).context("render job lease is too long")?;
        let lease_expires_at = now
            .checked_add(lease_seconds)
            .ok_or_else(|| anyhow!("render job lease timestamp overflow"))?;
        let connection = self.connect()?;
        let updated = connection.execute(
            "UPDATE render_jobs SET lease_expires_at = ?1, updated_at = CURRENT_TIMESTAMP \
             WHERE job_id = ?2 AND worker_id = ?3 AND status = 'running'",
            rusqlite::params![lease_expires_at, job_id, worker_id],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        let cancel_requested = connection.query_row(
            "SELECT cancel_requested FROM render_jobs WHERE job_id = ?1",
            [job_id],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(Some(cancel_requested))
    }

    pub(crate) fn recover_expired_render_jobs(&self) -> Result<Vec<ExpiredRenderAttempt>> {
        let now = unix_time_seconds()?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired = {
            let mut statement = transaction.prepare(
                "SELECT job_id, attempt, cancel_requested FROM render_jobs \
                 WHERE status = 'running' AND lease_expires_at <= ?1",
            )?;
            statement
                .query_map([now], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut attempts = Vec::with_capacity(expired.len());
        for (job_id, attempt, cancel_requested) in expired {
            let status = if cancel_requested {
                "cancelled"
            } else if attempt >= MAX_RENDER_JOB_ATTEMPTS {
                "failed"
            } else {
                "queued"
            };
            transaction.execute(
                "UPDATE render_jobs SET status = ?1, worker_id = NULL, lease_expires_at = NULL, \
                    cancel_requested = CASE WHEN ?1 = 'cancelled' THEN 1 ELSE 0 END, \
                    error = CASE WHEN ?1 = 'queued' THEN 'worker lease expired; job requeued' \
                                 WHEN ?1 = 'failed' THEN 'worker lease expired after the maximum attempts' \
                                 ELSE 'render cancelled' END, \
                    finished_at = CASE WHEN ?1 IN ('cancelled', 'failed') THEN CURRENT_TIMESTAMP ELSE NULL END, \
                    updated_at = CURRENT_TIMESTAMP \
                 WHERE job_id = ?2 AND status = 'running' AND lease_expires_at <= ?3",
                rusqlite::params![status, job_id, now],
            )?;
            attempts.push(ExpiredRenderAttempt {
                job_id,
                attempt: u32::try_from(attempt)
                    .context("database contains an invalid job attempt")?,
            });
        }
        transaction.commit()?;
        Ok(attempts)
    }

    pub(crate) fn complete_render_job(
        &self,
        job_id: &str,
        worker_id: &str,
        output_asset_id: &str,
        result: &RenderResult,
    ) -> Result<bool> {
        validate_project_id(output_asset_id).context("outputAssetId must be a UUID")?;
        let result_json = serde_json::to_string(result)?;
        let updated = self.connect()?.execute(
            "UPDATE render_jobs SET status = 'succeeded', result_json = ?1, output_asset_id = ?2, error = NULL, \
                worker_id = NULL, lease_expires_at = NULL, finished_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP \
             WHERE job_id = ?3 AND worker_id = ?4 AND status = 'running' AND cancel_requested = 0",
            rusqlite::params![result_json, output_asset_id, job_id, worker_id],
        )?;
        Ok(updated == 1)
    }

    pub(crate) fn fail_render_job(
        &self,
        job_id: &str,
        worker_id: &str,
        error: &str,
    ) -> Result<bool> {
        let error = truncate_error(error, 16 * 1024);
        let updated = self.connect()?.execute(
            "UPDATE render_jobs SET status = 'failed', error = ?1, worker_id = NULL, \
                lease_expires_at = NULL, finished_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP \
             WHERE job_id = ?2 AND worker_id = ?3 AND status = 'running' AND cancel_requested = 0",
            rusqlite::params![error, job_id, worker_id],
        )?;
        Ok(updated == 1)
    }

    pub(crate) fn finish_cancelled_render_job(
        &self,
        job_id: &str,
        worker_id: &str,
    ) -> Result<bool> {
        let updated = self.connect()?.execute(
            "UPDATE render_jobs SET status = 'cancelled', error = 'render cancelled', worker_id = NULL, \
                lease_expires_at = NULL, cancel_requested = 1, finished_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP \
             WHERE job_id = ?1 AND worker_id = ?2 AND status = 'running'",
            rusqlite::params![job_id, worker_id],
        )?;
        Ok(updated == 1)
    }

    pub(crate) fn requeue_render_job(&self, job_id: &str, worker_id: &str) -> Result<bool> {
        let updated = self.connect()?.execute(
            "UPDATE render_jobs SET status = 'queued', error = 'worker shutting down; job retained', \
                worker_id = NULL, lease_expires_at = NULL, cancel_requested = 0, updated_at = CURRENT_TIMESTAMP \
             WHERE job_id = ?1 AND worker_id = ?2 AND status = 'running' AND cancel_requested = 0",
            rusqlite::params![job_id, worker_id],
        )?;
        Ok(updated == 1)
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.database).with_context(|| {
            format!(
                "could not open project database {}",
                self.database.display()
            )
        })?;
        connection.busy_timeout(DATABASE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }

    fn ensure_schema(&self, connection: &mut Connection) -> Result<()> {
        let mut version: i32 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !(0..=DATABASE_VERSION).contains(&version) {
            bail!("unsupported project database version {version}");
        }
        while version < DATABASE_VERSION {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let next_version = match version {
                0 => {
                    transaction.execute_batch(include_str!("../migrations/0001_initial.sql"))?;
                    1
                }
                1 => {
                    transaction
                        .execute_batch(include_str!("../migrations/0002_render_jobs.sql"))?;
                    2
                }
                2 => {
                    transaction.execute_batch(include_str!(
                        "../migrations/0003_render_assets_and_retries.sql"
                    ))?;
                    3
                }
                _ => bail!("no migration available from project database version {version}"),
            };
            transaction.pragma_update(None, "user_version", next_version)?;
            transaction.commit()?;
            version = next_version;
        }
        Ok(())
    }
}

struct StoredRenderJob {
    job_id: String,
    project_id: String,
    revision: i64,
    preset_id: String,
    retry_of_job_id: Option<String>,
    retry_number: i64,
    status: String,
    attempt: i64,
    cancel_requested: bool,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    output_asset_id: Option<String>,
    result_json: Option<String>,
    error: Option<String>,
}

fn load_render_job(connection: &Connection, job_id: &str) -> Result<RenderJob> {
    let stored = connection
        .query_row(
            "SELECT job_id, project_id, revision, preset_id, retry_of_job_id, retry_number, status, attempt, cancel_requested, \
                created_at, started_at, finished_at, output_asset_id, result_json, error \
             FROM render_jobs WHERE job_id = ?1",
            [job_id],
            |row| {
                Ok(StoredRenderJob {
                    job_id: row.get(0)?,
                    project_id: row.get(1)?,
                    revision: row.get(2)?,
                    preset_id: row.get(3)?,
                    retry_of_job_id: row.get(4)?,
                    retry_number: row.get(5)?,
                    status: row.get(6)?,
                    attempt: row.get(7)?,
                    cancel_requested: row.get(8)?,
                    created_at: row.get(9)?,
                    started_at: row.get(10)?,
                    finished_at: row.get(11)?,
                    output_asset_id: row.get(12)?,
                    result_json: row.get(13)?,
                    error: row.get(14)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| anyhow!("JOB_NOT_FOUND: render job {job_id} does not exist"))?;
    let stored_result: Option<RenderResult> = stored
        .result_json
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .context("stored render job result is invalid")?;
    let result = stored_result.map(|result| RenderJobResult {
        timeline_sha256: result.timeline_sha256,
        output_sha256: result.output_sha256,
        output_duration_seconds: result.output_duration_seconds,
        output_width: result.output_width,
        output_height: result.output_height,
        output_frame_count: result.output_frame_count,
        output_frame_rate: result.output_frame_rate,
        video_codec: result.video_codec,
        audio_codec: result.audio_codec,
    });
    Ok(RenderJob {
        job_id: stored.job_id,
        project_id: stored.project_id,
        revision: from_sql_revision(stored.revision)?,
        preset_id: stored.preset_id,
        retry_of_job_id: stored.retry_of_job_id,
        retry_number: u32::try_from(stored.retry_number)
            .context("database contains an invalid retry number")?,
        status: RenderJobStatus::from_database_value(&stored.status)?,
        attempt: u32::try_from(stored.attempt)
            .context("database contains an invalid job attempt")?,
        cancel_requested: stored.cancel_requested,
        created_at: stored.created_at,
        started_at: stored.started_at,
        finished_at: stored.finished_at,
        output_asset_id: stored.output_asset_id,
        result,
        error: stored.error,
    })
}

fn render_request_digest(project_id: &str, revision: u64, preset_id: &str) -> Result<String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct RequestDigest<'a> {
        project_id: &'a str,
        revision: u64,
        preset_id: &'a str,
    }
    let bytes = serde_json::to_vec(&RequestDigest {
        project_id,
        revision,
        preset_id,
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn retry_request_digest(source_job_id: &str) -> Result<String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct RetryRequest<'a> {
        source_job_id: &'a str,
    }
    let bytes = serde_json::to_vec(&RetryRequest { source_job_id })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn unix_time_seconds() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    i64::try_from(duration.as_secs()).context("system clock is out of range")
}

fn truncate_error(error: &str, max_bytes: usize) -> String {
    if error.len() <= max_bytes {
        return error.to_owned();
    }
    let mut end = max_bytes;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &error[..end])
}

pub struct RenderRevisionOptions {
    pub database: PathBuf,
    pub workspace: PathBuf,
    pub project_id: String,
    pub revision: u64,
    pub output: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub font: Option<PathBuf>,
    pub timeout_seconds: u64,
}

pub fn render_revision(options: RenderRevisionOptions) -> Result<RenderResult> {
    render_revision_cancellable(options, None)
}

pub(crate) fn render_revision_cancellable(
    options: RenderRevisionOptions,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<RenderResult> {
    render_revision_with_preset_cancellable(options, RenderPreset::Export, cancellation)
}

pub(crate) fn render_revision_with_preset_cancellable(
    options: RenderRevisionOptions,
    preset: RenderPreset,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<RenderResult> {
    let store = ProjectStore::open(&options.database)?;
    let timeline_json = store.load_revision_json(&options.project_id, options.revision)?;
    let workspace = Workspace::open(&options.workspace)?;
    let timeline_path = workspace
        .temporary_dir()
        .join(format!("revision-{}.json", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&timeline_path)
        .context("could not stage stored project revision")?;
    let _cleanup = RemoveTimelineOnDrop(timeline_path.clone());
    file.write_all(timeline_json.as_bytes())?;
    file.sync_all()?;

    crate::render::render_timeline_with_preset_cancellable(
        RenderOptions {
            workspace: options.workspace,
            timeline: timeline_path,
            output: options.output,
            ffmpeg: options.ffmpeg,
            ffprobe: options.ffprobe,
            font: options.font,
            timeout_seconds: options.timeout_seconds,
        },
        preset,
        cancellation,
    )
}

pub fn read_edit_request(path: &Path) -> Result<EditRequest> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("could not inspect edit request {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_EDIT_FILE_BYTES {
        bail!("edit request must be a file no larger than 1 MiB");
    }
    let request: EditRequest = serde_json::from_slice(
        &fs::read(path)
            .with_context(|| format!("could not read edit request {}", path.display()))?,
    )
    .context("edit request JSON is invalid or contains unsupported fields")?;
    if request.operations.is_empty() || request.operations.len() > MAX_BATCH_OPERATIONS {
        bail!("edit batch must contain between 1 and 100 operations");
    }
    Ok(request)
}

fn apply_operations(
    timeline: &mut Timeline,
    operations: &[EditOperation],
    workspace: &Workspace,
) -> Result<()> {
    for (index, operation) in operations.iter().enumerate() {
        match operation {
            EditOperation::ClipAdd {
                track_id,
                clip,
                index: insert_at,
            } => {
                let track_index = timeline
                    .tracks
                    .iter()
                    .position(|track| track.id == *track_id)
                    .ok_or_else(|| {
                        anyhow!("operation {} track {track_id} was not found", index + 1)
                    })?;
                let track = &timeline.tracks[track_index];
                if track.locked {
                    bail!(
                        "operation {} cannot edit locked track {}",
                        index + 1,
                        track.id
                    );
                }
                if !clip_matches_track(clip, track.kind) {
                    bail!(
                        "operation {} clip kind does not match track {}",
                        index + 1,
                        track_id
                    );
                }
                let insert_at = insert_at.unwrap_or(track.clips.len());
                if insert_at > track.clips.len() {
                    bail!("operation {} insertion index is out of range", index + 1);
                }

                if let Some(asset_id) = clip_asset_id(clip) {
                    let (_, manifest) = workspace.load_asset(asset_id)?;
                    let timeline_asset = TimelineAsset {
                        kind: manifest.kind,
                        uri: format!("asset://{asset_id}"),
                        sha256: manifest.sha256,
                    };
                    match timeline.assets.get(asset_id) {
                        Some(existing)
                            if existing.kind != timeline_asset.kind
                                || existing.sha256 != timeline_asset.sha256 =>
                        {
                            bail!("operation {} asset manifest changed", index + 1);
                        }
                        Some(_) => {}
                        None => {
                            timeline.assets.insert(asset_id.to_owned(), timeline_asset);
                        }
                    }
                }

                let track = &mut timeline.tracks[track_index];
                if track.kind == TrackKind::Video {
                    sort_video_clips(track);
                }
                let insert_at = insert_at.min(track.clips.len());
                track.clips.insert(insert_at, clip.clone());
                if track.kind == TrackKind::Video {
                    reflow_video_track(track)?;
                }
            }
            EditOperation::ClipRemove { clip_id } => {
                let (track_index, clip_index) = find_clip_position(timeline, clip_id)?;
                let track = &mut timeline.tracks[track_index];
                if track.locked {
                    bail!(
                        "operation {} cannot edit locked track {}",
                        index + 1,
                        track.id
                    );
                }
                track.clips.remove(clip_index);
                if track.kind == TrackKind::Video {
                    sort_video_clips(track);
                    reflow_video_track(track)?;
                }
            }
            EditOperation::ClipMove {
                clip_id,
                start_frame,
            } => {
                let (track_index, clip_index) = find_clip_position(timeline, clip_id)?;
                let track = &mut timeline.tracks[track_index];
                if track.locked {
                    bail!(
                        "operation {} cannot edit locked track {}",
                        index + 1,
                        track.id
                    );
                }
                match &mut track.clips[clip_index] {
                    Clip::Video(_) => bail!(
                        "operation {} video clips must be reordered on the contiguous video track",
                        index + 1
                    ),
                    Clip::Audio(clip) => clip.start_frame = *start_frame,
                    Clip::Text(clip) => clip.start_frame = *start_frame,
                    Clip::Caption(_) | Clip::Image(_) => {
                        bail!("operation {} targets an unsupported clip", index + 1)
                    }
                }
            }
            EditOperation::ClipReorder {
                clip_id,
                destination_index,
            } => {
                let (track_index, clip_index) = find_clip_position(timeline, clip_id)?;
                let track = &mut timeline.tracks[track_index];
                if track.kind != TrackKind::Video
                    || !matches!(track.clips[clip_index], Clip::Video(_))
                {
                    bail!("operation {} clip.reorder targets a video clip", index + 1);
                }
                if track.locked {
                    bail!(
                        "operation {} cannot edit locked track {}",
                        index + 1,
                        track.id
                    );
                }
                sort_video_clips(track);
                let clip_index = track
                    .clips
                    .iter()
                    .position(|clip| matches!(clip, Clip::Video(video) if video.id == *clip_id))
                    .expect("validated video clip remains in its track");
                if *destination_index >= track.clips.len() {
                    bail!("operation {} destination index is out of range", index + 1);
                }
                let clip = track.clips.remove(clip_index);
                track.clips.insert(*destination_index, clip);
                reflow_video_track(track)?;
            }
            EditOperation::ClipTrim {
                clip_id,
                start_frame,
                duration_frames,
                source_in,
                source_out,
            } => {
                let (track_index, clip_index) = find_clip_position(timeline, clip_id)?;
                let track = &mut timeline.tracks[track_index];
                if track.locked {
                    bail!(
                        "operation {} cannot edit locked track {}",
                        index + 1,
                        track.id
                    );
                }
                match &mut track.clips[clip_index] {
                    Clip::Video(clip) => {
                        if *start_frame != clip.start_frame {
                            bail!(
                                "operation {} cannot create gaps on the contiguous video track",
                                index + 1
                            );
                        }
                        set_source_range(clip_id, source_in, source_out, index)?;
                        clip.duration_frames = *duration_frames;
                        clip.source_in = source_in.clone().expect("checked source range");
                        clip.source_out = source_out.clone().expect("checked source range");
                        reflow_video_track(track)?;
                    }
                    Clip::Audio(clip) => {
                        set_source_range(clip_id, source_in, source_out, index)?;
                        clip.start_frame = *start_frame;
                        clip.duration_frames = *duration_frames;
                        clip.source_in = source_in.clone().expect("checked source range");
                        clip.source_out = source_out.clone().expect("checked source range");
                    }
                    Clip::Text(clip) => {
                        if source_in.is_some() || source_out.is_some() {
                            bail!(
                                "operation {} text clips do not have source ranges",
                                index + 1
                            );
                        }
                        clip.start_frame = *start_frame;
                        clip.duration_frames = *duration_frames;
                    }
                    Clip::Caption(_) | Clip::Image(_) => {
                        bail!("operation {} targets an unsupported clip", index + 1)
                    }
                }
            }
            EditOperation::ClipSplit {
                clip_id,
                split_frame,
            } => {
                let (track_index, clip_index) = find_clip_position(timeline, clip_id)?;
                let track = &mut timeline.tracks[track_index];
                if track.locked {
                    bail!(
                        "operation {} cannot edit locked track {}",
                        index + 1,
                        track.id
                    );
                }
                split_clip(track, clip_index, *split_frame)?;
            }
            EditOperation::TextUpdate {
                clip_id,
                text,
                font_size,
                color,
                x,
                y,
            } => {
                if text.is_none()
                    && font_size.is_none()
                    && color.is_none()
                    && x.is_none()
                    && y.is_none()
                {
                    bail!(
                        "operation {} text.update must change at least one property",
                        index + 1
                    );
                }
                let mut found = false;
                for track in &mut timeline.tracks {
                    let track_kind = track.kind;
                    let locked = track.locked;
                    if let Some(Clip::Text(clip)) = track.clips.iter_mut().find(
                        |clip| matches!(clip, Clip::Text(text_clip) if text_clip.id == *clip_id),
                    ) {
                        if track_kind != TrackKind::Overlay {
                            bail!(
                                "operation {} targets a text clip on a non-overlay track",
                                index + 1
                            );
                        }
                        if locked {
                            bail!(
                                "operation {} cannot edit locked track {}",
                                index + 1,
                                track.id
                            );
                        }
                        apply_text_update(clip, text, *font_size, color, *x, *y);
                        found = true;
                        break;
                    }
                }
                if !found {
                    bail!(
                        "operation {} text.update clip {clip_id} was not found",
                        index + 1
                    );
                }
            }
            EditOperation::AudioGainSet { clip_id, gain } => {
                let mut found = false;
                for track in &mut timeline.tracks {
                    let track_kind = track.kind;
                    let locked = track.locked;
                    if let Some(Clip::Audio(clip)) = track.clips.iter_mut().find(
                        |clip| matches!(clip, Clip::Audio(audio_clip) if audio_clip.id == *clip_id),
                    ) {
                        if track_kind != TrackKind::Audio {
                            bail!(
                                "operation {} targets an audio clip on a non-audio track",
                                index + 1
                            );
                        }
                        if locked {
                            bail!(
                                "operation {} cannot edit locked track {}",
                                index + 1,
                                track.id
                            );
                        }
                        clip.gain = *gain;
                        found = true;
                        break;
                    }
                }
                if !found {
                    bail!(
                        "operation {} audio.gain.set clip {clip_id} was not found",
                        index + 1
                    );
                }
            }
        }
    }
    Ok(())
}

fn clip_matches_track(clip: &Clip, track_kind: TrackKind) -> bool {
    matches!(
        (track_kind, clip),
        (TrackKind::Video, Clip::Video(_))
            | (TrackKind::Audio, Clip::Audio(_))
            | (TrackKind::Overlay, Clip::Text(_))
    )
}

fn clip_asset_id(clip: &Clip) -> Option<&str> {
    match clip {
        Clip::Video(clip) => Some(&clip.asset_id),
        Clip::Audio(clip) => Some(&clip.asset_id),
        Clip::Text(_) | Clip::Caption(_) | Clip::Image(_) => None,
    }
}

fn find_clip_position(timeline: &Timeline, clip_id: &str) -> Result<(usize, usize)> {
    timeline
        .tracks
        .iter()
        .enumerate()
        .find_map(|(track_index, track)| {
            track
                .clips
                .iter()
                .position(|clip| clip_id_of(clip) == clip_id)
                .map(|clip_index| (track_index, clip_index))
        })
        .ok_or_else(|| anyhow!("clip {clip_id} was not found"))
}

fn clip_id_of(clip: &Clip) -> &str {
    match clip {
        Clip::Video(clip) => &clip.id,
        Clip::Audio(clip) => &clip.id,
        Clip::Text(clip) => &clip.id,
        Clip::Caption(clip) => &clip.id,
        Clip::Image(clip) => &clip.id,
    }
}

fn sort_video_clips(track: &mut Track) {
    track.clips.sort_by_key(Clip::start_frame);
}

fn reflow_video_track(track: &mut Track) -> Result<()> {
    let mut cursor = 0_u64;
    for clip in &mut track.clips {
        let Clip::Video(video) = clip else {
            bail!("video tracks may contain only video clips");
        };
        video.start_frame = cursor;
        cursor = cursor
            .checked_add(video.duration_frames)
            .context("video track frame position overflowed")?;
    }
    Ok(())
}

fn set_source_range(
    clip_id: &str,
    source_in: &Option<SourceTimestamp>,
    source_out: &Option<SourceTimestamp>,
    operation_index: usize,
) -> Result<()> {
    if source_in.is_none() || source_out.is_none() {
        bail!(
            "operation {} clip.trim for {clip_id} requires sourceIn and sourceOut",
            operation_index + 1
        );
    }
    Ok(())
}

fn split_clip(track: &mut Track, clip_index: usize, split_frame: u64) -> Result<()> {
    let clip = track.clips[clip_index].clone();
    let start_frame = clip.start_frame();
    let end_frame = start_frame
        .checked_add(clip.duration_frames())
        .context("clip frame range overflowed")?;
    if split_frame <= start_frame || split_frame >= end_frame {
        bail!("splitFrame must be inside the selected clip");
    }
    let first_duration = split_frame - start_frame;
    let second_duration = end_frame - split_frame;
    let second_id = Uuid::new_v4().to_string();

    match clip {
        Clip::Video(mut first) => {
            if track.kind != TrackKind::Video {
                bail!("video clips must be on a video track");
            }
            let source_split = source_timestamp_at_ratio(
                &first.source_in,
                &first.source_out,
                first_duration,
                first.duration_frames,
            )?;
            let original_source_out = first.source_out.clone();
            first.duration_frames = first_duration;
            first.source_out = source_split.clone();
            let second = VideoClip {
                id: second_id,
                asset_id: first.asset_id.clone(),
                start_frame: split_frame,
                duration_frames: second_duration,
                source_in: source_split,
                source_out: original_source_out,
                transform: first.transform.clone(),
                effects: first.effects.clone(),
            };
            track.clips[clip_index] = Clip::Video(first);
            track.clips.insert(clip_index + 1, Clip::Video(second));
            reflow_video_track(track)?;
        }
        Clip::Audio(mut first) => {
            if track.kind != TrackKind::Audio {
                bail!("audio clips must be on an audio track");
            }
            let source_split = source_timestamp_at_ratio(
                &first.source_in,
                &first.source_out,
                first_duration,
                first.duration_frames,
            )?;
            let original_source_out = first.source_out.clone();
            first.duration_frames = first_duration;
            first.source_out = source_split.clone();
            let second = AudioClip {
                id: second_id,
                asset_id: first.asset_id.clone(),
                start_frame: split_frame,
                duration_frames: second_duration,
                source_in: source_split,
                source_out: original_source_out,
                gain: first.gain,
            };
            track.clips[clip_index] = Clip::Audio(first);
            track.clips.insert(clip_index + 1, Clip::Audio(second));
        }
        Clip::Text(mut first) => {
            if track.kind != TrackKind::Overlay {
                bail!("text clips must be on an overlay track");
            }
            first.duration_frames = first_duration;
            let second = TextClip {
                id: second_id,
                start_frame: split_frame,
                duration_frames: second_duration,
                text: first.text.clone(),
                font_size: first.font_size,
                color: first.color.clone(),
                x: first.x,
                y: first.y,
            };
            track.clips[clip_index] = Clip::Text(first);
            track.clips.insert(clip_index + 1, Clip::Text(second));
        }
        Clip::Caption(_) | Clip::Image(_) => {
            bail!("captions and images cannot be split by the first renderer")
        }
    }
    Ok(())
}

fn apply_text_update(
    clip: &mut TextClip,
    text: &Option<String>,
    font_size: Option<u32>,
    color: &Option<String>,
    x: Option<u32>,
    y: Option<u32>,
) {
    if let Some(text) = text {
        clip.text.clone_from(text);
    }
    if let Some(font_size) = font_size {
        clip.font_size = font_size;
    }
    if let Some(color) = color {
        clip.color.clone_from(color);
    }
    if let Some(x) = x {
        clip.x = x;
    }
    if let Some(y) = y {
        clip.y = y;
    }
}

fn edit_request_digest(expected_revision: u64, operations: &[EditOperation]) -> Result<String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct CanonicalRequest<'a> {
        expected_revision: u64,
        operations: &'a [EditOperation],
    }
    let bytes = serde_json::to_vec(&CanonicalRequest {
        expected_revision,
        operations,
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn restore_request_digest(target_revision: u64, expected_revision: u64) -> Result<String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct CanonicalRestoreRequest {
        target_revision: u64,
        expected_revision: u64,
    }
    let bytes = serde_json::to_vec(&CanonicalRestoreRequest {
        target_revision,
        expected_revision,
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn validate_project_id(project_id: &str) -> Result<()> {
    let parsed = Uuid::parse_str(project_id).context("projectId must be a UUID")?;
    if parsed.to_string() != project_id.to_ascii_lowercase() {
        bail!("projectId must use canonical hyphenated UUID form");
    }
    Ok(())
}

fn validate_idempotency_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES || key.chars().any(char::is_control)
    {
        bail!("idempotency key must be 1–128 UTF-8 bytes and contain no control characters");
    }
    Ok(())
}

fn to_sql_revision(revision: u64) -> Result<i64> {
    i64::try_from(revision).context("revision exceeds SQLite's supported integer range")
}

fn from_sql_revision(revision: i64) -> Result<u64> {
    u64::try_from(revision).context("database contains an invalid revision")
}

struct RemoveTimelineOnDrop(PathBuf);

impl Drop for RemoveTimelineOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{AssetKind, MediaProbe, StreamInfo};
    use crate::workspace::AssetManifest;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn text_update_is_typed_and_changes_only_requested_fields() {
        let mut timeline: Timeline =
            serde_json::from_str(include_str!("../tests/fixtures/timeline-schema-valid.json"))
                .unwrap();
        timeline.tracks.push(
            serde_json::from_value(json!({
                "id": "titles", "kind": "overlay", "order": 1,
                "clips": [{
                    "id": "headline", "kind": "text", "startFrame": 0,
                    "durationFrames": 24, "text": "Original", "fontSize": 48,
                    "color": "#FFFFFF", "x": 10, "y": 20
                }]
            }))
            .unwrap(),
        );
        let operation: EditOperation = serde_json::from_value(json!({
            "type": "text.update", "clipId": "headline", "text": "Updated", "x": 40
        }))
        .unwrap();

        let root = tempdir().unwrap();
        let workspace = Workspace::open(&root.path().join("workspace")).unwrap();
        apply_operations(&mut timeline, &[operation], &workspace).unwrap();
        let Clip::Text(clip) = &timeline.tracks[1].clips[0] else {
            panic!("expected text clip");
        };
        assert_eq!(clip.text, "Updated");
        assert_eq!(clip.x, 40);
        assert_eq!(clip.font_size, 48);
        assert_eq!(clip.y, 20);
    }

    #[test]
    fn edit_request_rejects_unknown_operation_types() {
        let parsed = serde_json::from_value::<EditRequest>(json!({
            "operations": [{"type": "clip.delete", "clipId": "x"}]
        }));
        assert!(parsed.is_err());
    }

    #[test]
    fn timeline_clip_operations_add_split_trim_move_reorder_and_remove_atomically() {
        const VIDEO_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0b";
        const AUDIO_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0c";
        let root = tempdir().unwrap();
        let workspace = Workspace::open(&root.path().join("workspace")).unwrap();
        let video_manifest = write_test_asset(
            &workspace,
            VIDEO_ID,
            AssetKind::Video,
            MediaProbe {
                format_name: "mp4".to_owned(),
                duration: "1.000000".to_owned(),
                duration_seconds: 1.0,
                has_video: true,
                has_audio: false,
                video: Some(StreamInfo {
                    codec: "h264".to_owned(),
                    duration_seconds: Some(1.0),
                    frame_rate: Some("24/1".to_owned()),
                    width: Some(640),
                    height: Some(360),
                    sample_rate: None,
                    channels: None,
                }),
                audio: None,
            },
        );
        let _audio_manifest = write_test_asset(
            &workspace,
            AUDIO_ID,
            AssetKind::Audio,
            MediaProbe {
                format_name: "wav".to_owned(),
                duration: "1.000000".to_owned(),
                duration_seconds: 1.0,
                has_video: false,
                has_audio: true,
                video: None,
                audio: Some(StreamInfo {
                    codec: "pcm_s16le".to_owned(),
                    duration_seconds: Some(1.0),
                    frame_rate: None,
                    width: None,
                    height: None,
                    sample_rate: Some(48_000),
                    channels: Some(2),
                }),
            },
        );

        let mut document: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/timeline-schema-valid.json"))
                .unwrap();
        document["assets"][VIDEO_ID]["sha256"] = json!(video_manifest.sha256);
        document["tracks"].as_array_mut().unwrap().extend([
            json!({"id":"audio-1","kind":"audio","order":1,"clips":[]}),
            json!({"id":"audio-2","kind":"audio","order":2,"clips":[]}),
            json!({"id":"titles","kind":"overlay","order":3,"clips":[]}),
        ]);
        let mut timeline: Timeline = serde_json::from_value(document).unwrap();

        let operations = vec![
            serde_json::from_value(json!({
                "type":"clip.add", "trackId":"video-main", "index":0,
                "clip":{
                    "id":"second-shot","kind":"video","assetId":VIDEO_ID,
                    "startFrame":0,"durationFrames":12,
                    "sourceIn":{"ticks":0,"timebase_num":1,"timebase_den":1},
                    "sourceOut":{"ticks":500_000,"timebase_num":1,"timebase_den":1_000_000}
                }
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.split", "clipId":"second-shot", "splitFrame":6
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.reorder", "clipId":"clip-1", "destinationIndex":0
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.trim", "clipId":"clip-1", "startFrame":0,
                "durationFrames":12,
                "sourceIn":{"ticks":0,"timebase_num":1,"timebase_den":1},
                "sourceOut":{"ticks":500_000,"timebase_num":1,"timebase_den":1_000_000}
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.add", "trackId":"audio-1",
                "clip":{
                    "id":"music","kind":"audio","assetId":AUDIO_ID,
                    "startFrame":0,"durationFrames":24,
                    "sourceIn":{"ticks":0,"timebase_num":1,"timebase_den":1},
                    "sourceOut":{"ticks":1,"timebase_num":1,"timebase_den":1},"gain":1.0
                }
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.trim", "clipId":"music", "startFrame":5,
                "durationFrames":12,
                "sourceIn":{"ticks":250_000,"timebase_num":1,"timebase_den":1_000_000},
                "sourceOut":{"ticks":750_000,"timebase_num":1,"timebase_den":1_000_000}
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.split", "clipId":"music", "splitFrame":11
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"audio.gain.set", "clipId":"music", "gain":0.5
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.add", "trackId":"titles",
                "clip":{"id":"title","kind":"text","startFrame":0,"durationFrames":18,
                    "text":"Avida","fontSize":48,"color":"#FFFFFF","x":20,"y":30}
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.split", "clipId":"title", "splitFrame":9
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.move", "clipId":"title", "startFrame":3
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type":"clip.remove", "clipId":"second-shot"
            }))
            .unwrap(),
        ];

        apply_operations(&mut timeline, &operations, &workspace).unwrap();
        let validated = validate(timeline, &workspace).unwrap();
        assert_eq!(validated.total_frames, 18);
        assert_eq!(validated.timeline.assets.len(), 2);
        let videos = &validated.timeline.tracks[0].clips;
        assert_eq!(videos.len(), 2);
        assert!(
            matches!(&videos[0], Clip::Video(video) if video.id == "clip-1" && video.duration_frames == 12)
        );
        assert_eq!(validated.timeline.tracks[1].clips.len(), 2);
        assert!(
            matches!(&validated.timeline.tracks[1].clips[0], Clip::Audio(audio) if audio.gain == 0.5)
        );
        assert_eq!(validated.timeline.tracks[3].clips.len(), 2);
    }

    fn write_test_asset(
        workspace: &Workspace,
        asset_id: &str,
        kind: AssetKind,
        probe: MediaProbe,
    ) -> AssetManifest {
        use sha2::{Digest, Sha256};

        let bytes = format!("test asset {asset_id}").into_bytes();
        let directory = workspace.root().join("assets").join(asset_id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(crate::workspace::SOURCE_FILENAME), &bytes).unwrap();
        let manifest = AssetManifest {
            schema_version: 1,
            asset_id: asset_id.to_owned(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
            kind,
            file_name: Some(format!("{asset_id}.media")),
            probe,
        };
        fs::write(
            directory.join("asset.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        manifest
    }

    #[test]
    fn version_one_database_migrates_to_render_jobs_without_losing_revisions() {
        const PROJECT_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0a";
        let root = tempdir().unwrap();
        let database = root.path().join("projects.sqlite");
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(include_str!("../migrations/0001_initial.sql"))
                .unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
            connection
                .execute(
                    "INSERT INTO projects (project_id, current_revision) VALUES (?1, 1)",
                    [PROJECT_ID],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO revisions (project_id, revision, timeline_json) VALUES (?1, 1, '{}')",
                    [PROJECT_ID],
                )
                .unwrap();
        }

        let store = ProjectStore::open(&database).unwrap();
        assert_eq!(store.load_revision_json(PROJECT_ID, 1).unwrap(), "{}");
        assert_eq!(
            store
                .connect()
                .unwrap()
                .pragma_query_value(None, "user_version", |row| row.get::<_, i32>(0))
                .unwrap(),
            DATABASE_VERSION
        );
        let queued = store
            .enqueue_render_job(PROJECT_ID, 1, "export", "migrated-job")
            .unwrap();
        assert_eq!(queued.job.status, RenderJobStatus::Queued);
    }

    #[test]
    fn version_two_successful_jobs_migrate_to_retrievable_render_assets() {
        const PROJECT_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0a";
        const JOB_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0d";
        let root = tempdir().unwrap();
        let database = root.path().join("projects.sqlite");
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(include_str!("../migrations/0001_initial.sql"))
                .unwrap();
            connection
                .execute_batch(include_str!("../migrations/0002_render_jobs.sql"))
                .unwrap();
            connection.pragma_update(None, "user_version", 2).unwrap();
            connection
                .execute(
                    "INSERT INTO projects (project_id, current_revision) VALUES (?1, 1)",
                    [PROJECT_ID],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO revisions (project_id, revision, timeline_json) VALUES (?1, 1, '{}')",
                    [PROJECT_ID],
                )
                .unwrap();
            let result = RenderResult {
                output: root.path().join("renders/output.mp4"),
                provenance: root.path().join("renders/output.mp4.provenance.json"),
                timeline_sha256: "1".repeat(64),
                output_sha256: "2".repeat(64),
                output_duration_seconds: 1.0,
                output_width: 320,
                output_height: 240,
                output_frame_count: 24,
                output_frame_rate: "24/1".to_owned(),
                video_codec: "h264".to_owned(),
                audio_codec: None,
            };
            connection
                .execute(
                    "INSERT INTO render_jobs (job_id, project_id, revision, preset_id, idempotency_key, request_sha256, status, attempt, result_json) \
                     VALUES (?1, ?2, 1, 'export', 'old-success', 'digest', 'succeeded', 1, ?3)",
                    rusqlite::params![JOB_ID, PROJECT_ID, serde_json::to_string(&result).unwrap()],
                )
                .unwrap();
        }

        let store = ProjectStore::open(&database).unwrap();
        let job = store.get_render_job(JOB_ID).unwrap();
        assert_eq!(job.status, RenderJobStatus::Succeeded);
        let asset_id = job.output_asset_id.unwrap();
        let asset = store.get_render_asset(&asset_id).unwrap();
        assert_eq!(asset.asset_id, asset_id);
        assert_eq!(asset.job_id, JOB_ID);
        assert_eq!(asset.attempt, 1);
        assert_eq!(asset.output_sha256, "2".repeat(64));
        assert_eq!(asset.preset_id, "export");
    }
}
