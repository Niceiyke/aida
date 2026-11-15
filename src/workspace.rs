use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::media::{AssetKind, MediaProbe};

pub const SOURCE_FILENAME: &str = "source.media";
const ASSET_MANIFEST_FILENAME: &str = "asset.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssetManifest {
    pub schema_version: u32,
    pub asset_id: String,
    pub sha256: String,
    pub bytes: u64,
    pub kind: AssetKind,
    pub probe: MediaProbe,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    assets: PathBuf,
    temporary: PathBuf,
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
        let renders = root.join("renders");
        ensure_safe_directory(&assets)?;
        ensure_safe_directory(&temporary)?;
        ensure_safe_directory(&renders)?;
        Ok(Self {
            root,
            assets,
            temporary,
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
    if !path.exists() {
        return Ok(());
    }
    ensure_not_symlink(parent)?;
    ensure_not_symlink(path)?;
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
    #[cfg(unix)]
    use tempfile::tempdir;

    #[cfg(unix)]
    const ASSET_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0b";

    #[test]
    fn asset_ids_cannot_escape_the_workspace() {
        assert!(validate_asset_id("../outside").is_err());
        assert!(validate_asset_id("018f2e40-4d28-7e31-8b63-611d49c43b0b").is_ok());
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
