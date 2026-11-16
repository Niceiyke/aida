use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    thread,
    time::Duration,
};

use crate::{
    EditOperation, ImportOptions, ProjectStore, Workspace,
    capabilities::capabilities,
    error::{StudioError, find_studio_error, legacy_error_code},
    import_asset,
    inspect::{inspect_job, still_path},
    jobs::{JobCancellationRegistry, RenderJobRunner},
    render::validate_configured_font,
    timeline::Timeline,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub struct McpOptions {
    pub database: PathBuf,
    pub workspace: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub font: Option<PathBuf>,
    pub timeout_seconds: u64,
}

pub fn serve_mcp(options: McpOptions) -> Result<()> {
    if !(1..=3600).contains(&options.timeout_seconds) {
        bail!("timeout-seconds must be between 1 and 3600");
    }
    if let Some(font) = options.font.as_deref() {
        validate_configured_font(font).context("configured font is not usable")?;
    }
    Workspace::open(&options.workspace)?;
    ProjectStore::open(&options.database)?;
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout().lock();
    loop {
        let Some(message) = read_message(&mut input)? else {
            return Ok(());
        };
        if message.get("id").is_none() {
            continue;
        }
        let response = match dispatch(&options, &message) {
            Ok(result) => json!({
                "jsonrpc": "2.0",
                "id": message.get("id").cloned().unwrap_or(Value::Null),
                "result": result
            }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": message.get("id").cloned().unwrap_or(Value::Null),
                "error": error_object(&error)
            }),
        };
        write_message(&mut stdout, &response)?;
    }
}

fn dispatch(options: &McpOptions, message: &Value) -> Result<Value> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("MCP request is missing method"))?;
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "avida", "version": env!("CARGO_PKG_VERSION")}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => call_tool(options, &params),
        _ => Err(StudioError::new(
            "METHOD_NOT_FOUND",
            format!("unsupported MCP method {method}"),
        )
        .into()),
    }
}

fn call_tool(options: &McpOptions, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| StudioError::validation("tools/call requires name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = match name {
        "capabilities" => Ok(serde_json::to_value(capabilities(
            options.font.is_some(),
            &options.ffmpeg,
            &options.ffprobe,
        ))?),
        "import_media" => import_media(options, &arguments),
        "list_assets" => list_assets(options),
        "list_projects" => list_projects(options),
        "create_project" => create_project(options, &arguments),
        "get_timeline" => get_timeline(options, &arguments),
        "edit_timeline" => edit_timeline(options, &arguments),
        "list_jobs" => list_jobs(options, &arguments),
        "render" => render(options, &arguments),
        "get_job" => get_job(options, &arguments),
        "inspect_output" => inspect_output(options, &arguments),
        _ => Err(StudioError::validation(format!("unknown tool {name}")).into()),
    };
    match result {
        Ok(value) => Ok(tool_result(value, false)),
        Err(error) => Ok(tool_result(error_payload(&error), true)),
    }
}

fn import_media(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let file = required_string(arguments, "file")?;
    let asset = import_asset(ImportOptions {
        workspace: options.workspace.clone(),
        file: PathBuf::from(file),
        ffprobe: options.ffprobe.clone(),
    })?;
    Ok(serde_json::to_value(asset)?)
}

fn list_assets(options: &McpOptions) -> Result<Value> {
    let workspace = Workspace::open(&options.workspace)?;
    let assets = workspace.list_assets()?;
    Ok(json!({"assets": assets}))
}

fn list_projects(options: &McpOptions) -> Result<Value> {
    Ok(serde_json::to_value(
        ProjectStore::open(&options.database)?.list_project_summaries()?,
    )?)
}

fn create_project(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let store = ProjectStore::open(&options.database)?;
    if arguments.get("schemaVersion").is_some() {
        return Err(StudioError::validation(
            "create_project accepts assetIds, not a raw timeline. Use the HTTP API for a hand-authored timeline.",
        )
        .into());
    }
    let created = store.create_from_asset_ids(
        &options.workspace,
        &string_array(arguments, "assetIds")?,
        optional_string(arguments, "presetId")?,
        optional_string(arguments, "name")?,
    )?;
    Ok(serde_json::to_value(created)?)
}

fn get_timeline(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let store = ProjectStore::open(&options.database)?;
    let project_id = required_string(arguments, "projectId")?;
    let json = if let Some(revision) = arguments.get("revision").and_then(Value::as_u64) {
        store.load_revision_json(project_id, revision)?
    } else {
        store.load_current_revision_json(project_id)?.1
    };
    Ok(serde_json::from_str(&json)?)
}

fn edit_timeline(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let store = ProjectStore::open(&options.database)?;
    let operations = arguments.get("operations").cloned().ok_or_else(|| {
        StudioError::validation("operations is required").with_pointer("/operations")
    })?;
    let operations: Vec<EditOperation> = serde_json::from_value(operations)
        .context("operations do not match the supported edit schema")?;
    let result = store.apply_edit_batch_with(
        &options.workspace,
        required_string(arguments, "projectId")?,
        required_u64(arguments, "expectedRevision")?,
        required_string(arguments, "idempotencyKey")?,
        &operations,
        options.font.is_some(),
    )?;
    Ok(serde_json::to_value(result)?)
}

fn list_jobs(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let store = ProjectStore::open(&options.database)?;
    let project_id = optional_string(arguments, "projectId")?;
    Ok(serde_json::to_value(
        store.list_render_jobs(project_id.as_deref())?,
    )?)
}

fn render(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let store = ProjectStore::open(&options.database)?;
    let project_id = required_string(arguments, "projectId")?;
    let revision = required_u64(arguments, "revision")?;
    let preset_id = required_string(arguments, "presetId")?;
    let submission = store.enqueue_render_job(
        project_id,
        revision,
        preset_id,
        required_string(arguments, "idempotencyKey")?,
    )?;
    let wait = arguments
        .get("wait")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !wait || submission.job.status != crate::RenderJobStatus::Queued {
        return job_value(options, &submission.job);
    }
    let job_id = submission.job.job_id.clone();
    let runner = RenderJobRunner::new(
        store.clone(),
        options.workspace.clone(),
        options.ffmpeg.clone(),
        options.ffprobe.clone(),
        options.font.clone(),
        options.timeout_seconds,
        JobCancellationRegistry::default(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start MCP render runtime")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(options.timeout_seconds.max(1));
    loop {
        runtime.block_on(runner.run_once())?;
        let job = store.get_render_job(&job_id)?;
        if !matches!(
            job.status,
            crate::RenderJobStatus::Queued | crate::RenderJobStatus::Running
        ) {
            return job_value(options, &job);
        }
        if std::time::Instant::now() >= deadline {
            return job_value(options, &job);
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn get_job(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let job = ProjectStore::open(&options.database)?
        .get_render_job(required_string(arguments, "jobId")?)?;
    job_value(options, &job)
}

fn job_value(options: &McpOptions, job: &crate::RenderJob) -> Result<Value> {
    let mut value = serde_json::to_value(job)?;
    if job.status == crate::RenderJobStatus::Succeeded {
        let output =
            Workspace::open(&options.workspace)?.resolve_render_output(&job.job_id, job.attempt)?;
        value["outputFile"] = json!(output.to_string_lossy());
    }
    Ok(value)
}

fn inspect_output(options: &McpOptions, arguments: &Value) -> Result<Value> {
    let store = ProjectStore::open(&options.database)?;
    let job = store.get_render_job(required_string(arguments, "jobId")?)?;
    let timeline_json = store.load_revision_json(&job.project_id, job.revision).ok();
    let timeline = timeline_json.and_then(|json| serde_json::from_str::<Timeline>(&json).ok());
    let output = if job.status == crate::RenderJobStatus::Succeeded {
        Workspace::open(&options.workspace)?
            .resolve_render_output(&job.job_id, job.attempt)
            .ok()
    } else {
        None
    };
    let inspection = inspect_job(&job, timeline.as_ref(), output.as_deref(), &options.ffmpeg)?;
    let mut images = Vec::new();
    if let Some(output) = output.as_deref() {
        for still in &inspection.stills {
            let path = still_path(output, still.frame)?;
            let bytes = fs::read(&path)
                .with_context(|| format!("could not read generated still {}", path.display()))?;
            images.push(json!({"frame": still.frame, "data": base64_encode(&bytes)}));
        }
    }
    let output_file = output.map(|output| output.to_string_lossy().to_string());
    Ok(json!({"inspection": inspection, "images": images, "outputFile": output_file}))
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "capabilities",
            "Return supported operations, canvas presets, limits, font status, and FFmpeg fingerprints.",
            json!({"type": "object", "additionalProperties": false}),
        ),
        tool(
            "import_media",
            "Probe and stage a local video, audio, or still-image file. Supports PNG, JPEG, BMP, WebP, and TIFF; returns assetId, uri, sha256, and probe metadata.",
            json!({"type": "object", "additionalProperties": false, "required": ["file"], "properties": {"file": {"type": "string"}}}),
        ),
        tool(
            "list_assets",
            "List imported source assets in the workspace.",
            json!({"type": "object", "additionalProperties": false}),
        ),
        tool(
            "list_projects",
            "List local projects newest first.",
            json!({"type": "object", "additionalProperties": false}),
        ),
        tool(
            "create_project",
            "Create a revisioned project from imported asset IDs. Videos and still images become visual clips in assetIds order; images default to five seconds. presetId is a canvas preset, not export or preview.",
            json!({"type": "object", "additionalProperties": false, "required": ["assetIds"], "properties": {"assetIds": {"type": "array", "items": {"type": "string"}, "minItems": 1}, "presetId": {"type": "string"}, "name": {"type": "string"}}}),
        ),
        tool(
            "get_timeline",
            "Read the current timeline, or a pinned revision.",
            json!({"type": "object", "additionalProperties": false, "required": ["projectId"], "properties": {"projectId": {"type": "string"}, "revision": {"type": "integer"}}}),
        ),
        tool(
            "edit_timeline",
            "Apply an atomic edit batch. On REVISION_CONFLICT, read details.currentRevision and retry with a new idempotency key.",
            edit_operation_schema(),
        ),
        tool(
            "list_jobs",
            "List recent render jobs, optionally for one project.",
            json!({"type": "object", "additionalProperties": false, "properties": {"projectId": {"type": "string"}}}),
        ),
        tool(
            "render",
            "Queue a preview or export pinned to a revision. Waits for completion unless wait is false.",
            json!({"type": "object", "additionalProperties": false, "required": ["projectId", "revision", "presetId", "idempotencyKey"], "properties": {"projectId": {"type": "string"}, "revision": {"type": "integer"}, "presetId": {"type": "string", "enum": ["preview", "export"]}, "idempotencyKey": {"type": "string"}, "wait": {"type": "boolean"}}}),
        ),
        tool(
            "get_job",
            "Read a render job. Failed jobs include errorCode.",
            json!({"type": "object", "additionalProperties": false, "required": ["jobId"], "properties": {"jobId": {"type": "string"}}}),
        ),
        tool(
            "inspect_output",
            "Return QC findings and still-frame URLs for a render. Do not treat a successful render as approved without reading qc.",
            json!({"type": "object", "additionalProperties": false, "required": ["jobId"], "properties": {"jobId": {"type": "string"}}}),
        ),
    ]
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema
    })
}

fn edit_operation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["projectId", "expectedRevision", "idempotencyKey", "operations"],
        "properties": {
            "projectId": {"type": "string"},
            "expectedRevision": {"type": "integer", "minimum": 1},
            "idempotencyKey": {"type": "string", "minLength": 1},
            "operations": {
                "type": "array",
                "minItems": 1,
                "maxItems": 100,
                "items": {"oneOf": [
                    {"$ref": "#/$defs/clipAppend"},
                    {"$ref": "#/$defs/imageAdd"},
                    {"$ref": "#/$defs/audioAdd"},
                    {"$ref": "#/$defs/titleAdd"},
                    {"$ref": "#/$defs/clipAdd"},
                    {"$ref": "#/$defs/clipRemove"},
                    {"$ref": "#/$defs/clipMove"},
                    {"$ref": "#/$defs/clipReorder"},
                    {"$ref": "#/$defs/clipTrim"},
                    {"$ref": "#/$defs/clipCrop"},
                    {"$ref": "#/$defs/imageMotionSet"},
                    {"$ref": "#/$defs/clipTransitionSet"},
                    {"$ref": "#/$defs/clipTransitionRemove"},
                    {"$ref": "#/$defs/clipSplit"},
                    {"$ref": "#/$defs/textUpdate"},
                    {"$ref": "#/$defs/audioGainSet"},
                    {"$ref": "#/$defs/audioFadeSet"}
                ]}
            }
        },
        "$defs": {
            "clipAppend": {"type": "object", "additionalProperties": false, "required": ["type", "assetId"], "properties": {"type": {"const": "clip.append"}, "assetId": {"type": "string"}, "durationSeconds": {"type": "number", "exclusiveMinimum": 0}, "sourceInSeconds": {"type": "number", "minimum": 0}}},
            "imageAdd": {"type": "object", "additionalProperties": false, "required": ["type", "assetId"], "properties": {"type": {"const": "image.add"}, "assetId": {"type": "string"}, "durationSeconds": {"type": "number", "exclusiveMinimum": 0}}},
            "audioAdd": {"type": "object", "additionalProperties": false, "required": ["type", "assetId", "startSeconds"], "properties": {"type": {"const": "audio.add"}, "assetId": {"type": "string"}, "trackId": {"type": "string"}, "startSeconds": {"type": "number", "minimum": 0}, "durationSeconds": {"type": "number", "exclusiveMinimum": 0}, "gain": {"type": "number", "minimum": 0, "maximum": 4}}},
            "titleAdd": {"type": "object", "additionalProperties": false, "required": ["type", "text", "startSeconds", "durationSeconds"], "properties": {"type": {"const": "title.add"}, "text": {"type": "string", "minLength": 1, "maxLength": 4096}, "startSeconds": {"type": "number", "minimum": 0}, "durationSeconds": {"type": "number", "exclusiveMinimum": 0}, "fontSize": {"type": "integer", "minimum": 1}, "color": {"type": "string"}, "x": {"type": "integer", "minimum": 0}, "y": {"type": "integer", "minimum": 0}}},
            "clipAdd": {"type": "object", "additionalProperties": false, "required": ["type", "trackId", "clip"], "properties": {"type": {"const": "clip.add"}, "trackId": {"type": "string"}, "clip": {"type": "object"}, "index": {"type": "integer", "minimum": 0}}},
            "clipRemove": {"type": "object", "additionalProperties": false, "required": ["type", "clipId"], "properties": {"type": {"const": "clip.remove"}, "clipId": {"type": "string"}}},
            "clipMove": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "startFrame"], "properties": {"type": {"const": "clip.move"}, "clipId": {"type": "string"}, "startFrame": {"type": "integer", "minimum": 0}}},
            "clipReorder": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "destinationIndex"], "properties": {"type": {"const": "clip.reorder"}, "clipId": {"type": "string"}, "destinationIndex": {"type": "integer", "minimum": 0}}},
            "clipTrim": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "startFrame", "durationFrames"], "properties": {"type": {"const": "clip.trim"}, "clipId": {"type": "string"}, "startFrame": {"type": "integer", "minimum": 0}, "durationFrames": {"type": "integer", "minimum": 1}, "sourceIn": {"type": "object"}, "sourceOut": {"type": "object"}}},
            "clipCrop": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "crop"], "properties": {"type": {"const": "clip.crop"}, "clipId": {"type": "string"}, "crop": {"type": "object", "additionalProperties": false, "required": ["x", "y", "width", "height"], "properties": {"x": {"type": "number", "minimum": 0, "maximum": 1}, "y": {"type": "number", "minimum": 0, "maximum": 1}, "width": {"type": "number", "exclusiveMinimum": 0, "maximum": 1}, "height": {"type": "number", "exclusiveMinimum": 0, "maximum": 1}}}}},
            "imageMotionSet": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "motion"], "properties": {"type": {"const": "image.motion.set"}, "clipId": {"type": "string"}, "motion": {"enum": ["static", "zoom_in", "zoom_out", "pan_left", "pan_right", "pan_up", "pan_down"]}}},
            "clipTransitionSet": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "durationFrames"], "properties": {"type": {"const": "clip.transition.set"}, "clipId": {"type": "string"}, "durationFrames": {"type": "integer", "minimum": 1, "maximum": 150}}},
            "clipTransitionRemove": {"type": "object", "additionalProperties": false, "required": ["type", "clipId"], "properties": {"type": {"const": "clip.transition.remove"}, "clipId": {"type": "string"}}},
            "clipSplit": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "splitFrame"], "properties": {"type": {"const": "clip.split"}, "clipId": {"type": "string"}, "splitFrame": {"type": "integer", "minimum": 1}}},
            "textUpdate": {"type": "object", "additionalProperties": false, "required": ["type", "clipId"], "properties": {"type": {"const": "text.update"}, "clipId": {"type": "string"}, "text": {"type": "string"}, "fontSize": {"type": "integer"}, "color": {"type": "string"}, "x": {"type": "integer"}, "y": {"type": "integer"}}},
            "audioGainSet": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "gain"], "properties": {"type": {"const": "audio.gain.set"}, "clipId": {"type": "string"}, "gain": {"type": "number", "minimum": 0, "maximum": 4}}},
            "audioFadeSet": {"type": "object", "additionalProperties": false, "required": ["type", "clipId", "fadeInFrames", "fadeOutFrames"], "properties": {"type": {"const": "audio.fade.set"}, "clipId": {"type": "string"}, "fadeInFrames": {"type": "integer", "minimum": 0}, "fadeOutFrames": {"type": "integer", "minimum": 0}}}
        }
    })
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let mut content = Vec::new();
    let display_value = if let Some(inspection) = value.get("inspection") {
        if let Some(images) = value.get("images").and_then(Value::as_array) {
            for image in images {
                if let Some(data) = image.get("data").and_then(Value::as_str) {
                    content.push(json!({"type": "image", "mimeType": "image/jpeg", "data": data}));
                }
            }
        }
        let mut summary = inspection.clone();
        if let (Some(output), Some(summary)) = (value.get("outputFile"), summary.as_object_mut()) {
            summary.insert("outputFile".to_owned(), output.clone());
        }
        summary
    } else {
        value
    };
    content.insert(
        0,
        json!({
            "type": "text",
            "text": serde_json::to_string_pretty(&display_value).unwrap_or_else(|_| display_value.to_string())
        }),
    );
    json!({"content": content, "isError": is_error})
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[(a >> 2) as usize] as char);
        encoded.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(c & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn error_payload(error: &anyhow::Error) -> Value {
    if let Some(studio) = find_studio_error(error) {
        return json!({
            "error": {
                "code": studio.code,
                "message": studio.message,
                "pointer": studio.pointer,
                "expected": studio.expected,
                "actual": studio.actual,
                "currentRevision": studio.current_revision,
                "retryable": studio.retryable
            }
        });
    }
    if let Some(code) = legacy_error_code(error) {
        return json!({
            "error": {
                "code": code,
                "message": format!("{error:#}"),
                "retryable": false
            }
        });
    }
    json!({"error": {"code": "INTERNAL_ERROR", "message": format!("{error:#}"), "retryable": false}})
}

fn error_object(error: &anyhow::Error) -> Value {
    let code = find_studio_error(error).map_or(-32603, |studio| match studio.code {
        "METHOD_NOT_FOUND" => -32601,
        "VALIDATION_FAILED" => -32602,
        _ => -32603,
    });
    let message = find_studio_error(error)
        .map(|studio| studio.message.clone())
        .unwrap_or_else(|| format!("{error:#}"));
    json!({"code": code, "message": message})
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            StudioError::validation(format!("{field} is required"))
                .with_pointer(format!("/{field}"))
                .into()
        })
}

fn optional_string(value: &Value, field: &str) -> Result<Option<String>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(StudioError::validation(format!("{field} must be a string"))
            .with_pointer(format!("/{field}"))
            .into()),
    }
}

fn required_u64(value: &Value, field: &str) -> Result<u64> {
    value.get(field).and_then(Value::as_u64).ok_or_else(|| {
        StudioError::validation(format!("{field} must be a nonnegative integer"))
            .with_pointer(format!("/{field}"))
            .into()
    })
}

fn string_array(value: &Value, field: &str) -> Result<Vec<String>> {
    let items = value.get(field).and_then(Value::as_array).ok_or_else(|| {
        StudioError::validation(format!("{field} must be an array"))
            .with_pointer(format!("/{field}"))
    })?;
    items
        .iter()
        .map(|item| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                StudioError::validation(format!("{field} must contain strings")).into()
            })
        })
        .collect()
}

fn read_message(input: &mut impl BufRead) -> Result<Option<Value>> {
    const MAX_MESSAGE_BYTES: u64 = 2 * 1024 * 1024;
    let mut line = Vec::new();
    let read = std::io::Read::take(input, MAX_MESSAGE_BYTES + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() as u64 > MAX_MESSAGE_BYTES {
        bail!("MCP message exceeds 2 MiB");
    }
    let line = std::str::from_utf8(&line).context("MCP message is not UTF-8")?;
    Ok(Some(serde_json::from_str(line.trim_end()).context(
        "MCP message is not a newline-delimited JSON-RPC object",
    )?))
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    output.write_all(&body)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AssetKind, MediaProbe, StreamInfo,
        workspace::{AssetManifest, SOURCE_FILENAME},
    };
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    #[test]
    fn initialize_and_tool_list_advertise_the_agent_contract() {
        let options = McpOptions {
            database: PathBuf::from("projects.sqlite"),
            workspace: PathBuf::from("workspace"),
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            font: None,
            timeout_seconds: 30,
        };
        let initialized = dispatch(
            &options,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        )
        .unwrap();
        assert_eq!(initialized["protocolVersion"], PROTOCOL_VERSION);
        let tools = dispatch(
            &options,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .unwrap();
        let names: Vec<_> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(names.contains(&"create_project"));
        assert!(names.contains(&"edit_timeline"));
        assert!(names.contains(&"inspect_output"));
        assert!(!names.contains(&"text.add"));
        let edit_tool = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "edit_timeline")
            .unwrap();
        assert_eq!(
            edit_tool["inputSchema"]["properties"]["operations"]["items"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            17
        );
        assert!(
            edit_tool["inputSchema"]["properties"]["operations"]["items"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["$ref"] == "#/$defs/imageMotionSet")
        );
        assert!(
            edit_tool["inputSchema"]["properties"]["operations"]["items"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["$ref"] == "#/$defs/clipTransitionSet")
        );
        assert!(
            edit_tool["inputSchema"]["properties"]["operations"]["items"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["$ref"] == "#/$defs/audioFadeSet")
        );
    }

    #[test]
    fn mcp_tools_create_and_edit_a_project_through_the_shared_services() {
        const ASSET_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0b";
        const IMAGE_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0d";
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let workspace = Workspace::open(&workspace_path).unwrap();
        let source = b"mcp fixture bytes";
        let directory = workspace_path.join("assets").join(ASSET_ID);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(SOURCE_FILENAME), source).unwrap();
        let manifest = AssetManifest {
            schema_version: 1,
            asset_id: ASSET_ID.to_owned(),
            sha256: hex::encode(Sha256::digest(source)),
            bytes: source.len() as u64,
            kind: AssetKind::Video,
            file_name: Some("mcp-fixture.mp4".to_owned()),
            probe: MediaProbe {
                format_name: "mov,mp4".to_owned(),
                duration: "1.0".to_owned(),
                duration_seconds: 1.0,
                has_video: true,
                has_audio: false,
                has_still_image: false,
                video: Some(StreamInfo {
                    codec: "h264".to_owned(),
                    duration_seconds: Some(1.0),
                    frame_rate: Some("30/1".to_owned()),
                    width: Some(320),
                    height: Some(240),
                    sample_rate: None,
                    channels: None,
                }),
                audio: None,
            },
        };
        std::fs::write(
            directory.join("asset.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(workspace.load_asset(ASSET_ID).is_ok());
        let image_source = b"mcp image fixture bytes";
        let image_directory = workspace_path.join("assets").join(IMAGE_ID);
        std::fs::create_dir_all(&image_directory).unwrap();
        std::fs::write(image_directory.join(SOURCE_FILENAME), image_source).unwrap();
        let image_manifest = AssetManifest {
            schema_version: 1,
            asset_id: IMAGE_ID.to_owned(),
            sha256: hex::encode(Sha256::digest(image_source)),
            bytes: image_source.len() as u64,
            kind: AssetKind::Image,
            file_name: Some("mcp-fixture.png".to_owned()),
            probe: MediaProbe {
                format_name: "png_pipe".to_owned(),
                duration: "0".to_owned(),
                duration_seconds: 0.0,
                has_video: false,
                has_audio: false,
                has_still_image: true,
                video: Some(StreamInfo {
                    codec: "png".to_owned(),
                    duration_seconds: None,
                    frame_rate: Some("25/1".to_owned()),
                    width: Some(320),
                    height: Some(240),
                    sample_rate: None,
                    channels: None,
                }),
                audio: None,
            },
        };
        std::fs::write(
            image_directory.join("asset.json"),
            serde_json::to_vec(&image_manifest).unwrap(),
        )
        .unwrap();
        assert!(workspace.load_asset(IMAGE_ID).is_ok());
        let options = McpOptions {
            database: root.path().join("projects.sqlite"),
            workspace: workspace_path,
            ffmpeg: PathBuf::from("missing-ffmpeg"),
            ffprobe: PathBuf::from("missing-ffprobe"),
            font: None,
            timeout_seconds: 30,
        };

        let created = call_tool(
            &options,
            &json!({
                "name": "create_project",
                "arguments": {"assetIds": [ASSET_ID], "name": "MCP campaign"}
            }),
        )
        .unwrap();
        assert_eq!(created["isError"], false);
        let created: Value =
            serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(created["name"], "MCP campaign");
        let project_id = created["projectId"].as_str().unwrap();

        let edited = call_tool(
            &options,
            &json!({
                "name": "edit_timeline",
                "arguments": {
                    "projectId": project_id,
                    "expectedRevision": 1,
                    "idempotencyKey": "mcp-seconds-edit",
                    "operations": [{
                        "type": "clip.append",
                        "assetId": ASSET_ID,
                        "sourceInSeconds": 0.25,
                        "durationSeconds": 0.5
                    }, {
                        "type": "image.add",
                        "assetId": IMAGE_ID,
                        "durationSeconds": 2
                    }]
                }
            }),
        )
        .unwrap();
        assert_eq!(edited["isError"], false);
        let edited: Value =
            serde_json::from_str(edited["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(edited["revision"], 2);

        let timeline = call_tool(
            &options,
            &json!({"name": "get_timeline", "arguments": {"projectId": project_id}}),
        )
        .unwrap();
        let timeline: Value =
            serde_json::from_str(timeline["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(timeline["tracks"][0]["clips"].as_array().unwrap().len(), 3);
        assert_eq!(timeline["tracks"][0]["clips"][1]["durationFrames"], 15);
        assert_eq!(timeline["tracks"][0]["clips"][2]["kind"], "image");
        assert_eq!(timeline["tracks"][0]["clips"][2]["durationFrames"], 60);
        let image_clip_id = timeline["tracks"][0]["clips"][2]["id"].as_str().unwrap();
        let crossfade = call_tool(
            &options,
            &json!({
                "name": "edit_timeline",
                "arguments": {
                    "projectId": project_id,
                    "expectedRevision": 2,
                    "idempotencyKey": "mcp-crossfade-edit",
                    "operations": [{
                        "type": "clip.transition.set",
                        "clipId": image_clip_id,
                        "durationFrames": 12
                    }]
                }
            }),
        )
        .unwrap();
        assert_eq!(crossfade["isError"], false);
        let crossfade: Value =
            serde_json::from_str(crossfade["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(crossfade["revision"], 3);
        let timeline = call_tool(
            &options,
            &json!({"name": "get_timeline", "arguments": {"projectId": project_id}}),
        )
        .unwrap();
        let timeline: Value =
            serde_json::from_str(timeline["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(timeline["tracks"][0]["clips"][2]["startFrame"], 33);
        assert_eq!(
            timeline["tracks"][0]["clips"][2]["transitionIn"]["type"],
            "crossfade"
        );
    }

    #[test]
    fn mcp_stdio_is_bounded_newline_delimited_json_rpc() {
        let mut input = &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n"[..];
        let request = read_message(&mut input).unwrap().unwrap();
        assert_eq!(request["method"], "ping");

        let mut output = Vec::new();
        write_message(
            &mut output,
            &json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
        )
        .unwrap();
        assert!(output.ends_with(b"\n"));
        assert_eq!(base64_encode(b"f"), "Zg==");

        let error = dispatch(
            &McpOptions {
                database: PathBuf::from("projects.sqlite"),
                workspace: PathBuf::from("workspace"),
                ffmpeg: PathBuf::from("ffmpeg"),
                ffprobe: PathBuf::from("ffprobe"),
                font: None,
                timeout_seconds: 30,
            },
            &json!({"jsonrpc": "2.0", "id": 2, "method": "does/not/exist"}),
        )
        .unwrap_err();
        assert_eq!(error_object(&error)["code"], -32601);
        let actionable = anyhow!("PROJECT_NOT_FOUND: project is absent");
        assert_eq!(
            error_payload(&actionable)["error"]["code"],
            "PROJECT_NOT_FOUND"
        );
    }
}
