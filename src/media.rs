use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    process::run_checked_cancellable,
    workspace::{AssetManifest, SOURCE_FILENAME, Workspace},
};

pub const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_PROJECT_SECONDS: f64 = 5.0 * 60.0;
const MAX_SOURCE_WIDTH: u32 = 4096;
const MAX_SOURCE_HEIGHT: u32 = 2160;
const MAX_SOURCE_PIXELS: u64 = 8_847_360;
const MAX_AUDIO_CHANNELS: u32 = 8;
const MAX_AUDIO_SAMPLE_RATE: u32 = 192_000;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamInfo {
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaProbe {
    pub format_name: String,
    /// Exact decimal reported by ffprobe, retained for source-bound checks.
    pub duration: String,
    pub duration_seconds: f64,
    pub has_video: bool,
    pub has_audio: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<StreamInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<StreamInfo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedAsset {
    pub asset_id: String,
    pub uri: String,
    pub sha256: String,
    pub kind: AssetKind,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    pub probe: MediaProbe,
}

pub struct ImportOptions {
    pub workspace: PathBuf,
    pub file: PathBuf,
    pub ffprobe: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: ProbeFormat,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    format_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: String,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    duration: Option<String>,
    avg_frame_rate: Option<String>,
}

pub fn import_asset(options: ImportOptions) -> Result<ImportedAsset> {
    let source = fs::canonicalize(&options.file)
        .with_context(|| format!("could not resolve input file {}", options.file.display()))?;
    let source_metadata = fs::metadata(&source).context("could not inspect input file")?;
    if !source_metadata.is_file() {
        bail!("input must be a regular local file");
    }
    if source_metadata.len() == 0 || source_metadata.len() > MAX_IMPORT_BYTES {
        bail!("input must be between 1 byte and 2 GiB");
    }

    let workspace = Workspace::open(&options.workspace)?;
    let asset_id = Uuid::new_v4().to_string();
    let staging = workspace.create_staging_asset(&asset_id)?;
    let mut cleanup = StagingCleanup(Some(staging.clone()));
    let staged_source = staging.join(SOURCE_FILENAME);
    let (sha256, bytes) = copy_and_hash(&source, &staged_source)?;
    let probe = probe_media(&options.ffprobe, &staged_source)?;
    validate_probe(&probe)?;
    let kind = if probe.has_video {
        AssetKind::Video
    } else {
        AssetKind::Audio
    };

    let manifest = AssetManifest {
        schema_version: 1,
        asset_id: asset_id.clone(),
        sha256: sha256.clone(),
        bytes,
        kind,
        file_name: source
            .file_name()
            .and_then(|value| value.to_str())
            .map(str::to_owned),
        probe: probe.clone(),
    };
    workspace.write_asset_manifest(&staging, &manifest)?;
    workspace.publish_staging_asset(&asset_id, &staging)?;
    cleanup.0 = None;

    Ok(ImportedAsset {
        uri: format!("asset://{asset_id}"),
        asset_id,
        sha256,
        kind,
        bytes,
        file_name: manifest.file_name,
        probe,
    })
}

pub(crate) fn complete_uploaded_asset(
    workspace_path: &Path,
    upload_id: &str,
    ffprobe: &Path,
) -> Result<ImportedAsset> {
    let workspace = Workspace::open(workspace_path)?;
    let session = workspace.load_upload_session(upload_id)?;
    let (sha256, bytes) = hash_file(&session.source)?;
    if bytes != session.expected_bytes {
        bail!("uploaded media size does not match the upload reservation");
    }
    let probe = probe_media(ffprobe, &session.source)?;
    validate_probe(&probe)?;
    let kind = if probe.has_video {
        AssetKind::Video
    } else {
        AssetKind::Audio
    };
    let manifest = AssetManifest {
        schema_version: 1,
        asset_id: upload_id.to_owned(),
        sha256: sha256.clone(),
        bytes,
        kind,
        file_name: session.file_name.clone(),
        probe: probe.clone(),
    };
    workspace.publish_upload_session(upload_id, &manifest)?;
    Ok(ImportedAsset {
        asset_id: upload_id.to_owned(),
        uri: format!("asset://{upload_id}"),
        sha256,
        kind,
        bytes,
        file_name: session.file_name,
        probe,
    })
}

pub fn probe_media(ffprobe: &Path, file: &Path) -> Result<MediaProbe> {
    probe_media_cancellable(ffprobe, file, None)
}

pub(crate) fn probe_media_cancellable(
    ffprobe: &Path,
    file: &Path,
    cancellation: Option<&AtomicBool>,
) -> Result<MediaProbe> {
    let output = run_checked_cancellable(
        Command::new(ffprobe)
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration,format_name:stream=codec_type,codec_name,width,height,sample_rate,channels,duration,avg_frame_rate")
            .arg("-of")
            .arg("json")
            .arg("-protocol_whitelist")
            .arg("file,pipe")
            .arg(file),
        PROBE_TIMEOUT,
        "ffprobe",
        cancellation,
    )?;
    let parsed: ProbeOutput =
        serde_json::from_slice(&output.stdout).context("ffprobe returned malformed metadata")?;

    let video_stream = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type == "video")
        .map(to_stream_info)
        .transpose()?;
    let audio_stream = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type == "audio")
        .map(to_stream_info)
        .transpose()?;

    let duration = parsed
        .format
        .duration
        .or_else(|| {
            parsed
                .streams
                .iter()
                .filter_map(|stream| stream.duration.as_deref())
                .filter_map(|value| value.parse::<f64>().ok())
                .max_by(f64::total_cmp)
                .map(|value| value.to_string())
        })
        .ok_or_else(|| anyhow::anyhow!("media duration is unavailable"))?;
    let duration_seconds = duration
        .parse::<f64>()
        .context("ffprobe returned an invalid duration")?;
    let format_name = parsed
        .format
        .format_name
        .ok_or_else(|| anyhow::anyhow!("ffprobe did not identify the media container"))?;

    Ok(MediaProbe {
        format_name,
        duration,
        duration_seconds,
        has_video: video_stream.is_some(),
        has_audio: audio_stream.is_some(),
        video: video_stream,
        audio: audio_stream,
    })
}

fn to_stream_info(stream: &ProbeStream) -> Result<StreamInfo> {
    let codec = stream
        .codec_name
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let sample_rate = stream
        .sample_rate
        .as_deref()
        .map(|value| value.parse::<u32>().context("invalid audio sample rate"))
        .transpose()?;
    Ok(StreamInfo {
        codec,
        duration_seconds: stream
            .duration
            .as_deref()
            .and_then(|value| value.parse::<f64>().ok()),
        frame_rate: stream.avg_frame_rate.clone(),
        width: stream.width,
        height: stream.height,
        sample_rate,
        channels: stream.channels,
    })
}

fn validate_probe(probe: &MediaProbe) -> Result<()> {
    if !probe.has_video && !probe.has_audio {
        bail!("file has no supported audio or video stream");
    }
    if !probe.duration_seconds.is_finite()
        || probe.duration_seconds <= 0.0
        || probe.duration_seconds > MAX_PROJECT_SECONDS
    {
        bail!("media duration must be greater than zero and no more than five minutes");
    }
    let supported_format = probe.format_name.split(',').any(|format| {
        if probe.has_video {
            matches!(format, "mov" | "mp4" | "m4v" | "3gp" | "3g2" | "mj2")
        } else {
            matches!(format, "mp3" | "wav" | "aac" | "mov" | "mp4" | "m4a")
        }
    });
    if !supported_format {
        bail!("unsupported media container; use MP4/MOV video or MP3/WAV/AAC audio");
    }
    if let Some(video) = &probe.video {
        let (Some(width), Some(height)) = (video.width, video.height) else {
            bail!("video stream is missing its dimensions");
        };
        if width == 0
            || height == 0
            || width > MAX_SOURCE_WIDTH
            || height > MAX_SOURCE_HEIGHT
            || u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS
        {
            bail!("source dimensions exceed the 4K safety limit");
        }
    }
    if let Some(audio) = &probe.audio
        && (audio
            .sample_rate
            .is_some_and(|rate| rate > MAX_AUDIO_SAMPLE_RATE)
            || audio
                .channels
                .is_some_and(|channels| channels > MAX_AUDIO_CHANNELS))
    {
        bail!("audio stream exceeds the supported sample-rate or channel limit");
    }
    Ok(())
}

fn copy_and_hash(source: &Path, destination: &Path) -> Result<(String, u64)> {
    let mut input = File::open(source).context("could not open input file")?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("could not create staged asset")?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .context("could not read input media")?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| anyhow::anyhow!("input file size overflow"))?;
        if bytes > MAX_IMPORT_BYTES {
            bail!("input file exceeds the 2 GiB import limit");
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .context("could not stage input media")?;
    }
    output.sync_all().context("could not flush staged asset")?;
    Ok((hex::encode(hasher.finalize()), bytes))
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut input = File::open(path).context("could not open uploaded media")?;
    let metadata = input
        .metadata()
        .context("could not inspect uploaded media")?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_IMPORT_BYTES {
        bail!("uploaded media must be between 1 byte and 2 GiB");
    }
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .context("could not read uploaded media")?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(read).context("uploaded media size is out of range")?)
            .context("uploaded media size overflowed")?;
        if bytes > MAX_IMPORT_BYTES {
            bail!("uploaded media exceeds the 2 GiB import limit");
        }
        hasher.update(&buffer[..read]);
    }
    if bytes != metadata.len() {
        bail!("uploaded media changed while it was being hashed");
    }
    Ok((hex::encode(hasher.finalize()), bytes))
}

struct StagingCleanup(Option<PathBuf>);

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_or_overlong_media() {
        let mut probe = MediaProbe {
            format_name: "mov,mp4,m4a,3gp,3g2,mj2".to_owned(),
            duration: "301".to_owned(),
            duration_seconds: 301.0,
            has_video: true,
            has_audio: false,
            video: Some(StreamInfo {
                codec: "h264".to_owned(),
                duration_seconds: Some(10.0),
                frame_rate: Some("30/1".to_owned()),
                width: Some(1280),
                height: Some(720),
                sample_rate: None,
                channels: None,
            }),
            audio: None,
        };
        assert!(validate_probe(&probe).is_err());
        probe.duration = "10".to_owned();
        probe.duration_seconds = 10.0;
        assert!(validate_probe(&probe).is_ok());
    }
}
