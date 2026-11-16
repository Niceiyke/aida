use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    assemble::video_duration_frames,
    process::run_bounded_cancellable,
    project::{RenderJob, RenderJobStatus},
    render::RenderPreset,
    timeline::{FrameRate, Timeline, TrackKind},
};

const MAX_STILLS: usize = 8;
const STILL_TIMEOUT: Duration = Duration::from_secs(20);
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub job_id: String,
    pub status: RenderJobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_asset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_url: Option<String>,
    pub qc: QualityReport,
    pub stills: Vec<StillFrame>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityReport {
    pub passed: bool,
    pub findings: Vec<QcFinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_frame_count: Option<u64>,
    pub audio_present: bool,
    pub audio_expected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QcFinding {
    pub code: String,
    pub severity: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StillFrame {
    pub frame: u64,
    pub seconds: f64,
    pub url: String,
}

pub fn inspect_job(
    job: &RenderJob,
    timeline: Option<&Timeline>,
    output: Option<&Path>,
    ffmpeg: &Path,
) -> Result<Inspection> {
    let mut findings = Vec::new();
    if job.status != RenderJobStatus::Succeeded {
        findings.push(finding(
            "JOB_NOT_SUCCEEDED",
            "error",
            format!("render status is {}", job_status_name(job.status)),
        ));
    }
    let result = job.result.as_ref();
    let expected_frames = timeline.map(video_duration_frames);
    let expected_duration =
        timeline.map(|timeline| frame_seconds(expected_frames.unwrap_or(0), timeline.canvas.fps));
    let audio_expected = timeline.is_some_and(timeline_has_audio);
    if let (Some(result), Some(expected_frames), Some(expected_duration)) =
        (result, expected_frames, expected_duration)
    {
        let frame_tolerance = frame_seconds(1, timeline_fps(timeline));
        if (result.output_duration_seconds - expected_duration).abs() > frame_tolerance + 0.001 {
            findings.push(finding(
                "DURATION_MISMATCH",
                "error",
                format!(
                    "output duration {:.3}s differs from timeline duration {:.3}s by more than one frame",
                    result.output_duration_seconds, expected_duration
                ),
            ));
        }
        if result.output_frame_count != expected_frames {
            findings.push(finding(
                "FRAME_COUNT_MISMATCH",
                "error",
                format!(
                    "output has {} frames; timeline has {expected_frames}",
                    result.output_frame_count
                ),
            ));
        }
        if result.video_codec != "h264" {
            findings.push(finding(
                "CODEC_MISMATCH",
                "error",
                format!(
                    "output video codec is {}, expected h264",
                    result.video_codec
                ),
            ));
        }
        if audio_expected && result.audio_codec.is_none() {
            findings.push(finding(
                "AUDIO_MISSING",
                "error",
                "timeline has audio but the output has no audio stream",
            ));
        }
        if let Some(timeline) = timeline {
            let (expected_width, expected_height) = expected_dimensions(
                job.preset_id.as_str(),
                timeline.canvas.width,
                timeline.canvas.height,
            )?;
            if result.output_width != expected_width || result.output_height != expected_height {
                findings.push(finding(
                    "DIMENSION_MISMATCH",
                    "error",
                    format!(
                        "output is {}x{}; expected {expected_width}x{expected_height}",
                        result.output_width, result.output_height
                    ),
                ));
            }
        }
    } else if job.status == RenderJobStatus::Succeeded {
        findings.push(finding(
            "QC_INCOMPLETE",
            "warning",
            "the successful job is missing a timeline or probe result",
        ));
    }

    let mut stills = Vec::new();
    if job.status == RenderJobStatus::Succeeded {
        if let Some(output) = output {
            if let Err(error) = analyze_picture(output, ffmpeg, &mut findings) {
                findings.push(finding(
                    "INSPECTION_PARTIAL",
                    "warning",
                    format!("black-frame and silence scan was skipped: {error:#}"),
                ));
            }
            match extract_stills(job, timeline, output, ffmpeg) {
                Ok(extracted) => stills = extracted,
                Err(error) => findings.push(finding(
                    "STILLS_UNAVAILABLE",
                    "warning",
                    format!("could not extract still frames: {error:#}"),
                )),
            }
        } else {
            findings.push(finding(
                "OUTPUT_UNAVAILABLE",
                "warning",
                "the published MP4 is not available for stills or signal checks",
            ));
        }
    }

    let passed = job.status == RenderJobStatus::Succeeded
        && timeline.is_some()
        && result.is_some()
        && output.is_some_and(Path::is_file)
        && !findings.iter().any(|item| item.severity == "error");
    Ok(Inspection {
        job_id: job.job_id.clone(),
        status: job.status,
        output_asset_id: job.output_asset_id.clone(),
        content_url: job
            .output_asset_id
            .as_ref()
            .map(|asset_id| format!("/v1/assets/{asset_id}/content")),
        qc: QualityReport {
            passed,
            findings,
            duration_seconds: result.map(|result| result.output_duration_seconds),
            expected_duration_seconds: expected_duration,
            frame_count: result.map(|result| result.output_frame_count),
            expected_frame_count: expected_frames,
            audio_present: result.is_some_and(|result| result.audio_codec.is_some()),
            audio_expected,
            video_codec: result.map(|result| result.video_codec.clone()),
            width: result.map(|result| result.output_width),
            height: result.map(|result| result.output_height),
        },
        stills,
    })
}

pub fn still_path(output: &Path, frame: u64) -> Result<PathBuf> {
    let attempt_dir = output
        .parent()
        .context("render output has no parent directory")?;
    let canonical_attempt =
        fs::canonicalize(attempt_dir).context("could not resolve render attempt directory")?;
    let stills = attempt_dir.join("stills");
    let path = stills.join(format!("frame-{frame}.jpg"));
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| name.contains(".."))
    {
        bail!("still frame path is invalid");
    }
    let canonical_stills = match fs::symlink_metadata(&stills) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("still frames directory is not a regular workspace directory");
            }
            let canonical =
                fs::canonicalize(&stills).context("could not resolve stills directory")?;
            if !canonical.starts_with(&canonical_attempt) {
                bail!("still frames directory is outside the render attempt");
            }
            Some(canonical)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("could not inspect stills directory"),
    };
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("still frame is not a regular file");
        }
        let canonical = fs::canonicalize(&path).context("could not resolve still frame")?;
        if canonical_stills
            .as_ref()
            .is_none_or(|stills| !canonical.starts_with(stills))
        {
            bail!("still frame is outside the stills directory");
        }
    }
    Ok(path)
}

pub fn ensure_still(
    job: &RenderJob,
    timeline: Option<&Timeline>,
    output: &Path,
    ffmpeg: &Path,
    frame: u64,
) -> Result<PathBuf> {
    if job.status != RenderJobStatus::Succeeded {
        bail!("still frames are available only after a successful render");
    }
    let total = timeline
        .map(video_duration_frames)
        .or_else(|| job.result.as_ref().map(|result| result.output_frame_count))
        .unwrap_or(0);
    if frame >= total {
        bail!("requested frame is outside the rendered output");
    }
    let path = still_path(output, frame)?;
    if !path.is_file() {
        ensure_stills_directory(output)?;
        let fps = timeline
            .map(|timeline| timeline.canvas.fps)
            .unwrap_or(FrameRate { num: 30, den: 1 });
        extract_jpeg(ffmpeg, output, frame_seconds(frame, fps), &path)?;
    }
    Ok(path)
}

fn extract_stills(
    job: &RenderJob,
    timeline: Option<&Timeline>,
    output: &Path,
    ffmpeg: &Path,
) -> Result<Vec<StillFrame>> {
    let fps = timeline
        .map(|timeline| timeline.canvas.fps)
        .unwrap_or(FrameRate { num: 30, den: 1 });
    let total = timeline
        .map(video_duration_frames)
        .or_else(|| job.result.as_ref().map(|result| result.output_frame_count))
        .unwrap_or(1)
        .max(1);
    let mut frames = vec![0];
    if let Some(timeline) = timeline {
        let mut cursor = 0_u64;
        for track in &timeline.tracks {
            if track.kind != TrackKind::Video {
                continue;
            }
            for clip in &track.clips {
                if cursor > 0 {
                    frames.push(cursor);
                }
                cursor = cursor.saturating_add(clip.duration_frames());
            }
        }
    }
    frames.push(total / 2);
    if total > 1 {
        frames.push(total - 1);
    }
    frames.sort_unstable();
    frames.dedup();
    frames.truncate(MAX_STILLS);

    ensure_stills_directory(output)?;
    let mut stills = Vec::new();
    for frame in frames {
        let path = still_path(output, frame)?;
        if !path.is_file() {
            let seconds = frame_seconds(frame, fps);
            extract_jpeg(ffmpeg, output, seconds, &path)?;
        }
        stills.push(StillFrame {
            frame,
            seconds: frame_seconds(frame, fps),
            url: format!("/v1/jobs/{}/stills/{frame}", job.job_id),
        });
    }
    Ok(stills)
}

fn ensure_stills_directory(output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .context("render output has no parent directory")?;
    let stills = parent.join("stills");
    if let Ok(metadata) = fs::symlink_metadata(&stills)
        && metadata.file_type().is_symlink()
    {
        bail!("still frames directory must not be a symlink");
    }
    fs::create_dir_all(&stills).context("could not create stills directory")?;
    let canonical_parent =
        fs::canonicalize(parent).context("could not resolve render directory")?;
    let canonical_stills =
        fs::canonicalize(&stills).context("could not resolve stills directory")?;
    if !canonical_stills.starts_with(&canonical_parent) || !canonical_stills.is_dir() {
        bail!("still frames directory is outside the render directory");
    }
    Ok(())
}

fn extract_jpeg(ffmpeg: &Path, output: &Path, seconds: f64, destination: &Path) -> Result<()> {
    let seconds = format!("{seconds:.3}");
    let input = output.to_string_lossy();
    let destination_path = destination.to_path_buf();
    let destination = destination_path.to_string_lossy();
    let mut command = Command::new(ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-ss")
        .arg(&seconds)
        .arg("-i")
        .arg(input.as_ref())
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("3")
        .arg("-y")
        .arg(destination.as_ref());
    run_bounded_cancellable(&mut command, STILL_TIMEOUT, None)
        .with_context(|| format!("could not extract still at {seconds}s"))?;
    if !destination_path.is_file() {
        bail!("ffmpeg did not write a still frame");
    }
    Ok(())
}

fn analyze_picture(output: &Path, ffmpeg: &Path, findings: &mut Vec<QcFinding>) -> Result<()> {
    let captured = run_bounded_cancellable(
        Command::new(ffmpeg).args([
            "-hide_banner",
            "-i",
            &output.to_string_lossy(),
            "-vf",
            "blackdetect=d=0.2:pix_th=0.10",
            "-af",
            "silencedetect=noise=-50dB:d=0.3",
            "-f",
            "null",
            "-",
        ]),
        ANALYSIS_TIMEOUT,
        None,
    );
    let captured = match captured {
        Ok(captured) => captured,
        Err(error) => {
            let retry = run_bounded_cancellable(
                Command::new(ffmpeg).args([
                    "-hide_banner",
                    "-i",
                    &output.to_string_lossy(),
                    "-vf",
                    "blackdetect=d=0.2:pix_th=0.10",
                    "-f",
                    "null",
                    "-",
                ]),
                ANALYSIS_TIMEOUT,
                None,
            );
            match retry {
                Ok(captured) => captured,
                Err(_) => return Err(error),
            }
        }
    };
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let mut black = 0.0_f64;
    let mut silence = 0.0_f64;
    for line in stderr.lines() {
        if let Some(value) = field_after(line, "black_duration:") {
            black += value;
        } else if let Some(value) = field_after(line, "silence_duration:") {
            silence += value;
        }
    }
    if black >= 0.5 {
        findings.push(finding(
            "BLACK_FRAMES",
            "warning",
            format!("blackdetect found {black:.2}s of near-black video"),
        ));
    }
    if silence >= 0.5 {
        findings.push(finding(
            "SILENCE",
            "warning",
            format!("silencedetect found {silence:.2}s below -50 dB"),
        ));
    }
    Ok(())
}

fn field_after(line: &str, key: &str) -> Option<f64> {
    let rest = line.split(key).nth(1)?;
    let token = rest
        .trim()
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .next()?;
    token.parse().ok()
}

fn expected_dimensions(preset_id: &str, width: u32, height: u32) -> Result<(u32, u32)> {
    Ok(RenderPreset::parse(preset_id)?.output_dimensions(width, height))
}

fn timeline_has_audio(timeline: &Timeline) -> bool {
    timeline
        .tracks
        .iter()
        .any(|track| track.kind == TrackKind::Audio && track.enabled && !track.clips.is_empty())
}

fn timeline_fps(timeline: Option<&Timeline>) -> FrameRate {
    timeline
        .map(|timeline| timeline.canvas.fps)
        .unwrap_or(FrameRate { num: 30, den: 1 })
}

fn frame_seconds(frame: u64, fps: FrameRate) -> f64 {
    if fps.num == 0 {
        return 0.0;
    }
    frame as f64 * f64::from(fps.den) / f64::from(fps.num)
}

fn job_status_name(status: RenderJobStatus) -> &'static str {
    match status {
        RenderJobStatus::Queued => "queued",
        RenderJobStatus::Running => "running",
        RenderJobStatus::Succeeded => "succeeded",
        RenderJobStatus::Failed => "failed",
        RenderJobStatus::Cancelled => "cancelled",
    }
}

fn finding(code: &str, severity: &'static str, message: impl Into<String>) -> QcFinding {
    QcFinding {
        code: code.to_owned(),
        severity,
        message: message.into(),
    }
}
