use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail};
use jsonschema::validator_for;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    media::{AssetKind, MAX_IMPORT_BYTES, MAX_PROJECT_SECONDS},
    workspace::{AssetManifest, Workspace, validate_asset_id},
};

const MAX_TIMELINE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ASSETS: usize = 100;
const MAX_TRACKS: usize = 32;
const MAX_CLIPS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Timeline {
    pub schema_version: String,
    pub project_id: String,
    pub revision: u64,
    pub canvas: Canvas,
    pub assets: BTreeMap<String, TimelineAsset>,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub settings: Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub background: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimelineAsset {
    pub kind: AssetKind,
    pub uri: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Track {
    pub id: String,
    pub kind: TrackKind,
    pub order: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
    Overlay,
    Caption,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Clip {
    Video(VideoClip),
    Audio(AudioClip),
    Text(TextClip),
    Caption(CaptionClip),
    Image(ImageClip),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VideoClip {
    pub id: String,
    pub asset_id: String,
    pub start_frame: u64,
    pub duration_frames: u64,
    pub source_in: SourceTimestamp,
    pub source_out: SourceTimestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    #[serde(default)]
    pub effects: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioClip {
    pub id: String,
    pub asset_id: String,
    pub start_frame: u64,
    pub duration_frames: u64,
    pub source_in: SourceTimestamp,
    pub source_out: SourceTimestamp,
    #[serde(default = "default_gain")]
    pub gain: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextClip {
    pub id: String,
    pub start_frame: u64,
    pub duration_frames: u64,
    pub text: String,
    #[serde(default = "default_font_size")]
    pub font_size: u32,
    #[serde(default = "default_text_color")]
    pub color: String,
    #[serde(default)]
    pub x: u32,
    #[serde(default = "default_text_y")]
    pub y: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptionClip {
    pub id: String,
    pub start_frame: u64,
    pub duration_frames: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageClip {
    pub id: String,
    pub asset_id: String,
    pub start_frame: u64,
    pub duration_frames: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceTimestamp {
    pub ticks: i64,
    #[serde(rename = "timebase_num")]
    pub timebase_num: i64,
    #[serde(rename = "timebase_den")]
    pub timebase_den: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Transform {
    pub x: f64,
    pub y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub rotation_deg: f64,
    pub opacity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_audio_rate")]
    pub audio_sample_rate: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_preset: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio_sample_rate: default_audio_rate(),
            color_space: None,
            export_preset: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedAsset {
    pub path: std::path::PathBuf,
    pub manifest: AssetManifest,
}

pub struct ValidatedTimeline {
    pub timeline: Timeline,
    pub assets: BTreeMap<String, ResolvedAsset>,
    pub total_frames: u64,
    pub video_track_index: usize,
    pub audio_clips: Vec<(usize, usize)>,
    pub text_clips: Vec<(usize, usize)>,
}

pub fn load_and_validate(path: &Path, workspace: &Workspace) -> Result<ValidatedTimeline> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("could not inspect timeline {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_TIMELINE_BYTES {
        bail!("timeline must be a file no larger than 2 MiB");
    }
    let bytes =
        fs::read(path).with_context(|| format!("could not read timeline {}", path.display()))?;
    validate_json_bytes(&bytes, workspace)
}

pub fn validate_json_bytes(bytes: &[u8], workspace: &Workspace) -> Result<ValidatedTimeline> {
    if bytes.len() as u64 > MAX_TIMELINE_BYTES {
        bail!("timeline must be no larger than 2 MiB");
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let document: Value = serde_json::from_slice(bytes)
        .context("timeline JSON is invalid or contains unsupported fields")?;
    validate_json_document(document, workspace)
}

pub fn validate_json_document(document: Value, workspace: &Workspace) -> Result<ValidatedTimeline> {
    validate_json_schema(&document)?;
    let timeline: Timeline = serde_json::from_value(document)
        .context("timeline JSON does not match the typed timeline model")?;
    validate(timeline, workspace)
}

fn validate_json_schema(document: &Value) -> Result<()> {
    let schema: Value = serde_json::from_str(include_str!("../schemas/timeline-1.0.0.schema.json"))
        .context("bundled timeline JSON Schema is invalid")?;
    let validator =
        validator_for(&schema).context("could not compile bundled timeline JSON Schema")?;
    if let Some(error) = validator.iter_errors(document).next() {
        bail!("timeline JSON Schema validation failed: {error}");
    }
    Ok(())
}

pub fn validate(timeline: Timeline, workspace: &Workspace) -> Result<ValidatedTimeline> {
    if timeline.schema_version != "1.0.0" {
        bail!("unsupported timeline schemaVersion; expected 1.0.0");
    }
    Uuid::parse_str(&timeline.project_id).context("projectId must be a UUID")?;
    validate_canvas(&timeline.canvas)?;
    if timeline.assets.is_empty() || timeline.assets.len() > MAX_ASSETS {
        bail!("timeline must reference between 1 and 100 imported assets");
    }
    if timeline.tracks.is_empty() || timeline.tracks.len() > MAX_TRACKS {
        bail!("timeline must contain between 1 and 32 tracks");
    }
    if timeline.settings.audio_sample_rate != 48_000 {
        bail!("the first local renderer supports 48000 Hz project audio only");
    }
    if let Some(preset) = timeline.settings.export_preset.as_deref() {
        let expected = match preset {
            "landscape_720p30" => (1280, 720, 30, 1),
            "landscape_1080p30" => (1920, 1080, 30, 1),
            "vertical_1080p30" => (1080, 1920, 30, 1),
            "square_1080p30" => (1080, 1080, 30, 1),
            _ => bail!("unsupported exportPreset: {preset}"),
        };
        if (
            timeline.canvas.width,
            timeline.canvas.height,
            timeline.canvas.fps.num,
            timeline.canvas.fps.den,
        ) != expected
        {
            bail!("exportPreset does not match the timeline canvas and frame rate");
        }
    }
    if timeline
        .settings
        .color_space
        .as_deref()
        .is_some_and(|value| value != "rec709")
    {
        bail!("the first local renderer supports Rec. 709 only");
    }

    let assets = resolve_assets(&timeline.assets, workspace)?;
    let mut track_ids = HashSet::new();
    let mut orders = HashSet::new();
    let mut clip_ids = HashSet::new();
    let mut clip_count = 0_usize;
    let mut video_tracks = Vec::new();
    let mut audio_clips = Vec::new();
    let mut text_clips = Vec::new();

    for (track_index, track) in timeline.tracks.iter().enumerate() {
        if track.id.trim().is_empty() || !track_ids.insert(track.id.as_str()) {
            bail!("track IDs must be nonempty and unique");
        }
        if !orders.insert(track.order) {
            bail!("track order values must be unique");
        }
        if !track.enabled {
            continue;
        }
        if track.kind == TrackKind::Caption {
            bail!("caption tracks are not supported by the first renderer");
        }
        if track.kind == TrackKind::Video {
            video_tracks.push(track_index);
        }
        for clip in &track.clips {
            clip_count += 1;
            if clip_count > MAX_CLIPS {
                bail!("timeline contains more than 500 clips");
            }
            let id = clip.id();
            if id.trim().is_empty() || !clip_ids.insert(id) {
                bail!("clip IDs must be nonempty and unique across the timeline");
            }
            match (track.kind, clip) {
                (TrackKind::Video, Clip::Video(video)) => {
                    validate_clip_range(video.start_frame, video.duration_frames, &timeline)?;
                    validate_video_clip(video, &assets, &timeline)?;
                }
                (TrackKind::Audio, Clip::Audio(audio)) => {
                    validate_clip_range(audio.start_frame, audio.duration_frames, &timeline)?;
                    validate_audio_clip(audio, &assets, &timeline)?;
                }
                (TrackKind::Overlay, Clip::Text(text)) => {
                    validate_clip_range(text.start_frame, text.duration_frames, &timeline)?;
                    validate_text_clip(text, &timeline)?;
                }
                (_, Clip::Caption(_)) => {
                    bail!("caption clips are not supported in the first build")
                }
                (_, Clip::Image(_)) => bail!("image clips are not supported in the first build"),
                _ => bail!("clip kind does not match its track kind"),
            }
        }
    }

    if video_tracks.len() != 1 {
        bail!("the first renderer requires exactly one enabled video track");
    }
    let video_track_index = video_tracks[0];
    let video_track = &timeline.tracks[video_track_index];
    if video_track.clips.is_empty() {
        bail!("video track must contain at least one clip");
    }
    let mut ordered_video: Vec<&VideoClip> = video_track
        .clips
        .iter()
        .filter_map(|clip| match clip {
            Clip::Video(video) => Some(video),
            _ => None,
        })
        .collect();
    ordered_video.sort_by_key(|clip| clip.start_frame);
    let mut cursor = 0_u64;
    for clip in &ordered_video {
        if clip.start_frame != cursor {
            bail!("video clips must be contiguous, non-overlapping and start at frame 0");
        }
        cursor = cursor
            .checked_add(clip.duration_frames)
            .ok_or_else(|| anyhow::anyhow!("timeline frame count overflow"))?;
    }
    let total_frames = cursor;

    for (track_index, track) in timeline.tracks.iter().enumerate() {
        if !track.enabled {
            continue;
        }
        for (clip_index, clip) in track.clips.iter().enumerate() {
            match clip {
                Clip::Audio(audio) => {
                    let end = audio.start_frame + audio.duration_frames;
                    if end > total_frames {
                        bail!("audio clip {} extends past the video timeline", audio.id);
                    }
                    audio_clips.push((track_index, clip_index));
                }
                Clip::Text(text) => {
                    let end = text.start_frame + text.duration_frames;
                    if end > total_frames {
                        bail!("text clip {} extends past the video timeline", text.id);
                    }
                    text_clips.push((track_index, clip_index));
                }
                _ => {}
            }
        }
    }
    audio_clips.sort_by_key(|(track_index, clip_index)| {
        let track = &timeline.tracks[*track_index];
        let clip = &track.clips[*clip_index];
        (track.order, clip.start_frame(), *clip_index)
    });
    text_clips.sort_by_key(|(track_index, clip_index)| {
        let track = &timeline.tracks[*track_index];
        let clip = &track.clips[*clip_index];
        (track.order, clip.start_frame(), *clip_index)
    });

    Ok(ValidatedTimeline {
        timeline,
        assets,
        total_frames,
        video_track_index,
        audio_clips,
        text_clips,
    })
}

impl Clip {
    fn id(&self) -> &str {
        match self {
            Clip::Video(value) => &value.id,
            Clip::Audio(value) => &value.id,
            Clip::Text(value) => &value.id,
            Clip::Caption(value) => &value.id,
            Clip::Image(value) => &value.id,
        }
    }

    pub fn start_frame(&self) -> u64 {
        match self {
            Clip::Video(value) => value.start_frame,
            Clip::Audio(value) => value.start_frame,
            Clip::Text(value) => value.start_frame,
            Clip::Caption(value) => value.start_frame,
            Clip::Image(value) => value.start_frame,
        }
    }

    pub fn duration_frames(&self) -> u64 {
        match self {
            Clip::Video(value) => value.duration_frames,
            Clip::Audio(value) => value.duration_frames,
            Clip::Text(value) => value.duration_frames,
            Clip::Caption(value) => value.duration_frames,
            Clip::Image(value) => value.duration_frames,
        }
    }
}

pub fn frame_boundary_sample(frame: u64, fps: FrameRate, sample_rate: u32) -> Result<u64> {
    let numerator = i128::from(frame)
        .checked_mul(i128::from(sample_rate))
        .and_then(|value| value.checked_mul(i128::from(fps.den)))
        .ok_or_else(|| anyhow::anyhow!("audio sample position overflow"))?;
    let samples = numerator / i128::from(fps.num);
    u64::try_from(samples).context("audio sample position is out of range")
}

fn validate_canvas(canvas: &Canvas) -> Result<()> {
    if canvas.width == 0
        || canvas.height == 0
        || canvas.width > 1920
        || canvas.height > 1920
        || canvas.width * canvas.height > 2_073_600
        || !canvas.width.is_multiple_of(2)
        || !canvas.height.is_multiple_of(2)
    {
        bail!("canvas must be even-sized and no larger than 1920x1920 / 1080p");
    }
    if !is_hex_color(&canvas.background) {
        bail!("canvas.background must be a #RRGGBB color");
    }
    if canvas.fps.num == 0
        || canvas.fps.den == 0
        || gcd(canvas.fps.num, canvas.fps.den) != 1
        || !matches!(
            (canvas.fps.num, canvas.fps.den),
            (24, 1) | (25, 1) | (30, 1) | (30_000, 1_001)
        )
    {
        bail!("supported frame rates are 24, 25, 30 and 30000/1001 fps");
    }
    Ok(())
}

fn validate_clip_range(start: u64, duration: u64, timeline: &Timeline) -> Result<()> {
    if duration == 0 {
        bail!("clip durationFrames must be greater than zero");
    }
    let end = start
        .checked_add(duration)
        .ok_or_else(|| anyhow::anyhow!("clip frame range overflow"))?;
    let max_frames = (MAX_PROJECT_SECONDS * f64::from(timeline.canvas.fps.num)
        / f64::from(timeline.canvas.fps.den)) as u64;
    if end > max_frames {
        bail!("project duration exceeds the five-minute limit");
    }
    Ok(())
}

fn validate_video_clip(
    clip: &VideoClip,
    assets: &BTreeMap<String, ResolvedAsset>,
    timeline: &Timeline,
) -> Result<()> {
    let asset = assets
        .get(&clip.asset_id)
        .ok_or_else(|| anyhow::anyhow!("video clip {} references a missing asset", clip.id))?;
    if !asset.manifest.probe.has_video {
        bail!(
            "video clip {} references media without a video stream",
            clip.id
        );
    }
    if !clip.effects.is_empty() {
        bail!("effects are not supported by the first renderer");
    }
    if let Some(transform) = &clip.transform
        && (transform.x != 0.0
            || transform.y != 0.0
            || transform.scale_x != 1.0
            || transform.scale_y != 1.0
            || transform.rotation_deg != 0.0
            || transform.opacity != 1.0)
    {
        bail!("non-default video transforms are not supported by the first renderer");
    }
    validate_source_range(
        &clip.source_in,
        &clip.source_out,
        clip.duration_frames,
        &asset.manifest,
        timeline.canvas.fps,
    )
}

fn validate_audio_clip(
    clip: &AudioClip,
    assets: &BTreeMap<String, ResolvedAsset>,
    timeline: &Timeline,
) -> Result<()> {
    let asset = assets
        .get(&clip.asset_id)
        .ok_or_else(|| anyhow::anyhow!("audio clip {} references a missing asset", clip.id))?;
    if !asset.manifest.probe.has_audio {
        bail!(
            "audio clip {} references media without an audio stream",
            clip.id
        );
    }
    if !clip.gain.is_finite() || !(0.0..=4.0).contains(&clip.gain) {
        bail!("audio gain must be between 0 and 4");
    }
    validate_source_range(
        &clip.source_in,
        &clip.source_out,
        clip.duration_frames,
        &asset.manifest,
        timeline.canvas.fps,
    )
}

fn validate_text_clip(clip: &TextClip, timeline: &Timeline) -> Result<()> {
    if clip.text.trim().is_empty() || clip.text.len() > 4096 {
        bail!("text clips must contain between 1 and 4096 UTF-8 bytes");
    }
    if clip.font_size == 0 || clip.font_size > 256 {
        bail!("text fontSize must be between 1 and 256");
    }
    if !is_hex_color(&clip.color) {
        bail!("text color must be a #RRGGBB value");
    }
    if clip.x >= timeline.canvas.width || clip.y >= timeline.canvas.height {
        bail!("text x/y position must lie within the canvas");
    }
    Ok(())
}

fn validate_source_range(
    source_in: &SourceTimestamp,
    source_out: &SourceTimestamp,
    duration_frames: u64,
    asset: &AssetManifest,
    fps: FrameRate,
) -> Result<()> {
    let start = timestamp_fraction(source_in)?;
    let end = timestamp_fraction(source_out)?;
    if compare(start, end)? != std::cmp::Ordering::Less {
        bail!("sourceOut must be greater than sourceIn");
    }
    let media_duration = decimal_fraction(&asset.probe.duration)?;
    if compare(end, media_duration)? == std::cmp::Ordering::Greater {
        bail!("source range exceeds the probed media duration");
    }

    let source_duration = subtract(end, start)?;
    let project_duration = Fraction::new(
        i128::from(duration_frames) * i128::from(fps.den),
        i128::from(fps.num),
    )?;
    let difference = absolute(subtract(source_duration, project_duration)?);
    let half_frame = Fraction::new(i128::from(fps.den), i128::from(fps.num) * 2)?;
    if compare(difference, half_frame)? == std::cmp::Ordering::Greater {
        bail!("source duration and timeline duration differ by more than half a frame");
    }
    Ok(())
}

fn resolve_assets(
    timeline_assets: &BTreeMap<String, TimelineAsset>,
    workspace: &Workspace,
) -> Result<BTreeMap<String, ResolvedAsset>> {
    let mut resolved = BTreeMap::new();
    for (id, asset) in timeline_assets {
        validate_asset_id(id)?;
        if asset.uri != format!("asset://{id}") {
            bail!("asset URI must match its asset UUID");
        }
        if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("asset sha256 must be a 64-character hex digest");
        }
        let (path, manifest) = workspace.load_asset(id)?;
        if manifest.kind != asset.kind || !manifest.sha256.eq_ignore_ascii_case(&asset.sha256) {
            bail!("timeline asset metadata does not match imported asset {id}");
        }
        if manifest.bytes > MAX_IMPORT_BYTES {
            bail!("imported asset exceeds the current input-size limit");
        }
        let actual_hash = hash_file(&path)?;
        if !actual_hash.eq_ignore_ascii_case(&manifest.sha256) {
            bail!("imported asset {id} changed after import");
        }
        resolved.insert(id.clone(), ResolvedAsset { path, manifest });
    }
    Ok(resolved)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).context("could not read imported media")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read =
            std::io::Read::read(&mut file, &mut buffer).context("could not hash imported media")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Clone, Copy)]
struct Fraction {
    numerator: i128,
    denominator: i128,
}

impl Fraction {
    fn new(numerator: i128, denominator: i128) -> Result<Self> {
        if denominator <= 0 {
            bail!("rational time denominator must be positive");
        }
        let divisor = gcd_i128(numerator.unsigned_abs() as i128, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }
}

fn timestamp_fraction(timestamp: &SourceTimestamp) -> Result<Fraction> {
    if timestamp.ticks < 0 || timestamp.timebase_num <= 0 || timestamp.timebase_den <= 0 {
        bail!("source timestamps require nonnegative ticks and positive time bases");
    }
    if timestamp.ticks > 1_000_000_000_000_000
        || timestamp.timebase_num > 1_000_000
        || timestamp.timebase_den > 1_000_000_000
    {
        bail!("source timestamp exceeds the supported numeric bounds");
    }
    Fraction::new(
        i128::from(timestamp.ticks) * i128::from(timestamp.timebase_num),
        i128::from(timestamp.timebase_den),
    )
}

fn decimal_fraction(value: &str) -> Result<Fraction> {
    let value = value.trim();
    if let Some((whole, decimal)) = value.split_once('.') {
        if whole.is_empty()
            || decimal.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !decimal.bytes().all(|byte| byte.is_ascii_digit())
            || decimal.len() > 9
        {
            bail!("invalid exact decimal duration in asset manifest");
        }
        let denominator = 10_i128.pow(decimal.len() as u32);
        let whole = whole.parse::<i128>()?;
        let fractional = decimal.parse::<i128>()?;
        Fraction::new(whole * denominator + fractional, denominator)
    } else {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("invalid exact decimal duration in asset manifest");
        }
        Fraction::new(value.parse()?, 1)
    }
}

fn add(left: Fraction, right: Fraction) -> Result<Fraction> {
    Fraction::new(
        left.numerator * right.denominator + right.numerator * left.denominator,
        left.denominator * right.denominator,
    )
}

fn subtract(left: Fraction, right: Fraction) -> Result<Fraction> {
    add(left, Fraction::new(-right.numerator, right.denominator)?)
}

fn absolute(value: Fraction) -> Fraction {
    Fraction {
        numerator: value.numerator.abs(),
        denominator: value.denominator,
    }
}

fn compare(left: Fraction, right: Fraction) -> Result<std::cmp::Ordering> {
    let lhs = left
        .numerator
        .checked_mul(right.denominator)
        .ok_or_else(|| anyhow::anyhow!("rational comparison overflow"))?;
    let rhs = right
        .numerator
        .checked_mul(left.denominator)
        .ok_or_else(|| anyhow::anyhow!("rational comparison overflow"))?;
    Ok(lhs.cmp(&rhs))
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn gcd_i128(mut left: i128, mut right: i128) -> i128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn default_true() -> bool {
    true
}

fn default_gain() -> f64 {
    1.0
}

fn default_font_size() -> u32 {
    48
}

fn default_text_color() -> String {
    "#FFFFFF".to_owned()
}

fn default_text_y() -> u32 {
    80
}

fn default_audio_rate() -> u32 {
    48_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_to_sample_mapping_uses_floor_and_rational_fps() {
        assert_eq!(
            frame_boundary_sample(30, FrameRate { num: 30, den: 1 }, 48_000).unwrap(),
            48_000
        );
        assert_eq!(
            frame_boundary_sample(
                30_000,
                FrameRate {
                    num: 30_000,
                    den: 1_001
                },
                48_000
            )
            .unwrap(),
            48_048_000
        );
        assert_eq!(
            frame_boundary_sample(1, FrameRate { num: 30, den: 1 }, 48_000).unwrap(),
            1_600
        );
    }

    #[test]
    fn decimals_are_compared_exactly() {
        let one_and_half = decimal_fraction("1.500000").unwrap();
        let three_halves = Fraction::new(3, 2).unwrap();
        assert_eq!(
            compare(one_and_half, three_halves).unwrap(),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn source_duration_must_fit_both_media_and_project_frames() {
        let asset = AssetManifest {
            schema_version: 1,
            asset_id: "018f2e40-4d28-7e31-8b63-611d49c43b0b".to_owned(),
            sha256: "0".repeat(64),
            bytes: 1,
            kind: AssetKind::Video,
            probe: crate::media::MediaProbe {
                format_name: "mp4".to_owned(),
                duration: "3.000000".to_owned(),
                duration_seconds: 3.0,
                has_video: true,
                has_audio: false,
                video: None,
                audio: None,
            },
        };
        let source_in = SourceTimestamp {
            ticks: 0,
            timebase_num: 1,
            timebase_den: 90_000,
        };
        let source_out = SourceTimestamp {
            ticks: 270_000,
            timebase_num: 1,
            timebase_den: 90_000,
        };
        assert!(
            validate_source_range(
                &source_in,
                &source_out,
                90,
                &asset,
                FrameRate { num: 30, den: 1 }
            )
            .is_ok()
        );
        assert!(
            validate_source_range(
                &source_in,
                &source_out,
                91,
                &asset,
                FrameRate { num: 30, den: 1 }
            )
            .is_err()
        );
        let beyond_media = SourceTimestamp {
            ticks: 270_001,
            ..source_out
        };
        assert!(
            validate_source_range(
                &source_in,
                &beyond_media,
                90,
                &asset,
                FrameRate { num: 30, den: 1 }
            )
            .is_err()
        );
    }

    #[test]
    fn bundled_schema_accepts_a_valid_timeline_and_rejects_unknown_fields() {
        let document: Value =
            serde_json::from_str(include_str!("../tests/fixtures/timeline-schema-valid.json"))
                .unwrap();
        validate_json_schema(&document).unwrap();

        let invalid: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/timeline-schema-invalid.json"
        ))
        .unwrap();
        assert!(validate_json_schema(&invalid).is_err());
    }
}
