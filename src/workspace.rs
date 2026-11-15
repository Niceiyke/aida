use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::media::{AssetKind, MAX_IMPORT_BYTES, MediaProbe};

pub const SOURCE_FILENAME: &str = "source.media";
const ASSET_MANIFEST_FILENAME: &str = "asset.json";
const UPLOAD_SESSION_FILENAME: &str = "upload.json";
const UPLOAD_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadSessionManifest {
    schema_version: u32,
    upload_id: String,
    expected_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct UploadSession {
    pub directory: PathBuf,
    pub source: PathBuf,
    pub partial: PathBuf,
    pub expected_bytes: u64,
    pub file_name: Option<String>,
    last_activity: SystemTime,
}

impl UploadSession {
    pub(crate) fn is_expired(&self, now: SystemTime) -> bool {
        now.duration_since(self.last_activity)
            .is_ok_and(|age| age >= UPLOAD_SESSION_TTL)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssetManifest {
    pub schema_version: u32,
    pub asset_id: String,
    pub sha256: String,
    pub bytes: u64,
    pub kind: AssetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    pub probe: MediaProbe,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    assets: PathBuf,
    temporary: PathBuf,
    uploads: PathBuf,
    renders: PathBuf,
}

impl Workspace {
    pub fn open(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)
            .with_context(|| format!("could not create workspace {}", path.display()))?;
        let root = fs::canonicalize(path)
            .with_context(|| format!("could not resolve workspace {}", path.display()))?;
        let assets = root.join("assets");
        let temporary = root.join("tmp");
        let uploads = temporary.join("uploads");
        let renders = root.join("renders");
        ensure_safe_directory(&assets)?;
        ensure_safe_directory(&temporary)?;
        ensure_safe_directory(&renders)?;
        Ok(Self {
            root,
            assets,
            temporary,
            uploads,
            renders,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn temporary_dir(&self) -> &Path {
        &self.temporary
    }

    pub fn renders_dir(&self) -> &Path {
        &self.renders
    }

    pub fn create_render_attempt_dir(&self, job_id: &str, attempt: u32) -> Result<PathBuf> {
        validate_asset_id(job_id).context("jobId must be a UUID")?;
        if attempt == 0 {
            bail!("render job attempt must be greater than zero");
        }
        let jobs = self.temporary.join("render-jobs");
        ensure_safe_directory(&jobs)?;
        let job_dir = jobs.join(job_id);
        ensure_safe_directory(&job_dir)?;
        let attempt_dir = job_dir.join(format!("attempt-{attempt}"));
        fs::create_dir(&attempt_dir).with_context(|| {
            format!(
                "could not create render job attempt directory {}",
                attempt_dir.display()
            )
        })?;
        Ok(attempt_dir)
    }

    pub fn publish_render_attempt(&self, job_id: &str, attempt: u32) -> Result<PathBuf> {
        validate_asset_id(job_id).context("jobId must be a UUID")?;
        if attempt == 0 {
            bail!("render job attempt must be greater than zero");
        }
        let temporary_jobs = self.temporary.join("render-jobs");
        let temporary_job = temporary_jobs.join(job_id);
        let source = temporary_job.join(format!("attempt-{attempt}"));
        ensure_not_symlink(&temporary_jobs)?;
        ensure_not_symlink(&temporary_job)?;
        ensure_not_symlink(&source)?;
        let source = fs::canonicalize(&source).context("render job attempt output is missing")?;
        if !source.starts_with(&temporary_jobs) || !source.is_dir() {
            bail!("render job output is outside the workspace temporary directory");
        }

        let destination_job = self.renders.join(job_id);
        ensure_safe_directory(&destination_job)?;
        let destination = destination_job.join(format!("attempt-{attempt}"));
        if fs::symlink_metadata(&destination).is_ok() {
            bail!("render job output already exists");
        }
        fs::rename(&source, &destination).with_context(|| {
            format!(
                "could not publish render job output to {}",
                destination.display()
            )
        })?;
        Ok(destination)
    }

    pub(crate) fn resolve_render_output(&self, job_id: &str, attempt: u32) -> Result<PathBuf> {
        validate_asset_id(job_id).context("jobId must be a UUID")?;
        if attempt == 0 {
            bail!("render job attempt must be greater than zero");
        }
        let job_dir = self.renders.join(job_id);
        let attempt_dir = job_dir.join(format!("attempt-{attempt}"));
        let output = attempt_dir.join("output.mp4");
        ensure_not_symlink(&self.renders)?;
        ensure_not_symlink(&job_dir)?;
        ensure_not_symlink(&attempt_dir)?;
        ensure_not_symlink(&output)?;

        let renders_root = fs::canonicalize(&self.renders)
            .context("could not resolve workspace render directory")?;
        let attempt_dir =
            fs::canonicalize(&attempt_dir).context("published render attempt is unavailable")?;
        let output = fs::canonicalize(&output).context("published render output is unavailable")?;
        if !attempt_dir.starts_with(&renders_root)
            || !output.starts_with(&attempt_dir)
            || !fs::metadata(&output)?.is_file()
        {
            bail!("published render output is outside the workspace render directory");
        }
        Ok(output)
    }

    pub fn cleanup_render_attempt(&self, job_id: &str, attempt: u32) -> Result<()> {
        self.cleanup_temporary_render_attempt(job_id, attempt)?;
        validate_asset_id(job_id).context("jobId must be a UUID")?;
        if attempt == 0 {
            bail!("render job attempt must be greater than zero");
        }
        let render_job = self.renders.join(job_id);
        let render_attempt = render_job.join(format!("attempt-{attempt}"));
        ensure_not_symlink_if_present(&render_job)?;
        remove_owned_directory(&self.renders, &render_attempt)?;
        if let Ok(metadata) = fs::symlink_metadata(&render_job)
            && metadata.is_dir()
            && !metadata.file_type().is_symlink()
        {
            let _ = fs::remove_dir(&render_job);
        }
        Ok(())
    }

    pub fn cleanup_temporary_render_attempt(&self, job_id: &str, attempt: u32) -> Result<()> {
        validate_asset_id(job_id).context("jobId must be a UUID")?;
        if attempt == 0 {
            bail!("render job attempt must be greater than zero");
        }
        let temporary_jobs = self.temporary.join("render-jobs");
        let temporary_job = temporary_jobs.join(job_id);
        let temporary_attempt = temporary_job.join(format!("attempt-{attempt}"));
        ensure_not_symlink_if_present(&temporary_job)?;
        remove_owned_directory(&temporary_jobs, &temporary_attempt)?;
        if let Ok(metadata) = fs::symlink_metadata(&temporary_job)
            && metadata.is_dir()
            && !metadata.file_type().is_symlink()
        {
            let _ = fs::remove_dir(&temporary_job);
        }
        Ok(())
    }

    pub fn create_staging_asset(&self, id: &str) -> Result<PathBuf> {
        validate_asset_id(id)?;
        let staging = self.assets.join(format!(".import-{id}"));
        fs::create_dir(&staging)
            .with_context(|| format!("could not create staging directory for asset {id}"))?;
        Ok(staging)
    }

    pub(crate) fn create_upload_session(
        &self,
        id: &str,
        expected_bytes: u64,
        file_name: Option<String>,
    ) -> Result<()> {
        validate_asset_id(id)?;
        if expected_bytes == 0 || expected_bytes > MAX_IMPORT_BYTES {
            bail!("upload size must be between 1 byte and 2 GiB");
        }
        ensure_safe_directory(&self.uploads)?;
        let directory = self.uploads.join(id);
        fs::create_dir(&directory)
            .with_context(|| format!("could not create upload session {id}"))?;
        let session = UploadSessionManifest {
            schema_version: 1,
            upload_id: id.to_owned(),
            expected_bytes,
            file_name: file_name
                .map(|name| sanitize_media_file_name(&name))
                .transpose()?,
        };
        let result = (|| -> Result<()> {
            let path = directory.join(UPLOAD_SESSION_FILENAME);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .context("could not create upload session metadata")?;
            file.write_all(&serde_json::to_vec(&session)?)
                .context("could not write upload session metadata")?;
            file.sync_all()
                .context("could not flush upload session metadata")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&directory);
        }
        result
    }

    pub(crate) fn load_upload_session(&self, id: &str) -> Result<UploadSession> {
        validate_asset_id(id)?;
        let uploads_metadata = fs::symlink_metadata(&self.uploads).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!("UPLOAD_NOT_FOUND: upload {id} does not exist")
            } else {
                anyhow!(error).context("could not inspect upload staging directory")
            }
        })?;
        if uploads_metadata.file_type().is_symlink() || !uploads_metadata.is_dir() {
            bail!("upload staging path is not a safe directory");
        }
        let directory = self.uploads.join(id);
        let directory_metadata = fs::symlink_metadata(&directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!("UPLOAD_NOT_FOUND: upload {id} does not exist")
            } else {
                anyhow!(error).context("could not inspect upload session directory")
            }
        })?;
        if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
            bail!("upload session is not a safe directory");
        }
        let directory =
            fs::canonicalize(&directory).context("could not resolve upload session directory")?;
        if !directory.starts_with(&self.uploads) {
            bail!("upload session is outside the workspace staging directory");
        }
        let metadata_path = directory.join(UPLOAD_SESSION_FILENAME);
        ensure_not_symlink(&metadata_path)?;
        if !fs::metadata(&metadata_path)?.is_file() {
            bail!("upload session metadata is not a regular file");
        }
        let manifest: UploadSessionManifest = serde_json::from_slice(
            &fs::read(&metadata_path).context("could not read upload session metadata")?,
        )
        .context("upload session metadata is invalid")?;
        if manifest.schema_version != 1
            || manifest.upload_id != id
            || manifest.expected_bytes == 0
            || manifest.expected_bytes > MAX_IMPORT_BYTES
        {
            bail!("upload session metadata is unsupported or mismatched");
        }
        let source = directory.join(SOURCE_FILENAME);
        let partial = directory.join("source.media.part");
        ensure_not_symlink_if_present(&source)?;
        ensure_not_symlink_if_present(&partial)?;
        let last_activity = fs::metadata(&metadata_path)
            .context("could not inspect upload session activity")?
            .modified()
            .context("upload session activity time is unavailable")?;
        Ok(UploadSession {
            directory,
            source,
            partial,
            expected_bytes: manifest.expected_bytes,
            file_name: manifest.file_name,
            last_activity,
        })
    }

    pub(crate) fn touch_upload_session(&self, id: &str) -> Result<()> {
        let session = self.load_upload_session(id)?;
        let metadata_path = session.directory.join(UPLOAD_SESSION_FILENAME);
        let file = OpenOptions::new()
            .write(true)
            .open(&metadata_path)
            .context("could not open upload session activity marker")?;
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()))
            .context("could not update upload session activity time")
    }

    pub(crate) fn expired_upload_session_ids(&self, now: SystemTime) -> Result<Vec<String>> {
        let uploads_metadata = match fs::symlink_metadata(&self.uploads) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("could not inspect upload staging directory"),
        };
        if uploads_metadata.file_type().is_symlink() || !uploads_metadata.is_dir() {
            bail!("upload staging path is not a safe directory");
        }
        let mut expired = Vec::new();
        for entry in fs::read_dir(&self.uploads).context("could not list upload sessions")? {
            let entry = entry.context("could not read upload session entry")?;
            let file_type = entry
                .file_type()
                .context("could not inspect upload entry")?;
            if file_type.is_symlink() || !file_type.is_dir() {
                bail!("upload staging contains an unsafe entry");
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if self.load_upload_session(&id)?.is_expired(now) {
                expired.push(id);
            }
        }
        Ok(expired)
    }

    pub(crate) fn remove_upload_session_if_expired(
        &self,
        id: &str,
        now: SystemTime,
    ) -> Result<bool> {
        let session = match self.load_upload_session(id) {
            Ok(session) => session,
            Err(error) if format!("{error:#}").contains("UPLOAD_NOT_FOUND") => return Ok(false),
            Err(error) => return Err(error),
        };
        if !session.is_expired(now) {
            return Ok(false);
        }
        self.remove_upload_session(id)?;
        Ok(true)
    }

    pub(crate) fn upload_session_usage(&self) -> Result<(usize, u64)> {
        let uploads_metadata = match fs::symlink_metadata(&self.uploads) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(error) => return Err(error).context("could not inspect upload staging directory"),
        };
        if uploads_metadata.file_type().is_symlink() || !uploads_metadata.is_dir() {
            bail!("upload staging path is not a safe directory");
        }
        let mut sessions = 0_usize;
        let mut reserved_bytes = 0_u64;
        for entry in fs::read_dir(&self.uploads).context("could not list upload sessions")? {
            let entry = entry.context("could not read upload session entry")?;
            let file_type = entry
                .file_type()
                .context("could not inspect upload entry")?;
            if file_type.is_symlink() || !file_type.is_dir() {
                bail!("upload staging contains an unsafe entry");
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let session = self.load_upload_session(&id)?;
            sessions = sessions
                .checked_add(1)
                .context("upload session count overflowed")?;
            reserved_bytes = reserved_bytes
                .checked_add(session.expected_bytes)
                .context("upload reservation size overflowed")?;
        }
        Ok((sessions, reserved_bytes))
    }

    pub(crate) fn publish_upload_session(&self, id: &str, manifest: &AssetManifest) -> Result<()> {
        if manifest.asset_id != id || manifest.schema_version != 1 {
            bail!("upload manifest does not match its session");
        }
        let session = self.load_upload_session(id)?;
        if session.expected_bytes != manifest.bytes {
            bail!("upload manifest size does not match its reservation");
        }
        let source_metadata =
            fs::symlink_metadata(&session.source).context("uploaded media file is missing")?;
        if source_metadata.file_type().is_symlink()
            || !source_metadata.is_file()
            || source_metadata.len() != manifest.bytes
        {
            bail!("uploaded media file does not match its declared size");
        }

        let destination = self.assets.join(id);
        match fs::symlink_metadata(&destination) {
            Ok(_) => bail!("asset {id} already exists"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("could not inspect asset destination"),
        }
        let manifest_path = session.directory.join(ASSET_MANIFEST_FILENAME);
        ensure_not_symlink_if_present(&manifest_path)?;
        fs::write(&manifest_path, serde_json::to_vec_pretty(manifest)?)
            .context("could not write uploaded asset manifest")?;
        fs::rename(&session.directory, &destination)
            .context("could not atomically publish uploaded asset")?;
        // The directory rename is the publication point; this small metadata file is not
        // required for asset reads and can be cleaned up after an interrupted publication.
        let _ = fs::remove_file(destination.join(UPLOAD_SESSION_FILENAME));
        Ok(())
    }

    pub(crate) fn remove_upload_session(&self, id: &str) -> Result<()> {
        validate_asset_id(id)?;
        match fs::symlink_metadata(&self.uploads) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("could not inspect upload staging directory"),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!("upload staging path is not a safe directory");
            }
            Ok(_) => {}
        }
        let directory = self.uploads.join(id);
        remove_owned_directory(&self.uploads, &directory).context("could not remove upload session")
    }

    pub fn publish_staging_asset(&self, id: &str, staging: &Path) -> Result<()> {
        validate_asset_id(id)?;
        let expected_staging = self.assets.join(format!(".import-{id}"));
        if staging != expected_staging {
            bail!("staging directory is outside the workspace asset store");
        }
        let destination = self.assets.join(id);
        fs::rename(staging, &destination).with_context(|| {
            format!(
                "could not publish imported asset into {}",
                destination.display()
            )
        })?;
        Ok(())
    }

    pub fn write_asset_manifest(&self, staging: &Path, manifest: &AssetManifest) -> Result<()> {
        let path = staging.join(ASSET_MANIFEST_FILENAME);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        fs::write(path, bytes).context("could not write asset manifest")
    }

    pub fn load_asset(&self, id: &str) -> Result<(PathBuf, AssetManifest)> {
        validate_asset_id(id)?;
        let directory = self.assets.join(id);
        let directory_metadata = fs::symlink_metadata(&directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!("ASSET_NOT_FOUND: asset {id} is not imported into this workspace")
            } else {
                anyhow!(error).context("could not inspect imported asset directory")
            }
        })?;
        if directory_metadata.file_type().is_symlink() {
            bail!("refusing to follow symlink {}", directory.display());
        }
        if !directory_metadata.is_dir() {
            bail!("asset {id} path is not a directory");
        }
        let directory = fs::canonicalize(&directory)
            .with_context(|| format!("asset {id} is not imported into this workspace"))?;
        if !directory.starts_with(&self.assets) {
            bail!("asset {id} resolves outside the workspace");
        }

        let manifest_path = directory.join(ASSET_MANIFEST_FILENAME);
        ensure_not_symlink(&manifest_path)?;
        if !fs::metadata(&manifest_path)
            .context("could not inspect asset manifest")?
            .is_file()
        {
            bail!("asset manifest is not a regular file");
        }
        let manifest: AssetManifest = serde_json::from_slice(
            &fs::read(&manifest_path).context("could not read asset manifest")?,
        )
        .context("asset manifest is invalid")?;
        if manifest.asset_id != id || manifest.schema_version != 1 {
            bail!("asset {id} has an unsupported or mismatched manifest");
        }

        let source = directory.join(SOURCE_FILENAME);
        ensure_not_symlink(&source)?;
        let source = fs::canonicalize(&source).context("imported asset file is missing")?;
        if !source.starts_with(&directory) || !fs::metadata(&source)?.is_file() {
            bail!("asset {id} source is not a regular file inside its workspace directory");
        }
        Ok((source, manifest))
    }

    pub(crate) fn list_assets(&self) -> Result<Vec<AssetManifest>> {
        let assets_metadata = fs::symlink_metadata(&self.assets)
            .context("could not inspect workspace asset directory")?;
        if assets_metadata.file_type().is_symlink() || !assets_metadata.is_dir() {
            bail!("workspace asset path is not a safe directory");
        }

        let mut assets = Vec::new();
        for entry in fs::read_dir(&self.assets).context("could not list workspace assets")? {
            let entry = entry.context("could not read workspace asset entry")?;
            let file_type = entry
                .file_type()
                .context("could not inspect workspace asset entry")?;
            if file_type.is_symlink() {
                bail!("workspace asset directory contains a symlink");
            }
            if !file_type.is_dir() {
                bail!("workspace asset directory contains an unsafe entry");
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if id.starts_with(".import-") {
                continue;
            }
            let (_, manifest) = self.load_asset(&id)?;
            assets.push(manifest);
        }
        assets.sort_by(|left, right| {
            left.file_name
                .as_deref()
                .unwrap_or(&left.asset_id)
                .to_lowercase()
                .cmp(
                    &right
                        .file_name
                        .as_deref()
                        .unwrap_or(&right.asset_id)
                        .to_lowercase(),
                )
        });
        Ok(assets)
    }
}

pub(crate) fn sanitize_media_file_name(value: &str) -> Result<String> {
    let name = value.trim();
    if name.is_empty()
        || name.len() > 255
        || name.chars().any(char::is_control)
        || name.contains(['/', '\\'])
        || name == "."
        || name == ".."
    {
        bail!("fileName must be a plain filename up to 255 bytes");
    }
    Ok(name.to_owned())
}

pub fn validate_asset_id(id: &str) -> Result<()> {
    let parsed = Uuid::parse_str(id).map_err(|_| anyhow!("invalid asset UUID: {id}"))?;
    if parsed.to_string() != id.to_ascii_lowercase() {
        bail!("asset UUID must use canonical hyphenated form");
    }
    Ok(())
}

fn ensure_safe_directory(path: &Path) -> Result<()> {
    if path.exists() {
        ensure_not_symlink(path)?;
        if !path.is_dir() {
            bail!("{} is not a directory", path.display());
        }
    } else {
        fs::create_dir_all(path)
            .with_context(|| format!("could not create directory {}", path.display()))?;
    }
    Ok(())
}

fn ensure_not_symlink(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("refusing to follow symlink {}", path.display());
    }
    Ok(())
}

fn ensure_not_symlink_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to follow symlink {}", path.display());
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not inspect {}", path.display())),
    }
}

fn remove_owned_directory(parent: &Path, path: &Path) -> Result<()> {
    ensure_not_symlink(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to remove symlink {}", path.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect {}", path.display()));
        }
    }
    let parent = fs::canonicalize(parent).context("could not resolve render output parent")?;
    let path_canonical = fs::canonicalize(path).context("could not resolve render attempt")?;
    if !path_canonical.starts_with(parent) || !path_canonical.is_dir() {
        bail!("refusing to remove render output outside its job directory");
    }
    fs::remove_dir_all(path_canonical).context("could not clean render job attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::media::MediaProbe;
    use tempfile::tempdir;

    #[cfg(unix)]
    const ASSET_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0b";

    #[test]
    fn asset_ids_cannot_escape_the_workspace() {
        assert!(validate_asset_id("../outside").is_err());
        assert!(validate_asset_id("018f2e40-4d28-7e31-8b63-611d49c43b0b").is_ok());
    }

    #[test]
    fn media_file_names_are_plain_basenames_with_a_bounded_length() {
        assert_eq!(
            sanitize_media_file_name("  clip one.mp4  ").unwrap(),
            "clip one.mp4"
        );
        assert!(sanitize_media_file_name("").is_err());
        assert!(sanitize_media_file_name("../clip.mp4").is_err());
        assert!(sanitize_media_file_name("folder\\clip.mp4").is_err());
        assert!(sanitize_media_file_name(&"x".repeat(256)).is_err());
        assert!(sanitize_media_file_name("clip\n.mp4").is_err());
    }

    #[test]
    fn upload_session_expiry_removes_stale_sessions_but_keeps_active_ones() {
        const ACTIVE_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0b";
        const STALE_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0c";

        let root = tempdir().unwrap();
        let workspace = Workspace::open(&root.path().join("workspace")).unwrap();
        workspace
            .create_upload_session(ACTIVE_ID, 20, None)
            .unwrap();
        workspace.create_upload_session(STALE_ID, 10, None).unwrap();

        let now = SystemTime::now();
        let stale_session = workspace.load_upload_session(STALE_ID).unwrap();
        let old_activity = now - UPLOAD_SESSION_TTL - Duration::from_secs(1);
        OpenOptions::new()
            .write(true)
            .open(stale_session.directory.join(UPLOAD_SESSION_FILENAME))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_activity))
            .unwrap();
        workspace.touch_upload_session(ACTIVE_ID).unwrap();

        assert_eq!(
            workspace.expired_upload_session_ids(now).unwrap(),
            vec![STALE_ID.to_owned()]
        );
        assert!(
            workspace
                .remove_upload_session_if_expired(STALE_ID, now)
                .unwrap()
        );
        assert!(
            !workspace
                .remove_upload_session_if_expired(ACTIVE_ID, now)
                .unwrap()
        );
        assert_eq!(workspace.upload_session_usage().unwrap(), (1, 20));
        assert!(workspace.load_upload_session(ACTIVE_ID).is_ok());
        assert!(workspace.load_upload_session(STALE_ID).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn render_job_cleanup_rejects_a_symlinked_job_directory() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let workspace = Workspace::open(&root.path().join("workspace")).unwrap();
        let jobs = workspace.temporary.join("render-jobs");
        fs::create_dir_all(jobs.join("other-job").join("attempt-1")).unwrap();
        fs::write(
            jobs.join("other-job").join("attempt-1").join("partial.mp4"),
            b"do not delete",
        )
        .unwrap();
        symlink(jobs.join("other-job"), jobs.join(ASSET_ID)).unwrap();

        let error = workspace.cleanup_render_attempt(ASSET_ID, 1).unwrap_err();
        assert!(format!("{error:#}").contains("refusing to follow symlink"));
        assert!(
            jobs.join("other-job")
                .join("attempt-1")
                .join("partial.mp4")
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn render_asset_resolution_rejects_a_symlinked_output() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let workspace = Workspace::open(&root.path().join("workspace")).unwrap();
        let attempt = workspace.renders.join(ASSET_ID).join("attempt-1");
        fs::create_dir_all(&attempt).unwrap();
        let outside = root.path().join("outside.mp4");
        fs::write(&outside, b"not in the render store").unwrap();
        symlink(&outside, attempt.join("output.mp4")).unwrap();

        let error = workspace.resolve_render_output(ASSET_ID, 1).unwrap_err();
        assert!(format!("{error:#}").contains("refusing to follow symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn load_asset_rejects_a_symlinked_asset_directory() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let workspace = Workspace::open(&root.path().join("workspace")).unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, workspace.assets.join(ASSET_ID)).unwrap();

        let error = workspace.load_asset(ASSET_ID).unwrap_err();
        assert!(format!("{error:#}").contains("refusing to follow symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn load_asset_rejects_a_symlinked_source_file() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let workspace = Workspace::open(&root.path().join("workspace")).unwrap();
        let directory = workspace.assets.join(ASSET_ID);
        fs::create_dir(&directory).unwrap();
        let outside = root.path().join("outside.wav");
        fs::write(&outside, b"not used by this manifest-only path test").unwrap();
        let manifest = AssetManifest {
            schema_version: 1,
            asset_id: ASSET_ID.to_owned(),
            sha256: "0".repeat(64),
            bytes: 1,
            kind: AssetKind::Audio,
            file_name: None,
            probe: MediaProbe {
                format_name: "wav".to_owned(),
                duration: "1.0".to_owned(),
                duration_seconds: 1.0,
                has_video: false,
                has_audio: true,
                video: None,
                audio: None,
            },
        };
        fs::write(
            directory.join(ASSET_MANIFEST_FILENAME),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        symlink(&outside, directory.join(SOURCE_FILENAME)).unwrap();

        let error = workspace.load_asset(ASSET_ID).unwrap_err();
        assert!(format!("{error:#}").contains("refusing to follow symlink"));
    }
}
