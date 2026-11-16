use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use tokio::{task::JoinHandle, time};
use uuid::Uuid;

use crate::{
    ProjectStore, RenderRevisionOptions, Workspace,
    project::{RenderJob, RenderJobPhase, render_revision_with_progress_cancellable},
    render::RenderPreset,
};

const JOB_LEASE_SECONDS: u64 = 45;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const IDLE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Default)]
pub(crate) struct JobCancellationRegistry {
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl JobCancellationRegistry {
    pub(crate) fn request_cancel(&self, job_id: &str) {
        if let Some(flag) = self.lock().get(job_id) {
            flag.store(true, Ordering::Release);
        }
    }

    fn register(&self, job_id: &str, flag: Arc<AtomicBool>) {
        self.lock().insert(job_id.to_owned(), flag);
    }

    fn unregister(&self, job_id: &str) {
        self.lock().remove(job_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AtomicBool>>> {
        self.active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Clone)]
pub struct RenderJobRunner {
    store: ProjectStore,
    workspace: PathBuf,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    font: Option<PathBuf>,
    timeout_seconds: u64,
    worker_id: String,
    cancellation: JobCancellationRegistry,
    stopping: Arc<AtomicBool>,
}

impl RenderJobRunner {
    pub fn new(
        store: ProjectStore,
        workspace: PathBuf,
        ffmpeg: PathBuf,
        ffprobe: PathBuf,
        font: Option<PathBuf>,
        timeout_seconds: u64,
        cancellation: JobCancellationRegistry,
    ) -> Self {
        Self {
            store,
            workspace,
            ffmpeg,
            ffprobe,
            font,
            timeout_seconds,
            worker_id: Uuid::new_v4().to_string(),
            cancellation,
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn request_shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    pub async fn run(&self) -> Result<()> {
        while !self.stopping.load(Ordering::Acquire) {
            match self.run_once().await {
                Ok(true) => {}
                Ok(false) => time::sleep(IDLE_INTERVAL).await,
                Err(error) => {
                    eprintln!("render job worker error: {error:#}");
                    time::sleep(IDLE_INTERVAL).await;
                }
            }
        }
        Ok(())
    }

    pub async fn run_once(&self) -> Result<bool> {
        if self.stopping.load(Ordering::Acquire) {
            return Ok(false);
        }
        let workspace = Workspace::open(&self.workspace)?;
        for expired in self.store.recover_expired_render_jobs()? {
            if let Err(error) = workspace.cleanup_render_attempt(&expired.job_id, expired.attempt) {
                eprintln!(
                    "could not clean expired render attempt {}:{}: {error:#}",
                    expired.job_id, expired.attempt
                );
            }
        }
        let Some(job) = self
            .store
            .claim_next_render_job(&self.worker_id, JOB_LEASE_SECONDS)?
        else {
            return Ok(false);
        };
        self.process_job(job, workspace).await?;
        Ok(true)
    }

    async fn process_job(&self, job: RenderJob, workspace: Workspace) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            self.store
                .requeue_render_job(&job.job_id, &self.worker_id)?;
            return Ok(());
        }
        match self
            .store
            .heartbeat_render_job(&job.job_id, &self.worker_id, JOB_LEASE_SECONDS)?
        {
            Some(true) => {
                self.store
                    .finish_cancelled_render_job(&job.job_id, &self.worker_id)?;
                return Ok(());
            }
            Some(false) => {}
            None => return Ok(()),
        }
        let preset = match RenderPreset::parse(&job.preset_id) {
            Ok(preset) => preset,
            Err(error) => {
                self.store
                    .fail_render_job(&job.job_id, &self.worker_id, &format!("{error:#}"))?;
                return Err(error);
            }
        };
        let attempt_dir = match workspace.create_render_attempt_dir(&job.job_id, job.attempt) {
            Ok(directory) => directory,
            Err(error) => {
                let current = self.store.get_render_job(&job.job_id)?;
                if current.cancel_requested {
                    self.store
                        .finish_cancelled_render_job(&job.job_id, &self.worker_id)?;
                } else {
                    self.store.fail_render_job(
                        &job.job_id,
                        &self.worker_id,
                        &format!("{error:#}"),
                    )?;
                }
                return Err(error);
            }
        };
        let cancellation_flag = Arc::new(AtomicBool::new(false));
        self.cancellation
            .register(&job.job_id, Arc::clone(&cancellation_flag));

        let options = RenderRevisionOptions {
            database: self.store.database_path().to_path_buf(),
            workspace: self.workspace.clone(),
            project_id: job.project_id.clone(),
            revision: job.revision,
            output: attempt_dir.join("output.mp4"),
            ffmpeg: self.ffmpeg.clone(),
            ffprobe: self.ffprobe.clone(),
            font: self.font.clone(),
            timeout_seconds: self.timeout_seconds,
        };
        let progress_store = self.store.clone();
        let progress_job_id = job.job_id.clone();
        let progress_worker_id = self.worker_id.clone();
        let progress: crate::render::RenderProgressCallback = Arc::new(move |phase, percent| {
            if let Err(error) = progress_store.update_render_job_progress(
                &progress_job_id,
                &progress_worker_id,
                phase,
                percent,
            ) {
                eprintln!("could not update render job progress {progress_job_id}: {error:#}");
            }
        });
        let render_flag = Arc::clone(&cancellation_flag);
        let mut render_task: JoinHandle<Result<_>> = tokio::task::spawn_blocking(move || {
            render_revision_with_progress_cancellable(
                options,
                preset,
                Some(render_flag),
                Some(progress),
            )
        });
        let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
        heartbeat.tick().await;
        let mut requested_cancel = false;
        let mut lost_lease = false;
        let render_result = loop {
            tokio::select! {
                result = &mut render_task => break result,
                _ = heartbeat.tick() => {
                    match self.store.heartbeat_render_job(
                        &job.job_id,
                        &self.worker_id,
                        JOB_LEASE_SECONDS,
                    ) {
                        Ok(Some(true)) => {
                            requested_cancel = true;
                            cancellation_flag.store(true, Ordering::Release);
                        }
                        Ok(Some(false)) => {}
                        Ok(None) => {
                            lost_lease = true;
                            cancellation_flag.store(true, Ordering::Release);
                        }
                        Err(error) => {
                            eprintln!("could not renew render job lease {}: {error:#}", job.job_id);
                            lost_lease = true;
                            cancellation_flag.store(true, Ordering::Release);
                        }
                    }
                    if self.stopping.load(Ordering::Acquire) {
                        cancellation_flag.store(true, Ordering::Release);
                    }
                }
            }
        };
        self.cancellation.unregister(&job.job_id);

        let render_result = match render_result {
            Ok(result) => result,
            Err(error) => Err(anyhow!("render worker panicked or was cancelled: {error}")),
        };
        match render_result {
            Ok(mut result) => {
                if cancellation_flag.load(Ordering::Acquire) {
                    workspace.cleanup_temporary_render_attempt(&job.job_id, job.attempt)?;
                    let current = self.store.get_render_job(&job.job_id)?;
                    if requested_cancel || current.cancel_requested {
                        self.store
                            .finish_cancelled_render_job(&job.job_id, &self.worker_id)?;
                    } else if self.stopping.load(Ordering::Acquire) {
                        self.store
                            .requeue_render_job(&job.job_id, &self.worker_id)?;
                    }
                    return Ok(());
                }
                if let Err(error) = self.store.update_render_job_progress(
                    &job.job_id,
                    &self.worker_id,
                    RenderJobPhase::Publishing,
                    98,
                ) {
                    eprintln!(
                        "could not update render job progress {}: {error:#}",
                        job.job_id
                    );
                }
                let final_dir = match workspace.publish_render_attempt(&job.job_id, job.attempt) {
                    Ok(directory) => directory,
                    Err(error) => {
                        self.settle_failed_attempt(
                            &job,
                            &workspace,
                            &format!("{error:#}"),
                            requested_cancel,
                            lost_lease,
                        )?;
                        return Ok(());
                    }
                };
                result.output = final_dir.join("output.mp4");
                result.provenance =
                    PathBuf::from(format!("{}.provenance.json", result.output.display()));
                let output_asset_id = Uuid::new_v4().to_string();
                if !self.store.complete_render_job(
                    &job.job_id,
                    &self.worker_id,
                    &output_asset_id,
                    &result,
                )? {
                    workspace.cleanup_render_attempt(&job.job_id, job.attempt)?;
                    if self.store.get_render_job(&job.job_id)?.cancel_requested {
                        self.store
                            .finish_cancelled_render_job(&job.job_id, &self.worker_id)?;
                    }
                }
            }
            Err(error) => {
                self.settle_failed_attempt(
                    &job,
                    &workspace,
                    &format!("{error:#}"),
                    requested_cancel,
                    lost_lease,
                )?;
            }
        }
        Ok(())
    }

    fn settle_failed_attempt(
        &self,
        job: &RenderJob,
        workspace: &Workspace,
        error: &str,
        requested_cancel: bool,
        lost_lease: bool,
    ) -> Result<()> {
        workspace.cleanup_temporary_render_attempt(&job.job_id, job.attempt)?;
        let current = self.store.get_render_job(&job.job_id)?;
        if requested_cancel || current.cancel_requested {
            self.store
                .finish_cancelled_render_job(&job.job_id, &self.worker_id)?;
        } else if self.stopping.load(Ordering::Acquire) {
            self.store
                .requeue_render_job(&job.job_id, &self.worker_id)?;
        } else if !lost_lease {
            self.store
                .fail_render_job(&job.job_id, &self.worker_id, error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command};

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::{ImportOptions, RenderJobStatus, import_asset};

    const PROJECT_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0a";

    #[tokio::test]
    async fn expired_attempt_is_cleaned_and_retried_after_worker_restart() {
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let workspace = Workspace::open(&workspace_path).unwrap();
        let database = root.path().join("projects.sqlite");
        let store = ProjectStore::open(&database).unwrap();
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
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

        let submitted = store
            .enqueue_render_job(PROJECT_ID, 1, "preview", "recover-after-restart")
            .unwrap();
        let abandoned = store
            .claim_next_render_job("stopped-worker", 60)
            .unwrap()
            .unwrap();
        assert_eq!(abandoned.attempt, 1);
        let abandoned_attempt = workspace
            .create_render_attempt_dir(&submitted.job.job_id, abandoned.attempt)
            .unwrap();
        fs::write(abandoned_attempt.join("partial.mp4"), b"partial").unwrap();
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE render_jobs SET lease_expires_at = 0 WHERE job_id = ?1",
                [&submitted.job.job_id],
            )
            .unwrap();

        let runner = RenderJobRunner::new(
            store.clone(),
            workspace_path.clone(),
            "missing-ffmpeg".into(),
            "missing-ffprobe".into(),
            None,
            30,
            JobCancellationRegistry::default(),
        );
        assert!(runner.run_once().await.unwrap());

        let final_job = store.get_render_job(&submitted.job.job_id).unwrap();
        assert_eq!(final_job.status, RenderJobStatus::Failed);
        assert_eq!(final_job.attempt, 2);
        assert!(
            !workspace_path
                .join("tmp/render-jobs")
                .join(&submitted.job.job_id)
                .join("attempt-1")
                .exists()
        );
        assert!(
            !workspace_path
                .join("tmp/render-jobs")
                .join(&submitted.job.job_id)
                .join("attempt-2")
                .exists()
        );
        assert!(
            !workspace_path
                .join("renders")
                .join(&submitted.job.job_id)
                .exists()
        );

        let retry_one = store
            .retry_render_job(&submitted.job.job_id, "retry-once")
            .unwrap();
        assert_eq!(retry_one.job.status, RenderJobStatus::Queued);
        assert_eq!(retry_one.job.preset_id, "preview");
        assert_eq!(retry_one.job.retry_number, 1);
        assert_eq!(
            retry_one.job.retry_of_job_id.as_deref(),
            Some(submitted.job.job_id.as_str())
        );
        let replay = store
            .retry_render_job(&submitted.job.job_id, "retry-once")
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.job.job_id, retry_one.job.job_id);
        let stale_retry = store
            .retry_render_job(&submitted.job.job_id, "retry-again-too-early")
            .unwrap_err();
        assert!(format!("{stale_retry:#}").contains("JOB_NOT_RETRYABLE"));

        let retry_one_claim = store
            .claim_next_render_job("retry-worker-1", JOB_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        assert!(
            store
                .fail_render_job(
                    &retry_one_claim.job_id,
                    "retry-worker-1",
                    "synthetic failure"
                )
                .unwrap()
        );
        let retry_two = store
            .retry_render_job(&retry_one.job.job_id, "retry-twice")
            .unwrap();
        assert_eq!(retry_two.job.retry_number, 2);

        let retry_two_claim = store
            .claim_next_render_job("retry-worker-2", JOB_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        assert!(
            store
                .fail_render_job(
                    &retry_two_claim.job_id,
                    "retry-worker-2",
                    "synthetic failure"
                )
                .unwrap()
        );
        let retry_limit = store
            .retry_render_job(&retry_two.job.job_id, "retry-three-times")
            .unwrap_err();
        assert!(format!("{retry_limit:#}").contains("JOB_RETRY_LIMIT"));
    }

    #[test]
    fn render_job_progress_is_persisted_and_never_moves_backwards() {
        let root = tempdir().unwrap();
        let database = root.path().join("projects.sqlite");
        let store = ProjectStore::open(&database).unwrap();
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
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
        let submitted = store
            .enqueue_render_job(PROJECT_ID, 1, "export", "progress-test")
            .unwrap();
        let claimed = store
            .claim_next_render_job("progress-worker", JOB_LEASE_SECONDS)
            .unwrap()
            .unwrap();

        assert!(
            store
                .update_render_job_progress(
                    &claimed.job_id,
                    "progress-worker",
                    RenderJobPhase::Encoding,
                    50,
                )
                .unwrap()
        );
        assert!(
            !store
                .update_render_job_progress(
                    &claimed.job_id,
                    "progress-worker",
                    RenderJobPhase::Encoding,
                    20,
                )
                .unwrap()
        );
        assert!(
            store
                .update_render_job_progress(
                    &claimed.job_id,
                    "progress-worker",
                    RenderJobPhase::Validating,
                    92,
                )
                .unwrap()
        );

        let job = store.get_render_job(&submitted.job.job_id).unwrap();
        assert_eq!(job.phase, RenderJobPhase::Validating);
        assert_eq!(job.progress_percent, 92);
        let json = serde_json::to_value(job).unwrap();
        assert_eq!(json["phase"], "validating");
        assert_eq!(json["progressPercent"], 92);
    }

    #[test]
    fn running_cancellation_is_persisted_until_the_worker_acknowledges_it() {
        let root = tempdir().unwrap();
        let database = root.path().join("projects.sqlite");
        let store = ProjectStore::open(&database).unwrap();
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
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
        let submitted = store
            .enqueue_render_job(PROJECT_ID, 1, "export", "cancel-running")
            .unwrap();
        let claimed = store
            .claim_next_render_job("test-worker", JOB_LEASE_SECONDS)
            .unwrap()
            .unwrap();
        let cancellation = store.cancel_render_job(&claimed.job_id).unwrap();
        assert_eq!(cancellation.status, RenderJobStatus::Running);
        assert!(cancellation.cancel_requested);
        assert_eq!(
            store
                .heartbeat_render_job(&claimed.job_id, "test-worker", JOB_LEASE_SECONDS)
                .unwrap(),
            Some(true)
        );
        assert!(
            store
                .finish_cancelled_render_job(&claimed.job_id, "test-worker")
                .unwrap()
        );
        let final_job = store.get_render_job(&submitted.job.job_id).unwrap();
        assert_eq!(final_job.status, RenderJobStatus::Cancelled);
        assert!(final_job.cancel_requested);
        assert!(final_job.finished_at.is_some());
    }

    #[tokio::test]
    async fn worker_publishes_a_completed_render_atomically() {
        let Some(ffmpeg) = find_tool("ffmpeg") else {
            eprintln!("skipping render job integration test: ffmpeg not found");
            return;
        };
        let Some(ffprobe) = find_tool("ffprobe") else {
            eprintln!("skipping render job integration test: ffprobe not found");
            return;
        };
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let source = root.path().join("source.mp4");
        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=24:d=1",
                "-an",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success(), "could not create the render job fixture");

        let asset = import_asset(ImportOptions {
            workspace: workspace_path.clone(),
            file: source,
            ffprobe: ffprobe.clone(),
        })
        .unwrap();
        let timeline = json!({
            "schemaVersion": "1.0.0",
            "projectId": PROJECT_ID,
            "revision": 0,
            "canvas": {
                "width": 1280,
                "height": 720,
                "fps": {"num": 24, "den": 1},
                "background": "#101820"
            },
            "assets": {
                asset.asset_id.clone(): {
                    "kind": asset.kind,
                    "uri": asset.uri,
                    "sha256": asset.sha256
                }
            },
            "tracks": [{
                "id": "video-main",
                "kind": "video",
                "order": 0,
                "clips": [{
                    "id": "picture",
                    "kind": "video",
                    "assetId": asset.asset_id,
                    "startFrame": 0,
                    "durationFrames": 24,
                    "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 24},
                    "sourceOut": {"ticks": 24, "timebase_num": 1, "timebase_den": 24}
                }]
            }]
        });
        let store = ProjectStore::open(root.path().join("projects.sqlite")).unwrap();
        store
            .create_from_timeline_json(&workspace_path, timeline)
            .unwrap();
        let submitted = store
            .enqueue_render_job(PROJECT_ID, 1, "preview", "render-success")
            .unwrap();
        assert_eq!(submitted.job.preset_id, "preview");
        let runner = RenderJobRunner::new(
            store.clone(),
            workspace_path.clone(),
            ffmpeg,
            ffprobe,
            None,
            120,
            JobCancellationRegistry::default(),
        );

        assert!(runner.run_once().await.unwrap());
        let job = store.get_render_job(&submitted.job.job_id).unwrap();
        assert_eq!(job.status, RenderJobStatus::Succeeded);
        assert_eq!(job.phase, RenderJobPhase::Complete);
        assert_eq!(job.progress_percent, 100);
        assert!(job.output_asset_id.is_some());
        let result = job.result.unwrap();
        assert_eq!(result.output_frame_count, 24);
        assert_eq!((result.output_width, result.output_height), (640, 360));
        let workspace = Workspace::open(&workspace_path).unwrap();
        let output = workspace
            .resolve_render_output(&submitted.job.job_id, job.attempt)
            .unwrap();
        assert!(output.is_file());
        assert!(output.starts_with(workspace.renders_dir()));
        let provenance_path = PathBuf::from(format!("{}.provenance.json", output.display()));
        assert!(provenance_path.is_file());
        let provenance: serde_json::Value =
            serde_json::from_slice(&std::fs::read(provenance_path).unwrap()).unwrap();
        assert_eq!(provenance["renderPreset"], "preview");
        assert_eq!(provenance["width"], 640);
        assert_eq!(provenance["height"], 360);
        assert!(
            !workspace_path
                .join("tmp/render-jobs")
                .join(&submitted.job.job_id)
                .join("attempt-1")
                .exists()
        );
    }

    fn find_tool(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            {
                let candidate = directory.join(format!("{name}.exe"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            None
        })
    }
}
