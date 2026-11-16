use std::{collections::HashMap, path::Path, process::Command, sync::Mutex, time::Duration};

use serde::Serialize;
use serde_json::{Value, json};

use crate::{assemble::CANVAS_PRESETS, process::run_bounded_cancellable};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub schema_version: &'static str,
    pub timeline_schema_url: &'static str,
    pub openapi_url: &'static str,
    pub operations: Vec<OperationCapability>,
    pub canvas_presets: Vec<CanvasPresetCapability>,
    pub render_presets: Vec<RenderPresetCapability>,
    pub limits: Limits,
    pub font_configured: bool,
    pub ffmpeg: ToolStatus,
    pub ffprobe: ToolStatus,
    pub notes: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationCapability {
    pub r#type: &'static str,
    pub description: &'static str,
    pub required: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanvasPresetCapability {
    pub id: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderPresetCapability {
    pub id: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_project_seconds: u32,
    pub max_import_bytes: u64,
    pub max_upload_sessions: u32,
    pub max_batch_operations: u32,
    pub max_explicit_render_retries: u32,
    pub supported_fps: Vec<&'static str>,
    pub supported_clip_kinds: Vec<&'static str>,
    pub rejected_clip_kinds: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub available: bool,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

pub fn capabilities(font_configured: bool, ffmpeg: &Path, ffprobe: &Path) -> Capabilities {
    Capabilities {
        schema_version: "1.0.0",
        timeline_schema_url: "/v1/schemas/timeline",
        openapi_url: "/v1/openapi.json",
        operations: operation_catalog(),
        canvas_presets: CANVAS_PRESETS
            .iter()
            .map(|(id, width, height, num, den)| CanvasPresetCapability {
                id,
                width: *width,
                height: *height,
                fps: if *den == 1 {
                    num.to_string()
                } else {
                    format!("{num}/{den}")
                },
            })
            .collect(),
        render_presets: vec![
            RenderPresetCapability {
                id: "preview",
                description: "Downscales to a 640-pixel edge without upscaling. Use this before export.",
            },
            RenderPresetCapability {
                id: "export",
                description: "Full canvas H.264/AAC MP4. Pin a revision; export does not edit the timeline.",
            },
        ],
        limits: Limits {
            max_project_seconds: 300,
            max_import_bytes: 2 * 1024 * 1024 * 1024,
            max_upload_sessions: 4,
            max_batch_operations: 100,
            max_explicit_render_retries: 2,
            supported_fps: vec!["24", "25", "30", "30000/1001"],
            supported_clip_kinds: vec!["video", "image", "audio", "text"],
            rejected_clip_kinds: vec!["caption"],
        },
        font_configured,
        ffmpeg: probe_tool(ffmpeg),
        ffprobe: probe_tool(ffprobe),
        notes: vec![
            "Create a project with POST /v1/projects {assetIds, presetId?, name?}. presetId is a canvas preset, not export or preview.",
            "Prefer clip.append, image.add, audio.add, and title.add. The server converts seconds to frames and rational source timestamps.",
            "Times stored in a timeline stay integer frames. Source points stay {ticks, timebase_num, timebase_den}.",
            "A text timeline cannot be rendered unless the server was started with --font.",
            "On 409 REVISION_CONFLICT, read error.details.currentRevision and submit a new idempotency key.",
            "Same idempotency key and same body replays. Same key and a different body returns IDEMPOTENCY_CONFLICT.",
            "Inspect GET /v1/jobs/{id}/inspection instead of watching the MP4. Download /v1/assets/{outputAssetId}/content only after qc.passed.",
            "Render job status includes phase and progressPercent; encoding progress is estimated from FFmpeg frame reports.",
            "Still images are imported as assets and become five-second visual clips when bootstrapping a project; image.add can choose another duration.",
            "Still-image motion is static by default. image.motion.set supports static, zoom_in, zoom_out, pan_left, pan_right, pan_up, and pan_down.",
            "Video and image clips may have an incoming crossfade of up to five seconds; it must fit both adjacent clips, and the first visual clip cannot have a transition.",
            "Audio clips support linear fade-in and fade-out durations in frames. Each fade is at most five seconds, and the fades cannot overlap or exceed the clip duration.",
            "Supported still-image formats are PNG, JPEG, BMP, WebP, and TIFF.",
            "Video and still-image crop rectangles use normalized source coordinates; unconfigured clips retain fit-and-pad behavior.",
        ],
    }
}

fn operation_catalog() -> Vec<OperationCapability> {
    vec![
        OperationCapability {
            r#type: "clip.append",
            description: "Append a video asset to the contiguous visual track. Embedded audio is placed on audio-1 when present.",
            required: vec!["assetId"],
        },
        OperationCapability {
            r#type: "image.add",
            description: "Append a still-image asset to the contiguous visual track. durationSeconds defaults to five seconds.",
            required: vec!["assetId"],
        },
        OperationCapability {
            r#type: "audio.add",
            description: "Place an audio asset using startSeconds. durationSeconds and gain are optional.",
            required: vec!["assetId", "startSeconds"],
        },
        OperationCapability {
            r#type: "title.add",
            description: "Add a title on the overlay track using startSeconds and durationSeconds. Requires a configured font.",
            required: vec!["text", "startSeconds", "durationSeconds"],
        },
        OperationCapability {
            r#type: "clip.add",
            description: "Insert a fully specified video, image, audio, or text clip. Prefer the seconds-based operations.",
            required: vec!["trackId", "clip"],
        },
        OperationCapability {
            r#type: "clip.remove",
            description: "Remove a clip by id. Removing a visual clip closes the gap.",
            required: vec!["clipId"],
        },
        OperationCapability {
            r#type: "clip.move",
            description: "Move an audio or title clip to startFrame. Video and image clips must use clip.reorder.",
            required: vec!["clipId", "startFrame"],
        },
        OperationCapability {
            r#type: "clip.reorder",
            description: "Reorder a video or image clip. The track is reflowed so it stays contiguous from frame 0.",
            required: vec!["clipId", "destinationIndex"],
        },
        OperationCapability {
            r#type: "clip.trim",
            description: "Change startFrame and durationFrames, with source bounds for video/audio clips. Visual startFrame cannot open a gap; image clips have no source bounds.",
            required: vec!["clipId", "startFrame", "durationFrames"],
        },
        OperationCapability {
            r#type: "clip.crop",
            description: "Set a normalized crop rectangle in a video or still-image source. Use x=0, y=0, width=1, height=1 to reset to the full frame.",
            required: vec!["clipId", "crop"],
        },
        OperationCapability {
            r#type: "image.motion.set",
            description: "Set a still-image motion preset: static, zoom_in, zoom_out, pan_left, pan_right, pan_up, or pan_down.",
            required: vec!["clipId", "motion"],
        },
        OperationCapability {
            r#type: "clip.transition.set",
            description: "Set an incoming crossfade on a video or image clip. durationFrames must fit both adjacent clips and be no longer than five seconds.",
            required: vec!["clipId", "durationFrames"],
        },
        OperationCapability {
            r#type: "clip.transition.remove",
            description: "Remove the incoming crossfade from a video or image clip.",
            required: vec!["clipId"],
        },
        OperationCapability {
            r#type: "clip.split",
            description: "Split a clip at an interior splitFrame.",
            required: vec!["clipId", "splitFrame"],
        },
        OperationCapability {
            r#type: "text.update",
            description: "Update text, fontSize, color, x, or y on an existing title clip.",
            required: vec!["clipId"],
        },
        OperationCapability {
            r#type: "audio.gain.set",
            description: "Set linear gain from 0 to 4 on an audio clip.",
            required: vec!["clipId", "gain"],
        },
        OperationCapability {
            r#type: "audio.fade.set",
            description: "Set linear audio fade-in and fade-out durations in frames. Each is at most five seconds and their sum cannot exceed the clip duration; set both to zero to remove fades.",
            required: vec!["clipId", "fadeInFrames", "fadeOutFrames"],
        },
    ]
}

pub fn timeline_schema() -> Value {
    serde_json::from_str(include_str!("../schemas/timeline-1.0.0.schema.json"))
        .unwrap_or_else(|_| json!({"error": "bundled timeline schema is invalid"}))
}

pub fn openapi_document() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Avida local video studio",
            "version": "0.1.0",
            "description": "Loopback API for agent-first timeline assembly, revisioned edits, and verified renders. GET /v1/capabilities is the live contract if this document and the server disagree."
        },
        "components": {
            "securitySchemes": {
                "bearer": {"type": "http", "scheme": "bearer"}
            }
        },
        "security": [{"bearer": []}],
        "paths": {
            "/v1/capabilities": {"get": {"operationId": "getCapabilities", "responses": {"200": {"description": "Supported operations, presets, limits, and tool status"}}}},
            "/v1/schemas/timeline": {"get": {"operationId": "getTimelineSchema", "responses": {"200": {"description": "Timeline JSON Schema 1.0.0"}}}},
            "/v1/openapi.json": {"get": {"operationId": "getOpenApi", "responses": {"200": {"description": "This document"}}}},
            "/v1/projects": {
                "get": {"operationId": "listProjects", "responses": {"200": {"description": "Project summaries, newest first"}}},
                "post": {"operationId": "createProject", "description": "Send either a revision-0 timeline or {assetIds, presetId?, name?}. Video and still-image assets become visual clips; images hold for five seconds.", "responses": {"201": {"description": "{projectId, revision, name?}"}}}
            },
            "/v1/projects/{projectId}": {
                "get": {"operationId": "getProject", "parameters": [{"name": "projectId", "in": "path", "required": true, "schema": {"type": "string"}}], "responses": {"200": {"description": "Project summary and current revision"}}}
            },
            "/v1/projects/{projectId}/timeline": {
                "get": {"operationId": "getTimeline", "parameters": [{"name": "projectId", "in": "path", "required": true, "schema": {"type": "string"}}, {"name": "revision", "in": "query", "schema": {"type": "integer", "minimum": 1}}], "responses": {"200": {"description": "Current or pinned timeline JSON"}}}
            },
            "/v1/projects/{projectId}/revisions": {
                "get": {"operationId": "listRevisions", "parameters": [{"name": "projectId", "in": "path", "required": true, "schema": {"type": "string"}}], "responses": {"200": {"description": "Immutable revision history"}}}
            },
            "/v1/projects/{projectId}/restore": {
                "post": {"operationId": "restoreRevision", "description": "Restore a prior snapshot using targetRevision, expectedRevision, and idempotencyKey.", "responses": {"201": {"description": "New restored revision"}, "409": {"description": "Revision or idempotency conflict"}}}
            },
            "/v1/projects/{projectId}/operations": {
                "post": {"operationId": "editTimeline", "description": "{expectedRevision, idempotencyKey, operations}; supported operations include seconds-based edits, image.add, clip.crop, image.motion.set, validated incoming crossfades, and audio fades.", "responses": {"200": {"description": "{projectId, revision, replayed}"}, "409": {"description": "REVISION_CONFLICT or IDEMPOTENCY_CONFLICT"}}}
            },
            "/v1/projects/{projectId}/renders": {
                "post": {"operationId": "enqueueRender", "description": "{revision, presetId: export|preview, idempotencyKey}", "responses": {"202": {"description": "Queued job"}, "200": {"description": "Idempotent replay"}}}
            },
            "/v1/jobs": {"get": {"operationId": "listJobs", "parameters": [{"name": "projectId", "in": "query", "schema": {"type": "string"}}], "responses": {"200": {"description": "Recent render jobs"}}}},
            "/v1/jobs/{jobId}": {"get": {"operationId": "getJob", "responses": {"200": {"description": "Job status, phase, progressPercent, errorCode, and outputAssetId"}}}},
            "/v1/jobs/{jobId}/cancel": {"post": {"operationId": "cancelJob", "responses": {"200": {"description": "Updated cancellation state"}}}},
            "/v1/jobs/{jobId}/retry": {"post": {"operationId": "retryJob", "description": "Retry a failed job with an idempotencyKey.", "responses": {"202": {"description": "Queued retry"}, "409": {"description": "Job is not retryable"}}}},
            "/v1/jobs/{jobId}/inspection": {"get": {"operationId": "inspectJob", "responses": {"200": {"description": "QC findings and still frames"}}}},
            "/v1/jobs/{jobId}/stills/{frame}": {"get": {"operationId": "getStill", "responses": {"200": {"description": "JPEG still extracted from a successful render"}}}},
            "/v1/assets": {"get": {"operationId": "listAssets", "responses": {"200": {"description": "Imported source asset metadata"}}}},
            "/v1/assets/uploads": {"post": {"operationId": "createAssetUpload", "description": "Reserve a bounded upload using expectedBytes and optional fileName.", "responses": {"201": {"description": "Upload session"}}}},
            "/v1/assets/uploads/{uploadId}/content": {"put": {"operationId": "uploadAssetContent", "responses": {"200": {"description": "Received byte count"}}}},
            "/v1/assets/uploads/{uploadId}/complete": {"post": {"operationId": "completeAssetUpload", "responses": {"201": {"description": "Imported asset"}}}},
            "/v1/assets/uploads/{uploadId}": {"delete": {"operationId": "cancelAssetUpload", "responses": {"204": {"description": "Upload staging removed"}}}},
            "/v1/assets/{assetId}": {"get": {"operationId": "getAssetMetadata", "responses": {"200": {"description": "Asset metadata and content URL"}}}},
            "/v1/assets/{assetId}/content": {"get": {"operationId": "downloadAsset", "responses": {"200": {"description": "Verified source or render bytes"}}}}
        }
    })
}

fn probe_tool(path: &Path) -> ToolStatus {
    let key = path.to_string_lossy().to_string();
    if let Some(cached) = tool_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
    {
        return cached.clone();
    }
    let status = match run_bounded_cancellable(
        Command::new(path).arg("-version"),
        Duration::from_secs(5),
        None,
    ) {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let version = text.lines().next().unwrap_or("unknown").to_owned();
            ToolStatus {
                available: true,
                path: key.clone(),
                version: Some(version),
                fingerprint: Some(fingerprint(&output.stdout)),
            }
        }
        _ => ToolStatus {
            available: false,
            path: key.clone(),
            version: None,
            fingerprint: None,
        },
    };
    tool_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key, status.clone());
    status
}

fn fingerprint(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn tool_cache() -> &'static Mutex<HashMap<String, ToolStatus>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, ToolStatus>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}
