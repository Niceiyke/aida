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
}

impl Workspace {
    pub fn open(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)
            .with_context(|| format!("could not create workspace {}", path.display()))?;
        let root = fs::canonicalize(path)
            .with_context(|| format!("could not resolve workspace {}", path.display()))?;
        let assets = root.join("assets");
        let temporary = root.join("tmp");
        ensure_safe_directory(&assets)?;
        ensure_safe_directory(&temporary)?;
        Ok(Self {
            root,
            assets,
            temporary,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn temporary_dir(&self) -> &Path {
        &self.temporary
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
        ensure_not_symlink(&directory)?;
        let directory = fs::canonicalize(&directory)
            .with_context(|| format!("asset {id} is not imported into this workspace"))?;
        if !directory.starts_with(&self.assets) {
            bail!("asset {id} resolves outside the workspace");
        }

        let manifest_path = directory.join(ASSET_MANIFEST_FILENAME);
        ensure_not_symlink(&manifest_path)?;
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
        if !source.starts_with(&directory) {
            bail!("asset {id} resolves outside its workspace directory");
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
