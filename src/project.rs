use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    RenderOptions, RenderResult, render_timeline,
    timeline::{
        Clip, TextClip, Timeline, TrackKind, ValidatedTimeline, load_and_validate, validate,
        validate_json_document,
    },
    workspace::Workspace,
};

const DATABASE_VERSION: i32 = 1;
const MAX_EDIT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_BATCH_OPERATIONS: usize = 100;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditRequest {
    pub operations: Vec<EditOperation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum EditOperation {
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResult {
    pub project_id: String,
    pub revision: u64,
    pub replayed: bool,
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
        apply_operations(&mut timeline, operations)?;

        let next_revision = current_revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("project revision overflow"))?;
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

    pub fn get_project_summary(&self, project_id: &str) -> Result<ProjectSummary> {
        validate_project_id(project_id)?;
        let current_revision = self
            .connect()?
            .query_row(
                "SELECT current_revision FROM projects WHERE project_id = ?1",
                [project_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("PROJECT_NOT_FOUND: project {project_id} does not exist"))?;
        Ok(ProjectSummary {
            project_id: project_id.to_owned(),
            current_revision: from_sql_revision(current_revision)?,
        })
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
        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            DATABASE_VERSION => Ok(()),
            0 => {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute_batch(include_str!("../migrations/0001_initial.sql"))?;
                transaction.pragma_update(None, "user_version", DATABASE_VERSION)?;
                transaction.commit()?;
                Ok(())
            }
            other => bail!("unsupported project database version {other}"),
        }
    }
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

    render_timeline(RenderOptions {
        workspace: options.workspace,
        timeline: timeline_path,
        output: options.output,
        ffmpeg: options.ffmpeg,
        ffprobe: options.ffprobe,
        font: options.font,
        timeout_seconds: options.timeout_seconds,
    })
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

fn apply_operations(timeline: &mut Timeline, operations: &[EditOperation]) -> Result<()> {
    for (index, operation) in operations.iter().enumerate() {
        match operation {
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
    use serde_json::json;

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

        apply_operations(&mut timeline, &[operation]).unwrap();
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
}
