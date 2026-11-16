use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    media::{MediaProbe, probe_media_cancellable},
    process::{run_checked_cancellable, run_checked_cancellable_with_progress},
    project::RenderJobPhase,
    timeline::{
        Clip, CropRect, FrameRate, ImageMotion, ResolvedAsset, ValidatedTimeline,
        frame_boundary_sample, load_and_validate,
    },
    workspace::Workspace,
};

const MAX_FONT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const PREVIEW_MAX_EDGE: u32 = 640;

pub(crate) type RenderProgressCallback = Arc<dyn Fn(RenderJobPhase, u8) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderPreset {
    Export,
    Preview,
}

impl RenderPreset {
    pub(crate) fn parse(id: &str) -> Result<Self> {
        match id {
            "export" => Ok(Self::Export),
            "preview" => Ok(Self::Preview),
            _ => bail!("VALIDATION_FAILED: presetId must be export or preview"),
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Preview => "preview",
        }
    }

    pub(crate) fn output_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        if self == Self::Export || width.max(height) <= PREVIEW_MAX_EDGE {
            return (width, height);
        }

        if width >= height {
            (
                PREVIEW_MAX_EDGE,
                scaled_even_dimension(height, PREVIEW_MAX_EDGE, width),
            )
        } else {
            (
                scaled_even_dimension(width, PREVIEW_MAX_EDGE, height),
                PREVIEW_MAX_EDGE,
            )
        }
    }

    fn encoder_settings(self) -> (&'static str, &'static str) {
        match self {
            Self::Export => ("medium", "20"),
            Self::Preview => ("veryfast", "28"),
        }
    }
}

fn scaled_even_dimension(dimension: u32, target_major: u32, source_major: u32) -> u32 {
    let numerator = u64::from(dimension) * u64::from(target_major);
    let denominator = u64::from(source_major);
    let nearest_even = ((numerator + denominator) / (2 * denominator)) * 2;
    u32::try_from(nearest_even.max(2)).unwrap_or(2)
}

pub struct RenderOptions {
    pub workspace: PathBuf,
    pub timeline: PathBuf,
    pub output: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub font: Option<PathBuf>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    render_preset: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProvenanceAsset<'a> {
    asset_id: &'a str,
    sha256: &'a str,
}

pub fn render_timeline(options: RenderOptions) -> Result<RenderResult> {
    render_timeline_cancellable(options, None)
}

pub(crate) fn render_timeline_cancellable(
    options: RenderOptions,
    cancellation: Option<std::sync::Arc<AtomicBool>>,
) -> Result<RenderResult> {
    render_timeline_with_preset_cancellable(options, RenderPreset::Export, cancellation)
}

pub(crate) fn render_timeline_with_preset_cancellable(
    options: RenderOptions,
    preset: RenderPreset,
    cancellation: Option<std::sync::Arc<AtomicBool>>,
) -> Result<RenderResult> {
    render_timeline_with_progress_cancellable(options, preset, cancellation, None)
}

pub(crate) fn render_timeline_with_progress_cancellable(
    options: RenderOptions,
    preset: RenderPreset,
    cancellation: Option<Arc<AtomicBool>>,
    progress: Option<RenderProgressCallback>,
) -> Result<RenderResult> {
    if !(1..=3600).contains(&options.timeout_seconds) {
        bail!("timeout-seconds must be between 1 and 3600");
    }
    report_progress(&progress, RenderJobPhase::Preparing, 2);
    ensure_not_cancelled(cancellation.as_deref())?;
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
    let plan = build_render_plan(&validated, preset)?;
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

    let ffmpeg = tool_identity(&options.ffmpeg, timeout, cancellation.as_deref())?;
    let ffprobe = tool_identity(&options.ffprobe, timeout, cancellation.as_deref())?;
    run_ffmpeg(FfmpegInvocation {
        executable: &options.ffmpeg,
        inputs: &plan.inputs,
        filter_graph: &filter_graph,
        work_dir: &work_dir,
        output: &output_temp,
        frames: plan.total_frames,
        has_audio: !plan.audio_clips.is_empty(),
        preset,
        timeout,
        cancellation: cancellation.as_deref(),
        progress: progress.as_ref(),
    })?;
    ensure_not_cancelled(cancellation.as_deref())?;
    report_progress(&progress, RenderJobPhase::Validating, 92);

    let output_metadata = fs::metadata(&output_temp).context("FFmpeg did not create an output")?;
    if output_metadata.len() == 0 || output_metadata.len() > MAX_OUTPUT_BYTES {
        bail!("render output is empty or exceeds the 2 GiB output limit");
    }
    let output_probe =
        probe_media_cancellable(&options.ffprobe, &output_temp, cancellation.as_deref())?;
    let output_frame_count = count_video_frames(
        &options.ffprobe,
        &output_temp,
        timeout,
        cancellation.as_deref(),
    )?;
    ensure_not_cancelled(cancellation.as_deref())?;
    verify_output(&validated, &output_probe, output_frame_count, preset)?;
    let output_hash = hash_file(&output_temp)?;
    let font_hash = font.as_ref().map(|font| font.sha256.as_str());
    let provenance = build_provenance(
        &validated,
        &timeline_hash,
        font_hash,
        &ffmpeg,
        &ffprobe,
        &output_hash,
        preset,
    );
    ensure_not_cancelled(cancellation.as_deref())?;

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
    ensure_not_cancelled(cancellation.as_deref())?;
    ensure_output_absent(&output_path, &provenance_path)?;
    report_progress(&progress, RenderJobPhase::Publishing, 98);
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
    source_start: Option<String>,
    source_end: Option<String>,
    duration_frames: u64,
    transition_in_frames: u64,
    crop: Option<CropRect>,
    is_still_image: bool,
    motion: ImageMotion,
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
    fade_in_samples: u64,
    fade_out_samples: u64,
}

fn build_render_plan(validated: &ValidatedTimeline, preset: RenderPreset) -> Result<RenderPlan> {
    let timeline = &validated.timeline;
    let (width, height) = preset.output_dimensions(timeline.canvas.width, timeline.canvas.height);
    let video_track = &timeline.tracks[validated.video_track_index];
    let mut video_clips: Vec<&Clip> = video_track
        .clips
        .iter()
        .filter(|clip| matches!(clip, Clip::Video(_) | Clip::Image(_)))
        .collect();
    video_clips.sort_by_key(|clip| clip.start_frame());

    let mut inputs = Vec::new();
    let mut video_nodes = Vec::new();
    for clip in video_clips {
        let (
            asset_id,
            source_start,
            source_end,
            duration_frames,
            transition_in_frames,
            crop,
            is_still_image,
            motion,
        ) = match clip {
            Clip::Video(clip) => (
                clip.asset_id.as_str(),
                Some(timestamp_seconds(&clip.source_in)),
                Some(timestamp_seconds(&clip.source_out)),
                clip.duration_frames,
                clip.transition_in
                    .as_ref()
                    .map_or(0, |value| value.duration_frames()),
                clip.crop.clone(),
                false,
                ImageMotion::Static,
            ),
            Clip::Image(clip) => (
                clip.asset_id.as_str(),
                None,
                None,
                clip.duration_frames,
                clip.transition_in
                    .as_ref()
                    .map_or(0, |value| value.duration_frames()),
                clip.crop.clone(),
                true,
                clip.motion,
            ),
            _ => bail!("validated visual clip changed unexpectedly"),
        };
        let asset = get_asset(&validated.assets, asset_id)?;
        let input_index = inputs.len();
        inputs.push(asset.path.clone());
        video_nodes.push(VideoRenderNode {
            input_index,
            source_start,
            source_end,
            duration_frames,
            transition_in_frames,
            crop,
            is_still_image,
            motion,
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
            font_size: scale_text_font_size(
                text.font_size,
                timeline.canvas.width,
                timeline.canvas.height,
                width,
                height,
            ),
            color: text.color.clone(),
            x: scale_coordinate(text.x, timeline.canvas.width, width),
            y: scale_coordinate(text.y, timeline.canvas.height, height),
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
        let duration_samples = end_sample - start_sample;
        let fade_in_samples = frame_boundary_sample(
            audio.fade_in_frames,
            timeline.canvas.fps,
            timeline.settings.audio_sample_rate,
        )?
        .min(duration_samples);
        let fade_out_samples = frame_boundary_sample(
            audio.fade_out_frames,
            timeline.canvas.fps,
            timeline.settings.audio_sample_rate,
        )?
        .min(duration_samples.saturating_sub(fade_in_samples));
        audio_nodes.push(AudioRenderNode {
            input_index,
            source_start: timestamp_seconds(&audio.source_in),
            source_end: timestamp_seconds(&audio.source_out),
            start_sample,
            duration_samples,
            gain: audio.gain,
            fade_in_samples,
            fade_out_samples,
        });
    }

    Ok(RenderPlan {
        width,
        height,
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

fn scale_coordinate(value: u32, source_extent: u32, output_extent: u32) -> u32 {
    let scaled = u64::from(value) * u64::from(output_extent);
    u32::try_from((scaled + u64::from(source_extent) / 2) / u64::from(source_extent))
        .unwrap_or(u32::MAX)
}

fn image_motion_filter(
    motion: ImageMotion,
    duration_frames: u64,
    width: u32,
    height: u32,
    fps: FrameRate,
) -> String {
    if motion == ImageMotion::Static {
        return String::new();
    }

    let denominator = duration_frames.saturating_sub(1).max(1);
    let (zoom, x, y) = match motion {
        ImageMotion::Static => unreachable!("static motion exits before building a filter"),
        ImageMotion::ZoomIn => (
            format!("1+0.08*on/{denominator}"),
            "(iw-iw/zoom)/2".to_owned(),
            "(ih-ih/zoom)/2".to_owned(),
        ),
        ImageMotion::ZoomOut => (
            format!("1.08-0.08*on/{denominator}"),
            "(iw-iw/zoom)/2".to_owned(),
            "(ih-ih/zoom)/2".to_owned(),
        ),
        ImageMotion::PanLeft => (
            "1.08".to_owned(),
            format!("(iw-iw/zoom)*on/{denominator}"),
            "(ih-ih/zoom)/2".to_owned(),
        ),
        ImageMotion::PanRight => (
            "1.08".to_owned(),
            format!("(iw-iw/zoom)*(1-on/{denominator})"),
            "(ih-ih/zoom)/2".to_owned(),
        ),
        ImageMotion::PanUp => (
            "1.08".to_owned(),
            "(iw-iw/zoom)/2".to_owned(),
            format!("(ih-ih/zoom)*on/{denominator}"),
        ),
        ImageMotion::PanDown => (
            "1.08".to_owned(),
            "(iw-iw/zoom)/2".to_owned(),
            format!("(ih-ih/zoom)*(1-on/{denominator})"),
        ),
    };

    format!(
        "zoompan=z='{zoom}':x='{x}':y='{y}':d=1:s={width}x{height}:fps={fps},setpts=PTS-STARTPTS,",
        fps = fps_string(fps),
    )
}

fn scale_text_font_size(
    font_size: u32,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
) -> u32 {
    scale_coordinate(font_size, source_width, output_width)
        .min(scale_coordinate(font_size, source_height, output_height))
        .max(1)
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
        let crop_filter = clip.crop.as_ref().map_or_else(String::new, |crop| {
            format!(
                "crop=w=iw*{:.6}:h=ih*{:.6}:x=iw*{:.6}:y=ih*{:.6},",
                crop.width, crop.height, crop.x, crop.y
            )
        });
        // Stills do not change per frame, so resize and pad their source frame before looping it.
        let fit_filter = format!(
            "scale={}:{}:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color={color},setsar=1,",
            plan.width, plan.height, plan.width, plan.height
        );
        let source_filter = if clip.is_still_image {
            format!(
                "{crop_filter}{fit_filter}loop=loop=-1:size=1:start=0,setpts=N*{fps_den}/({fps_num}*TB),trim=end_frame={frames},setpts=PTS-STARTPTS,fps={fps},",
                fps_num = plan.fps.num,
                fps_den = plan.fps.den,
                frames = clip.duration_frames,
                fps = fps_string(plan.fps),
            )
        } else {
            format!(
                "trim=start={}:end={},setpts=PTS-STARTPTS,{crop_filter}fps={fps},{fit_filter}",
                clip.source_start.as_deref().unwrap_or("0"),
                clip.source_end.as_deref().unwrap_or("0"),
                fps = fps_string(plan.fps),
            )
        };
        let motion_filter = if clip.is_still_image {
            image_motion_filter(
                clip.motion,
                clip.duration_frames,
                plan.width,
                plan.height,
                plan.fps,
            )
        } else {
            String::new()
        };
        graph.push(format!(
            "[{input_index}:v:0]{source_filter}{motion_filter}tpad=stop_mode=clone:stop_duration={duration},trim=end_frame={frames},setpts=PTS-STARTPTS[{label}]",
            input_index = clip.input_index,
            source_filter = source_filter,
            motion_filter = motion_filter,
            frames = clip.duration_frames,
            duration = duration,
            label = label,
        ));
        video_labels.push(format!("[{label}]"));
    }

    let has_transitions = plan
        .video_clips
        .iter()
        .any(|clip| clip.transition_in_frames > 0);
    let video_output = if video_labels.len() == 1 {
        graph.push(format!("{}null[vbase]", video_labels[0]));
        "vbase".to_owned()
    } else if has_transitions {
        let mut current = "vclip0".to_owned();
        let mut assembled_frames = plan.video_clips[0].duration_frames;
        for (index, clip) in plan.video_clips.iter().enumerate().skip(1) {
            let next = if clip.transition_in_frames == 0 {
                let next = format!("vconcat{index}");
                graph.push(format!(
                    "[{current}][vclip{index}]concat=n=2:v=1:a=0[{next}]"
                ));
                assembled_frames = assembled_frames
                    .checked_add(clip.duration_frames)
                    .context("render video frame count overflowed")?;
                next
            } else {
                let offset_frames = assembled_frames
                    .checked_sub(clip.transition_in_frames)
                    .context("crossfade exceeds the preceding video sequence")?;
                let next = format!("vtransition{index}");
                graph.push(format!(
                    "[{current}][vclip{index}]xfade=transition=fade:duration={duration}:offset={offset}[{next}]",
                    duration = frames_seconds(clip.transition_in_frames, plan.fps),
                    offset = frames_seconds(offset_frames, plan.fps),
                ));
                assembled_frames = assembled_frames
                    .checked_add(clip.duration_frames)
                    .and_then(|frames| frames.checked_sub(clip.transition_in_frames))
                    .context("render video frame count overflowed")?;
                next
            };
            current = next;
        }
        graph.push(format!("[{current}]null[vbase]"));
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
        let mut audio_filter = format!(
            "[{input_index}:a:0]atrim=start={source_start}:end={source_end},asetpts=PTS-STARTPTS,aresample={sample_rate},apad=whole_len={clip_samples},atrim=end_sample={clip_samples},volume={gain:.6}",
            input_index = audio.input_index,
            source_start = audio.source_start,
            source_end = audio.source_end,
            sample_rate = plan.audio_sample_rate,
            clip_samples = audio.duration_samples,
            gain = audio.gain,
        );
        if audio.fade_in_samples > 0 {
            audio_filter.push_str(&format!(
                ",afade=t=in:curve=tri:ss=0:ns={}",
                audio.fade_in_samples
            ));
        }
        if audio.fade_out_samples > 0 {
            let fade_out_start = audio.duration_samples - audio.fade_out_samples;
            audio_filter.push_str(&format!(
                ",afade=t=out:curve=tri:ss={fade_out_start}:ns={}",
                audio.fade_out_samples
            ));
        }
        audio_filter.push_str(&format!(
            ",adelay=delays={}S:all=1[{label}]",
            audio.start_sample
        ));
        graph.push(audio_filter);
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
    preset: RenderPreset,
    timeout: Duration,
    cancellation: Option<&'a AtomicBool>,
    progress: Option<&'a RenderProgressCallback>,
}

fn run_ffmpeg(invocation: FfmpegInvocation<'_>) -> Result<()> {
    let mut command = Command::new(invocation.executable);
    let (encoder_preset, crf) = invocation.preset.encoder_settings();
    command
        .current_dir(invocation.work_dir)
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-v")
        .arg("error")
        .arg("-y")
        .arg("-filter_complex_threads")
        .arg("2");
    if invocation.progress.is_some() {
        command
            .arg("-progress")
            .arg("pipe:1")
            .arg("-stats_period")
            .arg("1")
            .arg("-nostats");
    }
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
        .arg(encoder_preset)
        .arg("-crf")
        .arg(crf)
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
    if let Some(progress) = invocation.progress {
        progress(RenderJobPhase::Encoding, 5);
        let callback = Arc::clone(progress);
        let total_frames = invocation.frames.max(1);
        run_checked_cancellable_with_progress(
            &mut command,
            invocation.timeout,
            "FFmpeg render",
            invocation.cancellation,
            Arc::new(move |frame| {
                let completed_frames = frame.min(total_frames);
                let percent = 5 + ((completed_frames.saturating_mul(85) / total_frames) as u8);
                callback(RenderJobPhase::Encoding, percent.min(90));
            }),
        )?;
    } else {
        run_checked_cancellable(
            &mut command,
            invocation.timeout,
            "FFmpeg render",
            invocation.cancellation,
        )?;
    }
    Ok(())
}

fn report_progress(progress: &Option<RenderProgressCallback>, phase: RenderJobPhase, percent: u8) {
    if let Some(progress) = progress {
        progress(phase, percent);
    }
}

fn ensure_not_cancelled(cancellation: Option<&AtomicBool>) -> Result<()> {
    if cancellation.is_some_and(|token| token.load(Ordering::Acquire)) {
        bail!("render was cancelled");
    }
    Ok(())
}

fn verify_output(
    validated: &ValidatedTimeline,
    probe: &MediaProbe,
    actual_frame_count: u64,
    preset: RenderPreset,
) -> Result<()> {
    let (expected_width, expected_height) = preset.output_dimensions(
        validated.timeline.canvas.width,
        validated.timeline.canvas.height,
    );
    let video = probe
        .video
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("rendered output has no video stream"))?;
    if video.codec != "h264"
        || video.width != Some(expected_width)
        || video.height != Some(expected_height)
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

fn count_video_frames(
    ffprobe: &Path,
    file: &Path,
    timeout: Duration,
    cancellation: Option<&AtomicBool>,
) -> Result<u64> {
    let output = run_checked_cancellable(
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
        cancellation,
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
    preset: RenderPreset,
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
        width: preset
            .output_dimensions(
                validated.timeline.canvas.width,
                validated.timeline.canvas.height,
            )
            .0,
        height: preset
            .output_dimensions(
                validated.timeline.canvas.width,
                validated.timeline.canvas.height,
            )
            .1,
        fps: fps_string(validated.timeline.canvas.fps),
        frame_count: validated.total_frames,
        video_encoder: match preset {
            RenderPreset::Export => "libx264, preset=medium, crf=20, pix_fmt=yuv420p",
            RenderPreset::Preview => "libx264, preset=veryfast, crf=28, pix_fmt=yuv420p",
        },
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
        render_preset: preset.id(),
    }
}

struct StagedFont {
    path: PathBuf,
    sha256: String,
}

pub(crate) fn validate_configured_font(path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("could not inspect font {}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_FONT_BYTES {
        bail!("font must be a regular file no larger than 100 MiB");
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !matches!(
        extension.to_ascii_lowercase().as_str(),
        "ttf" | "otf" | "ttc"
    ) {
        bail!("font file must use .ttf, .otf or .ttc extension");
    }
    Ok(())
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

fn tool_identity(
    path: &Path,
    timeout: Duration,
    cancellation: Option<&AtomicBool>,
) -> Result<ToolIdentity> {
    let output = run_checked_cancellable(
        Command::new(path).arg("-version"),
        timeout.min(Duration::from_secs(10)),
        &format!("{} version check", path.display()),
        cancellation,
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
    use crate::media::{ImportOptions, import_asset};
    use tempfile::tempdir;

    #[test]
    fn parses_rational_output_rates_without_decimal_rounding() {
        assert_eq!(parse_frame_rate("24/1"), Some((24, 1)));
        assert_eq!(parse_frame_rate("30000/1001"), Some((30_000, 1_001)));
        assert_eq!(parse_frame_rate("0/0"), None);
        assert_eq!(parse_frame_rate("24"), None);
    }

    #[test]
    fn preview_dimensions_scale_landscape_and_portrait_without_upscaling() {
        assert_eq!(
            RenderPreset::Preview.output_dimensions(1920, 1080),
            (640, 360)
        );
        assert_eq!(
            RenderPreset::Preview.output_dimensions(1080, 1920),
            (360, 640)
        );
        assert_eq!(
            RenderPreset::Preview.output_dimensions(1280, 720),
            (640, 360)
        );
        assert_eq!(
            RenderPreset::Preview.output_dimensions(320, 240),
            (320, 240)
        );
        assert_eq!(
            RenderPreset::Preview.output_dimensions(640, 640),
            (640, 640)
        );
        assert_eq!(
            RenderPreset::Export.output_dimensions(1920, 1080),
            (1920, 1080)
        );
    }

    #[test]
    fn preview_rounding_keeps_small_and_odd_aspect_dimensions_valid() {
        let (width, height) = RenderPreset::Preview.output_dimensions(1920, 2);
        assert_eq!(width, 640);
        assert_eq!(height, 2);
        assert_eq!(height % 2, 0);
        assert_eq!(scale_coordinate(24, 1920, 640), 8);
        assert_eq!(scale_text_font_size(48, 1920, 1080, 640, 360), 16);
        assert_eq!(scale_text_font_size(1, 1920, 1080, 640, 360), 1);
    }

    #[test]
    fn crop_rect_is_applied_before_fit_and_pad_scaling() {
        let work_dir = tempdir().unwrap();
        let plan = RenderPlan {
            width: 640,
            height: 360,
            fps: FrameRate { num: 30, den: 1 },
            background: "#101820".to_owned(),
            audio_sample_rate: 48_000,
            total_frames: 30,
            total_samples: 48_000,
            inputs: Vec::new(),
            video_clips: vec![VideoRenderNode {
                input_index: 0,
                source_start: Some("0".to_owned()),
                source_end: Some("1".to_owned()),
                duration_frames: 30,
                transition_in_frames: 0,
                crop: Some(CropRect {
                    x: 0.25,
                    y: 0.0,
                    width: 0.5,
                    height: 1.0,
                }),
                is_still_image: false,
                motion: ImageMotion::Static,
            }],
            text_clips: Vec::new(),
            audio_clips: Vec::new(),
        };

        let graph = compile_filter_graph(&plan, work_dir.path(), None).unwrap();
        assert!(graph.contains("crop=w=iw*0.500000:h=ih*1.000000:x=iw*0.250000:y=ih*0.000000"));
        assert!(graph.contains("scale=640:360:force_original_aspect_ratio=decrease"));
        assert!(graph.contains("pad=640:360"));
    }

    #[test]
    fn audio_filter_applies_sample_accurate_linear_fades_before_timeline_delay() {
        let work_dir = tempdir().unwrap();
        let plan = RenderPlan {
            width: 640,
            height: 360,
            fps: FrameRate { num: 30, den: 1 },
            background: "#101820".to_owned(),
            audio_sample_rate: 48_000,
            total_frames: 30,
            total_samples: 48_000,
            inputs: Vec::new(),
            video_clips: vec![VideoRenderNode {
                input_index: 0,
                source_start: Some("0".to_owned()),
                source_end: Some("1".to_owned()),
                duration_frames: 30,
                transition_in_frames: 0,
                crop: None,
                is_still_image: false,
                motion: ImageMotion::Static,
            }],
            text_clips: Vec::new(),
            audio_clips: vec![AudioRenderNode {
                input_index: 1,
                source_start: "0".to_owned(),
                source_end: "1".to_owned(),
                start_sample: 4_800,
                duration_samples: 48_000,
                gain: 0.5,
                fade_in_samples: 2_400,
                fade_out_samples: 4_800,
            }],
        };

        let graph = compile_filter_graph(&plan, work_dir.path(), None).unwrap();
        assert!(graph.contains("afade=t=in:curve=tri:ss=0:ns=2400"));
        assert!(graph.contains("afade=t=out:curve=tri:ss=43200:ns=4800"));
        assert!(graph.contains("adelay=delays=4800S:all=1[aclip0]"));
    }

    #[test]
    fn still_images_are_fit_once_before_the_frame_loop() {
        let work_dir = tempdir().unwrap();
        let plan = RenderPlan {
            width: 640,
            height: 360,
            fps: FrameRate { num: 30, den: 1 },
            background: "#101820".to_owned(),
            audio_sample_rate: 48_000,
            total_frames: 300,
            total_samples: 480_000,
            inputs: Vec::new(),
            video_clips: vec![VideoRenderNode {
                input_index: 0,
                source_start: None,
                source_end: None,
                duration_frames: 300,
                transition_in_frames: 0,
                crop: None,
                is_still_image: true,
                motion: ImageMotion::ZoomIn,
            }],
            text_clips: Vec::new(),
            audio_clips: Vec::new(),
        };

        let graph = compile_filter_graph(&plan, work_dir.path(), None).unwrap();
        let scaling = graph.find("scale=640:360").unwrap();
        let looping = graph.find("loop=loop=-1:size=1:start=0").unwrap();
        let motion = graph.find("zoompan=z='1+0.08*on/299'").unwrap();
        assert!(scaling < looping);
        assert!(looping < motion);
        assert!(graph.contains("s=640x360:fps=30/1"));
    }

    #[test]
    fn transition_graph_crossfades_incoming_clips_at_exact_frame_offsets() {
        let work_dir = tempdir().unwrap();
        let plan = RenderPlan {
            width: 640,
            height: 360,
            fps: FrameRate { num: 24, den: 1 },
            background: "#101820".to_owned(),
            audio_sample_rate: 48_000,
            total_frames: 36,
            total_samples: 72_000,
            inputs: Vec::new(),
            video_clips: vec![
                VideoRenderNode {
                    input_index: 0,
                    source_start: Some("0".to_owned()),
                    source_end: Some("1".to_owned()),
                    duration_frames: 24,
                    transition_in_frames: 0,
                    crop: None,
                    is_still_image: false,
                    motion: ImageMotion::Static,
                },
                VideoRenderNode {
                    input_index: 1,
                    source_start: Some("0".to_owned()),
                    source_end: Some("1".to_owned()),
                    duration_frames: 24,
                    transition_in_frames: 12,
                    crop: None,
                    is_still_image: false,
                    motion: ImageMotion::Static,
                },
            ],
            text_clips: Vec::new(),
            audio_clips: Vec::new(),
        };

        let graph = compile_filter_graph(&plan, work_dir.path(), None).unwrap();
        assert!(graph.contains(
            "[vclip0][vclip1]xfade=transition=fade:duration=0.500000000:offset=0.500000000[vtransition1]"
        ));
        assert!(!graph.contains("concat=n=2"));
    }

    #[test]
    fn each_image_motion_preset_has_a_bounded_frame_expression() {
        let fps = FrameRate { num: 24, den: 1 };
        for motion in [
            ImageMotion::ZoomIn,
            ImageMotion::ZoomOut,
            ImageMotion::PanLeft,
            ImageMotion::PanRight,
            ImageMotion::PanUp,
            ImageMotion::PanDown,
        ] {
            let filter = image_motion_filter(motion, 24, 320, 240, fps);
            assert!(filter.contains(":d=1:s=320x240:fps=24/1"));
            assert!(filter.contains("on/23"));
        }
        assert!(image_motion_filter(ImageMotion::Static, 24, 320, 240, fps).is_empty());
        let single_frame = image_motion_filter(ImageMotion::ZoomIn, 1, 320, 240, fps);
        assert!(single_frame.contains("on/1"));
    }

    #[test]
    fn image_motion_renders_with_preview_and_export_dimensions() {
        let Some(ffmpeg) = std::env::var_os("STUDIO_TEST_FFMPEG").map(PathBuf::from) else {
            eprintln!("skipping image motion preview/export test: STUDIO_TEST_FFMPEG is not set");
            return;
        };
        let Some(ffprobe) = std::env::var_os("STUDIO_TEST_FFPROBE").map(PathBuf::from) else {
            eprintln!("skipping image motion preview/export test: STUDIO_TEST_FFPROBE is not set");
            return;
        };

        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let image_path = root.path().join("motion.png");
        let generated = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=24",
                "-frames:v",
                "1",
                "-y",
            ])
            .arg(&image_path)
            .status()
            .unwrap();
        assert!(generated.success(), "could not generate test still");

        let imported = import_asset(ImportOptions {
            workspace: workspace.clone(),
            file: image_path,
            ffprobe: ffprobe.clone(),
        })
        .unwrap();
        let timeline = serde_json::json!({
            "schemaVersion": "1.0.0",
            "projectId": "018f2e40-4d28-7e31-8b63-611d49c43b0a",
            "revision": 0,
            "canvas": {"width": 800, "height": 600, "fps": {"num": 24, "den": 1}, "background": "#101820"},
            "assets": {
                imported.asset_id.clone(): {
                    "kind": "image",
                    "uri": format!("asset://{}", imported.asset_id),
                    "sha256": imported.sha256
                }
            },
            "tracks": [{
                "id": "video-main",
                "kind": "video",
                "order": 0,
                "clips": [{
                    "id": "moving-still",
                    "kind": "image",
                    "assetId": imported.asset_id,
                    "startFrame": 0,
                    "durationFrames": 48,
                    "motion": "zoom_in"
                }]
            }],
            "settings": {"audioSampleRate": 48000, "colorSpace": "rec709"}
        });
        let timeline_path = root.path().join("motion-timeline.json");
        fs::write(&timeline_path, serde_json::to_vec(&timeline).unwrap()).unwrap();

        for (preset, name, expected_dimensions) in [
            (RenderPreset::Preview, "preview", (640, 480)),
            (RenderPreset::Export, "export", (800, 600)),
        ] {
            let output = root.path().join(format!("motion-{name}.mp4"));
            let result = render_timeline_with_preset_cancellable(
                RenderOptions {
                    workspace: workspace.clone(),
                    timeline: timeline_path.clone(),
                    output,
                    ffmpeg: ffmpeg.clone(),
                    ffprobe: ffprobe.clone(),
                    font: None,
                    timeout_seconds: 120,
                },
                preset,
                None,
            )
            .unwrap();
            assert_eq!(
                (result.output_width, result.output_height),
                expected_dimensions
            );
            assert_eq!(result.output_frame_count, 48);
            assert_eq!(result.output_frame_rate, "24/1");
            assert_ne!(
                decode_edge_frame(&ffmpeg, &result.output, false),
                decode_edge_frame(&ffmpeg, &result.output, true),
                "the zoom preset should change the rendered image over time"
            );
        }
    }

    fn decode_edge_frame(ffmpeg: &Path, video: &Path, from_end: bool) -> Vec<u8> {
        let mut command = Command::new(ffmpeg);
        command.args(["-hide_banner", "-loglevel", "error"]);
        if from_end {
            command.args(["-sseof", "-0.1"]);
        }
        let output = command
            .args(["-i"])
            .arg(video)
            .args([
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not decode rendered test frame"
        );
        output.stdout
    }
}
