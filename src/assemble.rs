use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::{
    error::StudioError,
    media::{AssetKind, MAX_PROJECT_SECONDS},
    timeline::{
        AudioClip, Canvas, Clip, FrameRate, ImageClip, ImageMotion, Settings, SourceTimestamp,
        TextClip, Timeline, TimelineAsset, Track, TrackKind, VideoClip,
    },
    workspace::{AssetManifest, Workspace},
};

pub const CANVAS_PRESETS: &[(&str, u32, u32, u32, u32)] = &[
    ("landscape_720p30", 1280, 720, 30, 1),
    ("landscape_1080p30", 1920, 1080, 30, 1),
    ("vertical_1080p30", 1080, 1920, 30, 1),
    ("square_1080p30", 1080, 1080, 30, 1),
];

pub const DEFAULT_IMAGE_DURATION_SECONDS: f64 = 5.0;

#[derive(Debug, Clone)]
pub struct BootstrapRequest {
    pub asset_ids: Vec<String>,
    pub canvas_preset: Option<String>,
}

pub struct TitleOptions<'a> {
    pub text: &'a str,
    pub start_seconds: f64,
    pub duration_seconds: f64,
    pub font_size: Option<u32>,
    pub color: Option<String>,
    pub x: Option<u32>,
    pub y: Option<u32>,
}

pub fn normalize_project_name(name: Option<String>) -> Result<Option<String>> {
    let Some(name) = name else {
        return Ok(None);
    };
    if name.chars().any(char::is_control) {
        return Err(StudioError::validation(
            "name must be 1 to 120 characters and must not contain control characters",
        )
        .with_pointer("/name")
        .into());
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(StudioError::validation(
            "name must be 1 to 120 characters and must not contain control characters",
        )
        .with_pointer("/name")
        .into());
    }
    Ok(Some(name.to_owned()))
}

pub fn resolve_canvas_preset(
    preset_id: Option<&str>,
) -> Result<Option<(u32, u32, FrameRate, String)>> {
    let Some(preset_id) = preset_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if preset_id == "export" || preset_id == "preview" {
        return Err(StudioError::validation(
            "presetId on project creation is a canvas preset, not a render preset",
        )
        .with_pointer("/presetId")
        .with_expected("landscape_720p30, landscape_1080p30, vertical_1080p30, or square_1080p30")
        .with_actual(preset_id)
        .into());
    }
    let preset = CANVAS_PRESETS
        .iter()
        .find(|(id, ..)| *id == preset_id)
        .copied();
    let Some((id, width, height, num, den)) = preset else {
        return Err(
            StudioError::validation(format!("unsupported canvas preset {preset_id}"))
                .with_pointer("/presetId")
                .with_expected(
                    "landscape_720p30, landscape_1080p30, vertical_1080p30, or square_1080p30",
                )
                .with_actual(preset_id)
                .into(),
        );
    };
    Ok(Some((width, height, FrameRate { num, den }, id.to_owned())))
}

pub fn build_bootstrap_timeline(
    workspace: &Workspace,
    request: &BootstrapRequest,
) -> Result<Timeline> {
    if request.asset_ids.is_empty() || request.asset_ids.len() > 100 {
        return Err(StudioError::validation(
            "assetIds must contain between 1 and 100 imported assets",
        )
        .with_pointer("/assetIds")
        .into());
    }
    let mut assets = BTreeMap::new();
    let mut manifests = Vec::new();
    for (index, asset_id) in request.asset_ids.iter().enumerate() {
        let (_, manifest) = workspace.load_asset(asset_id).map_err(|error| {
            StudioError::new(
                "ASSET_NOT_FOUND",
                format!("asset {asset_id} is not in the workspace: {error:#}"),
            )
            .with_pointer(format!("/assetIds/{index}"))
        })?;
        let timeline_asset = timeline_asset_from_manifest(&manifest);
        assets.insert(asset_id.clone(), timeline_asset);
        manifests.push(manifest);
    }
    let opening = manifests
        .iter()
        .find(|manifest| manifest.kind != AssetKind::Audio)
        .ok_or_else(|| {
            StudioError::validation(
                "project bootstrap requires at least one video or still-image asset",
            )
            .with_pointer("/assetIds")
        })?;
    let (width, height, fps, export_preset) = if let Some((width, height, fps, preset)) =
        resolve_canvas_preset(request.canvas_preset.as_deref())?
    {
        (width, height, fps, Some(preset))
    } else {
        let video = opening.probe.video.as_ref();
        let (width, height) = fit_canvas(
            video.and_then(|stream| stream.width).unwrap_or(1280),
            video.and_then(|stream| stream.height).unwrap_or(720),
        );
        (width, height, FrameRate { num: 30, den: 1 }, None)
    };
    let max_frames = max_project_frames(fps);
    let mut video_clips = Vec::new();
    let mut audio_clips = Vec::new();
    let mut cursor = 0_u64;
    for manifest in manifests
        .iter()
        .filter(|manifest| manifest.kind != AssetKind::Audio)
    {
        let remaining = max_frames.saturating_sub(cursor);
        if remaining == 0 {
            return Err(StudioError::validation(
                "the selected assets exceed the five-minute project limit",
            )
            .with_pointer("/assetIds")
            .into());
        }
        let media_frames = if manifest.kind == AssetKind::Image {
            frames_for_seconds(DEFAULT_IMAGE_DURATION_SECONDS, fps)?
        } else {
            frames_for_media(&manifest.probe.duration, fps, u64::MAX)?
        };
        if media_frames > remaining {
            return Err(StudioError::validation(
                "the selected assets exceed the five-minute project limit",
            )
            .with_pointer("/assetIds")
            .into());
        }
        let duration_frames = media_frames;
        if manifest.kind == AssetKind::Image {
            video_clips.push(Clip::Image(ImageClip {
                id: Uuid::new_v4().to_string(),
                asset_id: manifest.asset_id.clone(),
                start_frame: cursor,
                duration_frames,
                motion: ImageMotion::Static,
                crop: None,
                transition_in: None,
            }));
        } else {
            let source_seconds =
                decimal_seconds(&manifest.probe.duration)?.min(frame_seconds(duration_frames, fps));
            video_clips.push(Clip::Video(VideoClip {
                id: Uuid::new_v4().to_string(),
                asset_id: manifest.asset_id.clone(),
                start_frame: cursor,
                duration_frames,
                source_in: source_timestamp_at_seconds(0.0)?,
                source_out: source_timestamp_at_seconds(source_seconds)?,
                transform: None,
                crop: None,
                transition_in: None,
                effects: Vec::new(),
            }));
        }
        if manifest.kind == AssetKind::Video
            && manifest.probe.has_audio
            && let Some(audio) = embedded_audio_clip(manifest, cursor, duration_frames, fps, 1.0)?
        {
            audio_clips.push(Clip::Audio(audio));
        }
        cursor = cursor.saturating_add(duration_frames);
    }
    for manifest in manifests
        .iter()
        .filter(|manifest| manifest.kind == AssetKind::Audio && manifest.probe.has_audio)
    {
        if cursor == 0 {
            break;
        }
        let seconds = manifest
            .probe
            .audio
            .as_ref()
            .and_then(|audio| audio.duration_seconds)
            .unwrap_or(manifest.probe.duration_seconds)
            .min(frame_seconds(cursor, fps));
        let duration_frames = frames_for_seconds(seconds, fps)?.min(cursor).max(1);
        audio_clips.push(Clip::Audio(AudioClip {
            id: Uuid::new_v4().to_string(),
            asset_id: manifest.asset_id.clone(),
            start_frame: 0,
            duration_frames,
            source_in: source_timestamp_at_seconds(0.0)?,
            source_out: source_timestamp_at_seconds(
                seconds.min(frame_seconds(duration_frames, fps)),
            )?,
            gain: 1.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
        }));
    }

    Ok(Timeline {
        schema_version: "1.0.0".to_owned(),
        project_id: Uuid::new_v4().to_string(),
        revision: 0,
        canvas: Canvas {
            width,
            height,
            fps,
            background: "#101820".to_owned(),
        },
        assets,
        tracks: vec![
            Track {
                id: "video-main".to_owned(),
                kind: TrackKind::Video,
                order: 0,
                enabled: true,
                locked: false,
                clips: video_clips,
            },
            Track {
                id: "audio-1".to_owned(),
                kind: TrackKind::Audio,
                order: 1,
                enabled: true,
                locked: false,
                clips: audio_clips,
            },
            Track {
                id: "audio-2".to_owned(),
                kind: TrackKind::Audio,
                order: 2,
                enabled: true,
                locked: false,
                clips: Vec::new(),
            },
            Track {
                id: "titles".to_owned(),
                kind: TrackKind::Overlay,
                order: 3,
                enabled: true,
                locked: false,
                clips: Vec::new(),
            },
        ],
        settings: Settings {
            audio_sample_rate: 48_000,
            color_space: Some("rec709".to_owned()),
            export_preset,
        },
    })
}

pub fn prepare_append(
    timeline: &Timeline,
    asset_id: &str,
    duration_seconds: Option<f64>,
    source_in_seconds: Option<f64>,
    workspace: &Workspace,
) -> Result<(VideoClip, Option<AudioClip>, TimelineAsset)> {
    let (_, manifest) = workspace.load_asset(asset_id).map_err(|error| {
        StudioError::new(
            "ASSET_NOT_FOUND",
            format!("asset {asset_id} is not in the workspace: {error:#}"),
        )
        .with_pointer("/assetId")
    })?;
    if manifest.kind != AssetKind::Video || !manifest.probe.has_video {
        return Err(StudioError::validation(format!(
            "clip.append requires a video asset; {asset_id} has no video stream"
        ))
        .with_pointer("/assetId")
        .into());
    }
    let fps = timeline.canvas.fps;
    let used = video_duration_frames(timeline);
    let remaining = max_project_frames(fps).saturating_sub(used);
    if remaining == 0 {
        return Err(StudioError::validation(
            "this project has reached the five-minute timeline limit",
        )
        .into());
    }
    let source_in_seconds = source_in_seconds.unwrap_or(0.0);
    reject_non_finite("sourceInSeconds", source_in_seconds)?;
    if source_in_seconds < 0.0 {
        return Err(
            StudioError::validation("sourceInSeconds must be zero or greater")
                .with_pointer("/sourceInSeconds")
                .into(),
        );
    }
    let available = available_seconds(&manifest.probe.duration, source_in_seconds)?;
    let requested = duration_seconds.unwrap_or(available);
    reject_non_finite("durationSeconds", requested)?;
    if requested <= 0.0 {
        return Err(
            StudioError::validation("durationSeconds must be greater than zero")
                .with_pointer("/durationSeconds")
                .into(),
        );
    }
    let clip_seconds = requested.min(available);
    let requested_frames = frames_for_seconds(clip_seconds, fps)?;
    if requested_frames > remaining {
        return Err(StudioError::validation(
            "clip.append would exceed the five-minute project limit",
        )
        .with_pointer("/durationSeconds")
        .into());
    }
    let duration_frames = requested_frames.max(1);
    let source_duration = clip_seconds.min(frame_seconds(duration_frames, fps));
    let source_in = source_timestamp_at_seconds(source_in_seconds)?;
    let source_out = source_timestamp_at_seconds(source_in_seconds + source_duration)?;
    let video = VideoClip {
        id: Uuid::new_v4().to_string(),
        asset_id: asset_id.to_owned(),
        start_frame: used,
        duration_frames,
        source_in,
        source_out,
        transform: None,
        crop: None,
        transition_in: None,
        effects: Vec::new(),
    };
    let audio = if manifest.probe.has_audio {
        embedded_audio_clip(&manifest, used, duration_frames, fps, 1.0)?
    } else {
        None
    };
    Ok((video, audio, timeline_asset_from_manifest(&manifest)))
}

pub fn prepare_image_add(
    timeline: &Timeline,
    asset_id: &str,
    duration_seconds: Option<f64>,
    workspace: &Workspace,
) -> Result<(ImageClip, TimelineAsset)> {
    let (_, manifest) = workspace.load_asset(asset_id).map_err(|error| {
        StudioError::new(
            "ASSET_NOT_FOUND",
            format!("asset {asset_id} is not in the workspace: {error:#}"),
        )
        .with_pointer("/assetId")
    })?;
    if manifest.kind != AssetKind::Image || !manifest.probe.has_still_image {
        return Err(StudioError::validation(format!(
            "image.add requires a still-image asset; {asset_id} is not an image"
        ))
        .with_pointer("/assetId")
        .into());
    }
    let duration_seconds = duration_seconds.unwrap_or(DEFAULT_IMAGE_DURATION_SECONDS);
    reject_non_finite("durationSeconds", duration_seconds)?;
    if duration_seconds <= 0.0 {
        return Err(
            StudioError::validation("durationSeconds must be greater than zero")
                .with_pointer("/durationSeconds")
                .into(),
        );
    }
    let fps = timeline.canvas.fps;
    let start_frame = video_duration_frames(timeline);
    let remaining = max_project_frames(fps).saturating_sub(start_frame);
    if remaining == 0 {
        return Err(StudioError::validation(
            "this project has reached the five-minute timeline limit",
        )
        .into());
    }
    let duration_frames = frames_for_seconds(duration_seconds, fps)?;
    if duration_frames > remaining {
        return Err(StudioError::validation(
            "image.add would exceed the five-minute project limit",
        )
        .with_pointer("/durationSeconds")
        .into());
    }
    Ok((
        ImageClip {
            id: Uuid::new_v4().to_string(),
            asset_id: asset_id.to_owned(),
            start_frame,
            duration_frames,
            motion: ImageMotion::Static,
            crop: None,
            transition_in: None,
        },
        timeline_asset_from_manifest(&manifest),
    ))
}

pub fn prepare_audio(
    timeline: &Timeline,
    asset_id: &str,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    gain: Option<f64>,
    workspace: &Workspace,
) -> Result<AudioClip> {
    let (_, manifest) = workspace.load_asset(asset_id).map_err(|error| {
        StudioError::new(
            "ASSET_NOT_FOUND",
            format!("asset {asset_id} is not in the workspace: {error:#}"),
        )
        .with_pointer("/assetId")
    })?;
    if !manifest.probe.has_audio {
        return Err(StudioError::validation(format!(
            "audio.add requires an audio stream; {asset_id} has none"
        ))
        .with_pointer("/assetId")
        .into());
    }
    let fps = timeline.canvas.fps;
    let total = video_duration_frames(timeline);
    if total == 0 {
        return Err(StudioError::validation("add a visual clip before placing audio").into());
    }
    reject_non_finite("startSeconds", start_seconds)?;
    if start_seconds < 0.0 {
        return Err(
            StudioError::validation("startSeconds must be zero or greater")
                .with_pointer("/startSeconds")
                .into(),
        );
    }
    let start_frame = frames_for_seconds(start_seconds, fps)?;
    if start_frame >= total {
        return Err(
            StudioError::validation("startSeconds must be before the end of the video")
                .with_pointer("/startSeconds")
                .into(),
        );
    }
    let remaining = total.saturating_sub(start_frame).max(1);
    let available = available_seconds(&manifest.probe.duration, 0.0)?;
    let requested = duration_seconds.unwrap_or(available);
    reject_non_finite("durationSeconds", requested)?;
    let source_duration = requested.min(available);
    if source_duration <= 0.0 {
        return Err(
            StudioError::validation("durationSeconds must be greater than zero")
                .with_pointer("/durationSeconds")
                .into(),
        );
    }
    let duration_frames = frames_for_seconds(source_duration, fps)?
        .min(remaining)
        .max(1);
    let source_duration = source_duration.min(frame_seconds(duration_frames, fps));
    let gain = gain.unwrap_or(1.0);
    if !(0.0..=4.0).contains(&gain) {
        return Err(StudioError::validation("gain must be between 0 and 4")
            .with_pointer("/gain")
            .into());
    }
    Ok(AudioClip {
        id: Uuid::new_v4().to_string(),
        asset_id: asset_id.to_owned(),
        start_frame,
        duration_frames,
        source_in: source_timestamp_at_seconds(0.0)?,
        source_out: source_timestamp_at_seconds(source_duration)?,
        gain,
        fade_in_frames: 0,
        fade_out_frames: 0,
    })
}

pub fn prepare_title(timeline: &Timeline, options: TitleOptions<'_>) -> Result<TextClip> {
    let TitleOptions {
        text,
        start_seconds,
        duration_seconds,
        font_size,
        color,
        x,
        y,
    } = options;
    if text.is_empty() || text.len() > 4096 {
        return Err(
            StudioError::validation("title text must contain 1 to 4096 bytes")
                .with_pointer("/text")
                .into(),
        );
    }
    reject_non_finite("startSeconds", start_seconds)?;
    reject_non_finite("durationSeconds", duration_seconds)?;
    if start_seconds < 0.0 || duration_seconds <= 0.0 {
        return Err(StudioError::validation(
            "title startSeconds must be zero or greater and durationSeconds must be greater than zero",
        )
        .into());
    }
    let fps = timeline.canvas.fps;
    let total = video_duration_frames(timeline);
    if total == 0 {
        return Err(StudioError::validation("add a visual clip before adding a title").into());
    }
    let start_frame = frames_for_seconds(start_seconds, fps)?;
    if start_frame >= total {
        return Err(
            StudioError::validation("startSeconds must be before the end of the video")
                .with_pointer("/startSeconds")
                .into(),
        );
    }
    let duration_frames = frames_for_seconds(duration_seconds, fps)?;
    if duration_frames > total.saturating_sub(start_frame) {
        return Err(
            StudioError::validation("title duration extends past the end of the video")
                .with_pointer("/durationSeconds")
                .into(),
        );
    }
    Ok(TextClip {
        id: Uuid::new_v4().to_string(),
        start_frame,
        duration_frames,
        text: text.to_owned(),
        font_size: font_size.unwrap_or(48),
        color: color.unwrap_or_else(|| "#FFFFFF".to_owned()),
        x: x.unwrap_or(0),
        y: y.unwrap_or(80),
    })
}

pub fn timeline_has_text(timeline: &Timeline) -> bool {
    timeline
        .tracks
        .iter()
        .any(|track| track.clips.iter().any(|clip| matches!(clip, Clip::Text(_))))
}

pub fn video_duration_frames(timeline: &Timeline) -> u64 {
    timeline
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Video)
        .map(|track| {
            let duration_frames = track.clips.iter().map(Clip::duration_frames).sum::<u64>();
            let transition_frames = track
                .clips
                .iter()
                .map(Clip::transition_in_frames)
                .sum::<u64>();
            duration_frames.saturating_sub(transition_frames)
        })
        .unwrap_or(0)
}

pub fn source_timestamp_at_seconds(seconds: f64) -> Result<SourceTimestamp> {
    reject_non_finite("seconds", seconds)?;
    if seconds < 0.0 {
        bail!("source time must be zero or greater");
    }
    let ticks = (seconds * 1_000_000.0).round();
    if ticks > i64::MAX as f64 {
        bail!("source time is out of range");
    }
    Ok(SourceTimestamp {
        ticks: ticks as i64,
        timebase_num: 1,
        timebase_den: 1_000_000,
    })
}

fn embedded_audio_clip(
    manifest: &AssetManifest,
    start_frame: u64,
    video_frames: u64,
    fps: FrameRate,
    gain: f64,
) -> Result<Option<AudioClip>> {
    let audio_seconds = manifest
        .probe
        .audio
        .as_ref()
        .and_then(|audio| audio.duration_seconds)
        .unwrap_or(manifest.probe.duration_seconds);
    if audio_seconds <= 0.0 {
        return Ok(None);
    }
    let duration_frames = frames_for_seconds(audio_seconds, fps)?
        .min(video_frames)
        .max(1);
    let source_seconds = audio_seconds.min(frame_seconds(duration_frames, fps));
    Ok(Some(AudioClip {
        id: Uuid::new_v4().to_string(),
        asset_id: manifest.asset_id.clone(),
        start_frame,
        duration_frames,
        source_in: source_timestamp_at_seconds(0.0)?,
        source_out: source_timestamp_at_seconds(source_seconds)?,
        gain,
        fade_in_frames: 0,
        fade_out_frames: 0,
    }))
}

fn timeline_asset_from_manifest(manifest: &AssetManifest) -> TimelineAsset {
    TimelineAsset {
        kind: manifest.kind,
        uri: format!("asset://{}", manifest.asset_id),
        sha256: manifest.sha256.clone(),
    }
}

fn fit_canvas(width: u32, height: u32) -> (u32, u32) {
    let width = width.max(2) as f64;
    let height = height.max(2) as f64;
    let scale = 1.0_f64
        .min(1920.0 / width)
        .min(1920.0 / height)
        .min((2_073_600.0 / (width * height)).sqrt());
    (
        ((width * scale) / 2.0).floor().max(1.0) as u32 * 2,
        ((height * scale) / 2.0).floor().max(1.0) as u32 * 2,
    )
}

fn max_project_frames(fps: FrameRate) -> u64 {
    (MAX_PROJECT_SECONDS * f64::from(fps.num) / f64::from(fps.den)) as u64
}

fn frames_for_media(duration: &str, fps: FrameRate, max_frames: u64) -> Result<u64> {
    let seconds = decimal_seconds(duration).context("asset duration is not a usable decimal")?;
    if seconds <= 0.0 {
        return Err(StudioError::validation("imported media has no usable duration").into());
    }
    let frames = frames_for_seconds(seconds, fps)?.min(max_frames).max(1);
    Ok(frames)
}

fn frames_for_seconds(seconds: f64, fps: FrameRate) -> Result<u64> {
    reject_non_finite("seconds", seconds)?;
    let frames = (seconds * f64::from(fps.num) / f64::from(fps.den)).round();
    if frames < 1.0 {
        return Ok(1);
    }
    if frames > u64::MAX as f64 {
        bail!("time is out of range");
    }
    Ok(frames as u64)
}

fn frame_seconds(frames: u64, fps: FrameRate) -> f64 {
    frames as f64 * f64::from(fps.den) / f64::from(fps.num)
}

fn available_seconds(duration: &str, source_in_seconds: f64) -> Result<f64> {
    let seconds = decimal_seconds(duration).context("asset duration is not a usable decimal")?;
    Ok((seconds - source_in_seconds).max(0.0))
}

fn decimal_seconds(value: &str) -> Result<f64> {
    let value = value.trim();
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_digit() || byte == b'.'))
    {
        bail!("invalid media duration");
    }
    value.parse::<f64>().context("invalid media duration")
}

fn reject_non_finite(field: &str, value: f64) -> Result<()> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(
            StudioError::validation(format!("{field} must be a finite number"))
                .with_pointer(format!("/{field}"))
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_preset_is_not_a_canvas_preset() {
        let error = resolve_canvas_preset(Some("export")).unwrap_err();
        let studio = crate::error::find_studio_error(&error).unwrap();
        assert_eq!(studio.code, "VALIDATION_FAILED");
        assert_eq!(studio.pointer.as_deref(), Some("/presetId"));
    }

    #[test]
    fn project_names_are_trimmed_and_bounded_by_unicode_characters() {
        assert_eq!(
            normalize_project_name(Some("  新しい作品  ".to_owned())).unwrap(),
            Some("新しい作品".to_owned())
        );
        assert!(normalize_project_name(Some("bad\nname".to_owned())).is_err());
        assert!(normalize_project_name(Some("x".repeat(121))).is_err());
    }
}
