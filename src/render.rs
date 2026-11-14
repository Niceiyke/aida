use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    media::{MediaProbe, probe_media},
    process::run_checked,
    timeline::{
        Clip, FrameRate, ResolvedAsset, ValidatedTimeline, VideoClip, frame_boundary_sample,
        load_and_validate,
    },
    workspace::Workspace,
};

const MAX_FONT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub struct RenderOptions {
    pub workspace: PathBuf,
    pub timeline: PathBuf,
    pub output: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub font: Option<PathBuf>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderResult {
    pub output: PathBuf,
    pub provenance: PathBuf,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Provenance<'a> {
    schema_version: u32,
    renderer_version: &'static str,
    project_id: &'a str,
    revision: u64,
    timeline_sha256: &'a str,
    assets: Vec<ProvenanceAsset<'a>>,
    font_sha256: Option<&'a str>,
    ffmpeg_version: &'a str,
    ffmpeg_fingerprint: &'a str,
    ffprobe_version: &'a str,
    ffprobe_fingerprint: &'a str,
    width: u32,
    height: u32,
    fps: String,
    frame_count: u64,
    video_encoder: &'static str,
    audio_encoder: Option<&'static str>,
    output_sha256: &'a str,
    output_preset: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProvenanceAsset<'a> {
    asset_id: &'a str,
    sha256: &'a str,
}

pub fn render_timeline(options: RenderOptions) -> Result<RenderResult> {
    if !(1..=3600).contains(&options.timeout_seconds) {
        bail!("timeout-seconds must be between 1 and 3600");
    }
    let timeout = Duration::from_secs(options.timeout_seconds);
    let workspace = Workspace::open(&options.workspace)?;
    let validated = load_and_validate(&options.timeline, &workspace)?;
    let timeline_bytes = fs::read(&options.timeline).context("could not read timeline JSON")?;
    let timeline_hash = sha256(&timeline_bytes);

    let (output_path, output_temp, provenance_path, provenance_temp) =
        prepare_output_paths(&options.output)?;
    let work_dir = workspace
        .temporary_dir()
        .join(format!("render-{}", Uuid::new_v4()));
    fs::create_dir(&work_dir).context("could not create render working directory")?;
    let mut cleanup = RenderCleanup {
        work_dir: Some(work_dir.clone()),
        output_temp: Some(output_temp.clone()),
        provenance_temp: Some(provenance_temp.clone()),
        published_output: None,
    };

    let font = stage_font(
        options.font.as_deref(),
        &work_dir,
        !validated.text_clips.is_empty(),
    )?;
    let plan = build_render_plan(&validated)?;
    let filter_graph = compile_filter_graph(&plan, &work_dir, font.as_ref())?;
    let command_budget = filter_graph.len()
        + plan
            .inputs
            .iter()
            .map(|path| path.as_os_str().to_string_lossy().len() + 24)
            .sum::<usize>()
        + 4096;
    if command_budget > 28 * 1024 {
        bail!("generated FFmpeg command exceeds the supported 28 KiB command limit");
    }

    let ffmpeg = tool_identity(&options.ffmpeg, timeout)?;
    let ffprobe = tool_identity(&options.ffprobe, timeout)?;
    run_ffmpeg(FfmpegInvocation {
        executable: &options.ffmpeg,
        inputs: &plan.inputs,
        filter_graph: &filter_graph,
        work_dir: &work_dir,
        output: &output_temp,
        frames: plan.total_frames,
        has_audio: !plan.audio_clips.is_empty(),
        timeout,
    })?;

    let output_metadata = fs::metadata(&output_temp).context("FFmpeg did not create an output")?;
    if output_metadata.len() == 0 || output_metadata.len() > MAX_OUTPUT_BYTES {
        bail!("render output is empty or exceeds the 2 GiB output limit");
    }
    let output_probe = probe_media(&options.ffprobe, &output_temp)?;
    let output_frame_count = count_video_frames(&options.ffprobe, &output_temp, timeout)?;
    verify_output(&validated, &output_probe, output_frame_count)?;
    let output_hash = hash_file(&output_temp)?;
    let font_hash = font.as_ref().map(|font| font.sha256.as_str());
    let provenance = build_provenance(
        &validated,
        &timeline_hash,
        font_hash,
        &ffmpeg,
        &ffprobe,
        &output_hash,
    );

    let serialized = serde_json::to_vec_pretty(&provenance)?;
    let mut provenance_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&provenance_temp)
        .context("could not create provenance sidecar")?;
    provenance_file
        .write_all(&serialized)
        .context("could not write provenance sidecar")?;
    provenance_file
        .sync_all()
        .context("could not flush provenance sidecar")?;

    // Both files are staged beside their destinations so publication stays on one volume.
    ensure_output_absent(&output_path, &provenance_path)?;
    fs::rename(&output_temp, &output_path)
        .with_context(|| format!("could not publish output to {}", output_path.display()))?;
    cleanup.output_temp = None;
    cleanup.published_output = Some(output_path.clone());
    fs::rename(&provenance_temp, &provenance_path).with_context(|| {
        format!(
            "could not publish provenance to {}",
            provenance_path.display()
        )
    })?;
    cleanup.provenance_temp = None;
    cleanup.published_output = None;
    cleanup.work_dir = None;
    let _ = fs::remove_dir_all(&work_dir);

    Ok(RenderResult {
        output: output_path,
        provenance: provenance_path,
        timeline_sha256: timeline_hash,
        output_sha256: output_hash,
        output_duration_seconds: output_probe.duration_seconds,
        output_width: output_probe
            .video
            .as_ref()
            .and_then(|v| v.width)
            .unwrap_or(0),
        output_height: output_probe
            .video
            .as_ref()
            .and_then(|v| v.height)
            .unwrap_or(0),
        output_frame_count,
        output_frame_rate: output_probe
            .video
            .as_ref()
            .and_then(|video| video.frame_rate.clone())
            .unwrap_or_default(),
        video_codec: output_probe
            .video
            .as_ref()
            .map(|v| v.codec.clone())
            .unwrap_or_default(),
        audio_codec: output_probe.audio.as_ref().map(|v| v.codec.clone()),
    })
}

struct RenderPlan {
    width: u32,
    height: u32,
    fps: FrameRate,
    background: String,
    audio_sample_rate: u32,
    total_frames: u64,
    total_samples: u64,
    inputs: Vec<PathBuf>,
    video_clips: Vec<VideoRenderNode>,
    text_clips: Vec<TextRenderNode>,
    audio_clips: Vec<AudioRenderNode>,
}

struct VideoRenderNode {
    input_index: usize,
    source_start: String,
    source_end: String,
    duration_frames: u64,
}

struct TextRenderNode {
    start_frame: u64,
    end_frame: u64,
    text: String,
    font_size: u32,
    color: String,
    x: u32,
    y: u32,
}

struct AudioRenderNode {
    input_index: usize,
    source_start: String,
    source_end: String,
    start_sample: u64,
    duration_samples: u64,
    gain: f64,
}

fn build_render_plan(validated: &ValidatedTimeline) -> Result<RenderPlan> {
    let timeline = &validated.timeline;
    let video_track = &timeline.tracks[validated.video_track_index];
    let mut video_clips: Vec<&VideoClip> = video_track
        .clips
        .iter()
        .filter_map(|clip| match clip {
            Clip::Video(value) => Some(value),
            _ => None,
        })
        .collect();
    video_clips.sort_by_key(|clip| clip.start_frame);

    let mut inputs = Vec::new();
    let mut video_nodes = Vec::new();
    for clip in video_clips {
        let asset = get_asset(&validated.assets, &clip.asset_id)?;
        let input_index = inputs.len();
        inputs.push(asset.path.clone());
        video_nodes.push(VideoRenderNode {
            input_index,
            source_start: timestamp_seconds(&clip.source_in),
            source_end: timestamp_seconds(&clip.source_out),
            duration_frames: clip.duration_frames,
        });
    }

    let mut text_nodes = Vec::new();
    for (track_index, clip_index) in &validated.text_clips {
        let Clip::Text(text) = &timeline.tracks[*track_index].clips[*clip_index] else {
            bail!("validated text clip index changed unexpectedly");
        };
        text_nodes.push(TextRenderNode {
            start_frame: text.start_frame,
            end_frame: text.start_frame + text.duration_frames,
            text: text.text.clone(),
            font_size: text.font_size,
            color: text.color.clone(),
            x: text.x,
            y: text.y,
        });
    }

    let mut audio_nodes = Vec::new();
    for (track_index, clip_index) in &validated.audio_clips {
        let Clip::Audio(audio) = &timeline.tracks[*track_index].clips[*clip_index] else {
            bail!("validated audio clip index changed unexpectedly");
        };
        let asset = get_asset(&validated.assets, &audio.asset_id)?;
        let input_index = inputs.len();
        inputs.push(asset.path.clone());
        let start_sample = frame_boundary_sample(
            audio.start_frame,
            timeline.canvas.fps,
            timeline.settings.audio_sample_rate,
        )?;
        let end_sample = frame_boundary_sample(
            audio.start_frame + audio.duration_frames,
            timeline.canvas.fps,
            timeline.settings.audio_sample_rate,
        )?;
        audio_nodes.push(AudioRenderNode {
            input_index,
            source_start: timestamp_seconds(&audio.source_in),
            source_end: timestamp_seconds(&audio.source_out),
            start_sample,
            duration_samples: end_sample - start_sample,
            gain: audio.gain,
        });
    }

    Ok(RenderPlan {
        width: timeline.canvas.width,
        height: timeline.canvas.height,
        fps: timeline.canvas.fps,
        background: timeline.canvas.background.clone(),
        audio_sample_rate: timeline.settings.audio_sample_rate,
        total_frames: validated.total_frames,
        total_samples: frame_boundary_sample(
            validated.total_frames,
            timeline.canvas.fps,
            timeline.settings.audio_sample_rate,
        )?,
        inputs,
        video_clips: video_nodes,
        text_clips: text_nodes,
        audio_clips: audio_nodes,
    })
}

fn compile_filter_graph(
    plan: &RenderPlan,
    work_dir: &Path,
    font: Option<&StagedFont>,
) -> Result<String> {
    let mut graph = Vec::new();
    let mut video_labels = Vec::new();
    for (index, clip) in plan.video_clips.iter().enumerate() {
        let label = format!("vclip{index}");
        let duration = frames_seconds(clip.duration_frames, plan.fps);
        let color = ffmpeg_color(&plan.background)?;
        graph.push(format!(
            "[{input_index}:v:0]trim=start={source_start}:end={source_end},setpts=PTS-STARTPTS,fps={fps},scale={width}:{height}:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color={color},setsar=1,tpad=stop_mode=clone:stop_duration={duration},trim=end_frame={frames},setpts=PTS-STARTPTS[{label}]",
            input_index = clip.input_index,
            source_start = clip.source_start,
            source_end = clip.source_end,
            fps = fps_string(plan.fps),
            width = plan.width,
            height = plan.height,
            frames = clip.duration_frames,
        ));
        video_labels.push(format!("[{label}]"));
    }

    let video_output = if video_labels.len() == 1 {
        graph.push(format!("{}null[vbase]", video_labels[0]));
        "vbase".to_owned()
    } else {
        graph.push(format!(
            "{}concat=n={}:v=1:a=0[vbase]",
            video_labels.join(""),
            video_labels.len()
        ));
        "vbase".to_owned()
    };

    let mut current_video = video_output;
    for (index, text) in plan.text_clips.iter().enumerate() {
        let font = font.ok_or_else(|| anyhow::anyhow!("text rendering requires --font"))?;
        let text_path = work_dir.join(format!("text-{index}.txt"));
        fs::write(&text_path, text.text.as_bytes()).context("could not stage text overlay")?;
        let text_file = staged_filter_filename(&text_path)?;
        let font_file = staged_filter_filename(&font.path)?;
        let next_video = format!("vtext{index}");
        graph.push(format!(
            "[{current_video}]drawtext=fontfile={font_file}:textfile={text_file}:expansion=none:fontsize={font_size}:fontcolor={color}:x={x}:y={y}:enable='between(n,{start},{end})'[{next_video}]",
            font_size = text.font_size,
            color = ffmpeg_color(&text.color)?,
            x = text.x,
            y = text.y,
            start = text.start_frame,
            end = text.end_frame - 1,
        ));
        current_video = next_video;
    }
    graph.push(format!("[{current_video}]format=yuv420p[vout]"));

    let mut audio_labels = Vec::new();
    for (index, audio) in plan.audio_clips.iter().enumerate() {
        let label = format!("aclip{index}");
        graph.push(format!(
            "[{input_index}:a:0]atrim=start={source_start}:end={source_end},asetpts=PTS-STARTPTS,aresample={sample_rate},apad=whole_len={clip_samples},atrim=end_sample={clip_samples},volume={gain:.6},adelay=delays={clip_start_sample}S:all=1[{label}]",
            input_index = audio.input_index,
            source_start = audio.source_start,
            source_end = audio.source_end,
            sample_rate = plan.audio_sample_rate,
            clip_samples = audio.duration_samples,
            gain = audio.gain,
            clip_start_sample = audio.start_sample,
        ));
        audio_labels.push(format!("[{label}]"));
    }

    if !audio_labels.is_empty() {
        graph.push(format!(
            "{}amix=inputs={}:duration=longest:dropout_transition=0:normalize=0,alimiter=limit=0.95,apad=whole_len={},atrim=end_sample={}[aout]",
            audio_labels.join(""),
            audio_labels.len(),
            plan.total_samples,
            plan.total_samples
        ));
    }

    Ok(graph.join(";\n"))
}

struct FfmpegInvocation<'a> {
    executable: &'a Path,
    inputs: &'a [PathBuf],
    filter_graph: &'a str,
    work_dir: &'a Path,
    output: &'a Path,
    frames: u64,
    has_audio: bool,
    timeout: Duration,
}

fn run_ffmpeg(invocation: FfmpegInvocation<'_>) -> Result<()> {
    let mut command = Command::new(invocation.executable);
    command
        .current_dir(invocation.work_dir)
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-v")
        .arg("error")
        .arg("-y")
        .arg("-filter_complex_threads")
        .arg("2");
    for input in invocation.inputs {
        command
            .arg("-protocol_whitelist")
            .arg("file,pipe")
            .arg("-i")
            .arg(input);
    }
    command
        .arg("-filter_complex")
        .arg(invocation.filter_graph)
        .arg("-map")
        .arg("[vout]");
    if invocation.has_audio {
        command.arg("-map").arg("[aout]");
    } else {
        command.arg("-an");
    }
    command
        .arg("-frames:v")
        .arg(invocation.frames.to_string())
        .arg("-fps_mode")
        .arg("cfr")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("medium")
        .arg("-crf")
        .arg("20")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-threads")
        .arg("4");
    if invocation.has_audio {
        command
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg("192k")
            .arg("-ar")
            .arg("48000");
    }
    command
        .arg("-movflags")
        .arg("+faststart")
        .arg("-fs")
        .arg(MAX_OUTPUT_BYTES.to_string())
        .arg(invocation.output);
    run_checked(&mut command, invocation.timeout, "FFmpeg render")?;
    Ok(())
}

fn verify_output(
    validated: &ValidatedTimeline,
    probe: &MediaProbe,
    actual_frame_count: u64,
) -> Result<()> {
    let video = probe
        .video
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("rendered output has no video stream"))?;
    if video.codec != "h264"
        || video.width != Some(validated.timeline.canvas.width)
        || video.height != Some(validated.timeline.canvas.height)
    {
        bail!("rendered output does not match requested H.264 dimensions");
    }
    if actual_frame_count != validated.total_frames {
        bail!(
            "rendered output has {actual_frame_count} frames; expected {}",
            validated.total_frames
        );
    }
    let expected_rate = validated.timeline.canvas.fps;
    let output_rate = video
        .frame_rate
        .as_deref()
        .and_then(parse_frame_rate)
        .ok_or_else(|| anyhow::anyhow!("rendered video frame rate is unavailable or invalid"))?;
    if u64::from(output_rate.0) * u64::from(expected_rate.den)
        != u64::from(expected_rate.num) * u64::from(output_rate.1)
    {
        bail!(
            "rendered output frame rate is {}/{}; expected {}/{}",
            output_rate.0,
            output_rate.1,
            expected_rate.num,
            expected_rate.den
        );
    }
    if !probe.duration_seconds.is_finite() {
        bail!("rendered output has an invalid duration");
    }
    let expected_duration =
        frames_seconds(validated.total_frames, validated.timeline.canvas.fps).parse::<f64>()?;
    let one_frame =
        f64::from(validated.timeline.canvas.fps.den) / f64::from(validated.timeline.canvas.fps.num);
    if (probe.duration_seconds - expected_duration).abs() > one_frame {
        bail!("rendered output duration differs by more than one frame");
    }
    if validated.audio_clips.is_empty() {
        if probe.has_audio {
            bail!("unexpected audio stream in rendered output");
        }
    } else {
        let audio = probe
            .audio
            .as_ref()
            .filter(|audio| audio.codec == "aac")
            .ok_or_else(|| anyhow::anyhow!("rendered output is missing its AAC audio stream"))?;
        let audio_duration = audio
            .duration_seconds
            .ok_or_else(|| anyhow::anyhow!("rendered AAC stream duration is unavailable"))?;
        if (audio_duration - expected_duration).abs() > one_frame {
            bail!("rendered AAC stream does not cover the full video timeline");
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct FrameCountOutput {
    streams: Vec<FrameCountStream>,
}

#[derive(Deserialize)]
struct FrameCountStream {
    nb_read_frames: Option<String>,
}

fn count_video_frames(ffprobe: &Path, file: &Path, timeout: Duration) -> Result<u64> {
    let output = run_checked(
        Command::new(ffprobe)
            .arg("-v")
            .arg("error")
            .arg("-count_frames")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-show_entries")
            .arg("stream=nb_read_frames")
            .arg("-of")
            .arg("json")
            .arg("-protocol_whitelist")
            .arg("file,pipe")
            .arg(file),
        timeout,
        "ffprobe frame count",
    )?;
    let parsed: FrameCountOutput = serde_json::from_slice(&output.stdout)
        .context("ffprobe returned malformed frame-count metadata")?;
    parsed
        .streams
        .first()
        .and_then(|stream| stream.nb_read_frames.as_deref())
        .ok_or_else(|| anyhow::anyhow!("ffprobe did not return a video frame count"))?
        .parse::<u64>()
        .context("ffprobe returned an invalid video frame count")
}

fn parse_frame_rate(value: &str) -> Option<(u32, u32)> {
    let (numerator, denominator) = value.split_once('/')?;
    let numerator = numerator.parse::<u32>().ok()?;
    let denominator = denominator.parse::<u32>().ok()?;
    (numerator > 0 && denominator > 0).then_some((numerator, denominator))
}

fn build_provenance<'a>(
    validated: &'a ValidatedTimeline,
    timeline_hash: &'a str,
    font_hash: Option<&'a str>,
    ffmpeg: &'a ToolIdentity,
    ffprobe: &'a ToolIdentity,
    output_hash: &'a str,
) -> Provenance<'a> {
    let assets = validated
        .assets
        .iter()
        .map(|(id, asset)| ProvenanceAsset {
            asset_id: id,
            sha256: &asset.manifest.sha256,
        })
        .collect();
    Provenance {
        schema_version: 1,
        renderer_version: env!("CARGO_PKG_VERSION"),
        project_id: &validated.timeline.project_id,
        revision: validated.timeline.revision,
        timeline_sha256: timeline_hash,
        assets,
        font_sha256: font_hash,
        ffmpeg_version: &ffmpeg.version,
        ffmpeg_fingerprint: &ffmpeg.fingerprint,
        ffprobe_version: &ffprobe.version,
        ffprobe_fingerprint: &ffprobe.fingerprint,
        width: validated.timeline.canvas.width,
        height: validated.timeline.canvas.height,
        fps: fps_string(validated.timeline.canvas.fps),
        frame_count: validated.total_frames,
        video_encoder: "libx264, preset=medium, crf=20, pix_fmt=yuv420p",
        audio_encoder: if validated.audio_clips.is_empty() {
            None
        } else {
            Some("aac, 192 kbps, 48000 Hz")
        },
        output_sha256: output_hash,
        output_preset: validated
            .timeline
            .settings
            .export_preset
            .as_deref()
            .unwrap_or("custom"),
    }
}

struct StagedFont {
    path: PathBuf,
    sha256: String,
}

fn stage_font(path: Option<&Path>, work_dir: &Path, required: bool) -> Result<Option<StagedFont>> {
    if !required {
        return Ok(None);
    }
    let path =
        path.ok_or_else(|| anyhow::anyhow!("timeline has text clips; pass --font <font-file>"))?;
    let source = fs::canonicalize(path).context("could not resolve font file")?;
    let metadata = fs::metadata(&source).context("could not inspect font file")?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_FONT_BYTES {
        bail!("font must be a regular file no larger than 100 MiB");
    }
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow::anyhow!("font file must use .ttf, .otf or .ttc extension"))?;
    if !matches!(extension.as_str(), "ttf" | "otf" | "ttc") {
        bail!("font file must use .ttf, .otf or .ttc extension");
    }
    let destination = work_dir.join(format!("font.{extension}"));
    fs::copy(&source, &destination).context("could not stage font file")?;
    Ok(Some(StagedFont {
        sha256: hash_file(&destination)?,
        path: destination,
    }))
}

fn get_asset<'a>(
    assets: &'a std::collections::BTreeMap<String, ResolvedAsset>,
    id: &str,
) -> Result<&'a ResolvedAsset> {
    assets
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("clip references an unregistered asset"))
}

fn timestamp_seconds(timestamp: &crate::timeline::SourceTimestamp) -> String {
    let seconds =
        timestamp.ticks as f64 * timestamp.timebase_num as f64 / timestamp.timebase_den as f64;
    format!("{seconds:.9}")
}

fn frames_seconds(frames: u64, fps: FrameRate) -> String {
    let seconds = frames as f64 * f64::from(fps.den) / f64::from(fps.num);
    format!("{seconds:.9}")
}

fn fps_string(fps: FrameRate) -> String {
    format!("{}/{}", fps.num, fps.den)
}

fn ffmpeg_color(color: &str) -> Result<String> {
    if color.len() != 7 || !color.starts_with('#') {
        bail!("invalid color in validated timeline");
    }
    Ok(format!("0x{}", &color[1..]))
}

fn staged_filter_filename(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("staged filter asset has an invalid name"))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("staged filter asset filename contains unsupported characters");
    }
    Ok(name.to_owned())
}

fn prepare_output_paths(output: &Path) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    if output
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|ext| !ext.eq_ignore_ascii_case("mp4"))
    {
        bail!("output path must use the .mp4 extension");
    }
    let file_name = output
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("output path must name a file"))?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        bail!("output directory must already exist");
    }
    let parent = fs::canonicalize(parent).context("could not resolve output directory")?;
    let output_path = parent.join(file_name);
    let mut provenance_name = file_name.to_os_string();
    provenance_name.push(".provenance.json");
    let provenance_path = parent.join(provenance_name);
    ensure_output_absent(&output_path, &provenance_path)?;
    let id = Uuid::new_v4();
    let output_temp = parent.join(format!(".studio-{id}.tmp.mp4"));
    let provenance_temp = parent.join(format!(".studio-{id}.tmp.provenance.json"));
    Ok((output_path, output_temp, provenance_path, provenance_temp))
}

fn ensure_output_absent(output: &Path, provenance: &Path) -> Result<()> {
    for path in [output, provenance] {
        if fs::symlink_metadata(path).is_ok() {
            bail!("output or provenance path already exists; refusing to overwrite");
        }
    }
    Ok(())
}

struct ToolIdentity {
    version: String,
    fingerprint: String,
}

fn tool_identity(path: &Path, timeout: Duration) -> Result<ToolIdentity> {
    let output = run_checked(
        Command::new(path).arg("-version"),
        timeout.min(Duration::from_secs(10)),
        &format!("{} version check", path.display()),
    )?;
    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("unknown version")
        .to_owned();
    Ok(ToolIdentity {
        version,
        fingerprint: sha256(&output.stdout),
    })
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("could not read file for SHA-256")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).context("could not hash file")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct RenderCleanup {
    work_dir: Option<PathBuf>,
    output_temp: Option<PathBuf>,
    provenance_temp: Option<PathBuf>,
    published_output: Option<PathBuf>,
}

impl Drop for RenderCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.work_dir.take() {
            let _ = fs::remove_dir_all(path);
        }
        if let Some(path) = self.output_temp.take() {
            let _ = fs::remove_file(path);
        }
        if let Some(path) = self.provenance_temp.take() {
            let _ = fs::remove_file(path);
        }
        if let Some(path) = self.published_output.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rational_output_rates_without_decimal_rounding() {
        assert_eq!(parse_frame_rate("24/1"), Some((24, 1)));
        assert_eq!(parse_frame_rate("30000/1001"), Some((30_000, 1_001)));
        assert_eq!(parse_frame_rate("0/0"), None);
        assert_eq!(parse_frame_rate("24"), None);
    }
}
