# AI Video Studio

Local-first video studio. The CLI stages video, audio, and still-image assets, validates JSON timelines against the bundled [`1.0.0` schema](schemas/timeline-1.0.0.schema.json), and renders H.264/AAC MP4 with FFmpeg. An authenticated loopback API, stdio MCP tools, and React editor support asset-based project bootstrap, revisioned video/image/audio/title edits and crossfades, preview/export jobs, QC inspection, and verified downloads. Full NLE parity is not implemented.

## Requirements

- Rust toolchain (edition 2024)
- Recent `ffmpeg` and `ffprobe` on `PATH`, or pass their executable paths explicitly
- An FFmpeg build with `libx264`, AAC, and `drawtext` support for text overlays
- A `.ttf`, `.otf`, or `.ttc` font file when the timeline contains text
- Node.js 18+ and npm to run the browser studio

#### FFmpeg paths on this Windows workspace

The installed FFmpeg tools are **not on `PATH`**. Use these executable paths when running tests or starting the API here:

- `C:\ffmpeg-9.0.1-full_build-shared\bin\ffmpeg.exe`
- `C:\ffmpeg-9.0.1-full_build-shared\bin\ffprobe.exe`

`STUDIO_TEST_FFMPEG` and `STUDIO_TEST_FFPROBE` configure test discovery only. The API needs the `--ffmpeg` and `--ffprobe` CLI options as shown below. Timelines with title/text clips also require the API's `--font` option; this workspace has Arial at `C:\Windows\Fonts\arial.ttf`. On another machine, replace these with that machine's tool/font paths or put FFmpeg tools on `PATH`.

## Build and test

```powershell
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

Build the browser studio separately with `cd apps/web-editor; npm ci; npm run build`.

The FFmpeg integration tests run when `ffmpeg` and `ffprobe` are on `PATH`. They cover still-image import/bootstrap/crop/motion, preview and export motion renders, synthetic H.264 and H.265/VFR media, portrait scaling, preview downscaling, a 70-second audio source, exact output frame count/rate, audio onset, failed-render cleanup, SQLite edit rollback/idempotency/restart rendering, job recovery/cancellation/retries/atomic publication, output asset streaming, and (on Unix) symlink rejection. H.265 is skipped when the installed FFmpeg has no `libx265` encoder. If the tools are elsewhere, set their paths (and optionally a font) before testing:

```powershell
$env:STUDIO_TEST_FFMPEG = 'C:\ffmpeg-9.0.1-full_build-shared\bin\ffmpeg.exe'
$env:STUDIO_TEST_FFPROBE = 'C:\ffmpeg-9.0.1-full_build-shared\bin\ffprobe.exe'
$env:STUDIO_TEST_FONT = 'C:\Windows\Fonts\arial.ttf'
cargo test
```

Run the complete synthetic demo on Windows with FFmpeg available:

```powershell
.\scripts\demo.ps1 -Ffmpeg 'C:\tools\ffmpeg\bin\ffmpeg.exe' -Ffprobe 'C:\tools\ffmpeg\bin\ffprobe.exe'
```

The script creates synthetic video, music-tone and narration-placeholder fixtures and outputs under a unique directory in `target\`; no licensed media or voice is bundled, and it does not overwrite prior demos.

## CLI

Import copies video, audio, or PNG/JPEG/BMP/WebP/TIFF still-image media into a workspace using a generated asset ID and returns JSON containing the URI and checksum:

```powershell
.\target\release\studio.exe import --workspace .\workspace --file .\media\clip.mp4 --ffprobe 'C:\tools\ffmpeg\bin\ffprobe.exe'
```

Create a timeline JSON using the returned `assetId`, `uri`, and `sha256`, then render it:

```powershell
.\target\release\studio.exe render --workspace .\workspace --timeline .\timeline.json --output .\promo.mp4 `
  --ffmpeg 'C:\tools\ffmpeg\bin\ffmpeg.exe' `
  --ffprobe 'C:\tools\ffmpeg\bin\ffprobe.exe' `
  --font 'C:\Windows\Fonts\arial.ttf'
```

The output directory must already exist. The renderer refuses to overwrite the MP4 or its `.provenance.json` sidecar. On Linux/macOS, use `./target/release/studio` instead. See [`docs/SPEC.md`](docs/SPEC.md) for the supported first-build timeline shape and limits.

### Local SQLite revisions

Create a project from a validated timeline with `revision: 0`; creation stores it as revision 1:

```powershell
$project = .\target\release\studio.exe project create `
  --database .\workspace\projects.sqlite --workspace .\workspace `
  --timeline .\timeline.json | Out-String | ConvertFrom-Json
```

Apply a JSON batch using the expected revision and an idempotency key. Supported edits include clip operations, `audio.gain.set`, `audio.fade.set`, `text.update`, `image.motion.set`, and validated incoming crossfade transitions:

```json
{
  "operations": [
    {"type": "audio.gain.set", "clipId": "music-clip", "gain": 0.75},
    {"type": "audio.fade.set", "clipId": "music-clip", "fadeInFrames": 12, "fadeOutFrames": 12},
    {"type": "text.update", "clipId": "brand-title", "text": "Updated title"},
    {"type": "image.motion.set", "clipId": "hero-still", "motion": "zoom_in"}
  ]
}
```

```powershell
$edit = .\target\release\studio.exe edit --database .\workspace\projects.sqlite `
  --workspace .\workspace --project-id $project.projectId `
  --expected-revision $project.revision --idempotency-key edit-0001 `
  --operations .\operations.json | Out-String | ConvertFrom-Json
```

The whole batch is validated and committed atomically. Reusing a key with the same request returns its original revision; a different request conflicts, as does a new edit based on a stale revision. Render any saved snapshot with:

```powershell
.\target\release\studio.exe render-revision --database .\workspace\projects.sqlite `
  --workspace .\workspace --project-id $edit.projectId --revision $edit.revision `
  --output .\revision.mp4
```

The database is local and single-user. The API and shared operation executor expose revision history/restore and typed clip, audio-gain/fade, title, crop, still-image motion, and crossfade edits. Audio fades are linear, frame-based, and individually limited to five seconds; their durations cannot overlap or exceed the clip. Image motion accepts `static`, `zoom_in`, `zoom_out`, `pan_left`, `pan_right`, `pan_up`, or `pan_down`; static is the default. Crossfades are explicit incoming transitions on video/image clips, up to five seconds, and must fit both adjacent clips. Operation/audit records and multi-user scopes remain future work.

### Loopback REST API

Start the API with a random token of at least 32 bytes. It refuses non-loopback bind addresses and requires `Authorization: Bearer <token>` on every route. Keep the server terminal open; set the same token in the client terminal.

```powershell
$token = [guid]::NewGuid().ToString('N') + [guid]::NewGuid().ToString('N')
$token # copy this value into the client terminal
$env:STUDIO_API_TOKEN = $token
.\target\release\studio.exe api serve --database .\workspace\projects.sqlite `
  --workspace .\workspace `
  --ffmpeg 'C:\ffmpeg-9.0.1-full_build-shared\bin\ffmpeg.exe' `
  --ffprobe 'C:\ffmpeg-9.0.1-full_build-shared\bin\ffprobe.exe' `
  --font 'C:\Windows\Fonts\arial.ttf'
```

In a second terminal, start the React studio (install its dependencies once). The Vite proxy keeps browser requests same-origin while forwarding them to the loopback API:

```powershell
cd .\apps\web-editor
npm ci
npm run dev
```

Open `http://127.0.0.1:5173`, paste the API token, and connect. Import video, audio, and still-image assets, then create a project from a video or image in the media bin or reopen a recent project. Images become five-second visual clips by default. The editor supports one contiguous visual track with hard cuts or explicit incoming crossfades, two audio tracks, and a title track: add/reorder/split/remove visual clips, set or remove crossfades, trim video or adjust image hold duration, move audio/title clips, adjust audio gain/fades and title properties, and undo/redo through immutable revisions. Queue a rendered preview or full-quality export and download the verified MP4 when the job succeeds. Source playback is labelled approximate for composite timelines; the rendered preview is authoritative for its revision. The token is held in browser session storage only; media, project data, and renders stay in the configured local workspace. Other transitions, effects, captions, synchronized proxy playback, TTS, and collaboration remain deferred. For a different API port, set `VITE_API_TARGET` before `npm run dev`.

Upload media directly instead of using `studio import`:

```powershell
$headers = @{ Authorization = "Bearer $env:STUDIO_API_TOKEN" }
$source = Get-Item .\clip.mp4
$upload = Invoke-RestMethod -Method Post -Uri 'http://127.0.0.1:8787/v1/assets/uploads' `
  -Headers $headers -ContentType 'application/json' `
  -Body (@{ expectedBytes = $source.Length } | ConvertTo-Json)
Invoke-WebRequest -Method Put -Uri "http://127.0.0.1:8787$($upload.uploadUrl)" `
  -Headers $headers -ContentType 'application/octet-stream' -InFile $source.FullName
$asset = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:8787$($upload.completeUrl)" `
  -Headers $headers
```

After uploading media or staging it with `studio import`, create a project by POSTing `{ "assetIds": ["<video-or-image-asset-id>", "<music-asset-id>"], "presetId": "vertical_1080p30", "name": "Spring campaign" }` to `/v1/projects`. Video and still-image assets become contiguous visual clips in the supplied order; each image holds for five seconds. Audio assets are placed on the first audio track. A revision-0 timeline JSON remains available for full control. The API runs one SQLite-backed render worker; render jobs pin an immutable revision and support `export` and `preview` presets. For example, from another terminal:

```powershell
$env:STUDIO_API_TOKEN = '<paste the same token>'
$headers = @{ Authorization = "Bearer $env:STUDIO_API_TOKEN" }
$project = Invoke-RestMethod -Method Post -Uri 'http://127.0.0.1:8787/v1/projects' `
  -Headers $headers -ContentType 'application/json' -InFile .\timeline.json
Invoke-RestMethod -Method Get `
  -Uri "http://127.0.0.1:8787/v1/projects/$($project.projectId)/timeline?revision=1" `
  -Headers $headers
$render = Invoke-RestMethod -Method Post `
  -Uri "http://127.0.0.1:8787/v1/projects/$($project.projectId)/renders" `
  -Headers $headers -ContentType 'application/json' `
  -Body (@{ revision = 1; presetId = 'export'; idempotencyKey = 'render-0001' } | ConvertTo-Json)
$jobId = $render.job.jobId
$job = Invoke-RestMethod -Method Get -Uri "http://127.0.0.1:8787/v1/jobs/$jobId" -Headers $headers
while ($job.status -in @('queued', 'running')) {
  Start-Sleep -Seconds 1
  $job = Invoke-RestMethod -Method Get -Uri "http://127.0.0.1:8787/v1/jobs/$jobId" -Headers $headers
}
if ($job.status -eq 'succeeded') {
  Invoke-WebRequest -Method Get `
    -Uri "http://127.0.0.1:8787/v1/assets/$($job.outputAssetId)/content" `
    -Headers $headers -OutFile .\render.mp4
}
# Optional: cancel while queued/running, or retry if failed.
# Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:8787/v1/jobs/$jobId/cancel" -Headers $headers
# Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:8787/v1/jobs/$jobId/retry" -Headers $headers -ContentType 'application/json' -Body (@{ idempotencyKey = 'retry-render-0001' } | ConvertTo-Json)
```

`GET /v1/projects/{id}/revisions` returns the current revision and revision summaries newest-first. `POST /v1/projects/{id}/restore` accepts `{targetRevision, expectedRevision, idempotencyKey}` and creates a new revision from that snapshot; it never rewrites history. A stale `expectedRevision` returns `REVISION_CONFLICT` with the current revision, and a repeated request returns the same new revision. `POST /v1/projects/{id}/renders` accepts `{revision, presetId: "export" | "preview", idempotencyKey}` and returns `{job, replayed}`. Repeat submissions with the same key and request return the same job. `preview` scales down to a maximum 640-pixel edge without upscaling, preserves aspect ratio within even H.264 dimensions, and uses faster/lower-quality encoding; the `export` dimensions and encoding are unchanged. Poll `GET /v1/jobs/{id}` for `queued`, `running`, `succeeded`, `failed`, or `cancelled`; a successful job returns `outputAssetId`, not a local filesystem path. `GET /v1/assets/{id}` returns imported-source metadata and probe information or render-output metadata including `presetId`; `/content` verifies file size and SHA-256 before streaming from that same open file handle. To upload media, `POST /v1/assets/uploads` with `{"expectedBytes": <size>}` (1 byte–2 GiB), stream the raw bytes with `PUT` to the returned `uploadUrl`, then `POST` to `completeUrl`. Completion applies the CLI importer’s ffprobe and media limits and is idempotent; `DELETE /v1/assets/uploads/{uploadId}` cancels an incomplete session. Up to four sessions and 2 GiB of aggregate reserved upload data are allowed, and sessions survive API restarts. `POST /v1/jobs/{id}/cancel` is idempotent. `POST /v1/jobs/{id}/retry` accepts `{idempotencyKey}` and creates a new job pinned to the same revision and preset; retries are idempotent and limited to two explicit retries of the latest failed job. If the API stops during a render, the worker cancels the subprocess and leaves the job queued for a later server start; expired worker leases are recovered and partial attempt files are removed.

Render jobs expose `phase` (`queued`, `preparing`, `encoding`, `validating`, `publishing`, or `complete`) and `progressPercent`; encoding progress is estimated from FFmpeg frame reports and transitions to validation/publishing before completion. The CLI, API, and MCP render timeout defaults to 1800 seconds (30 minutes); `--timeout-seconds` accepts values up to 3600.

Upload sessions persist across server restarts but expire after 24 hours without activity. Expired sessions return `410 UPLOAD_EXPIRED`; a new upload request removes expired staging before checking the upload quota.

### Agent-first workflow

Before editing, agents SHOULD GET `/v1/capabilities`. It lists supported edit operations, canvas/render presets, limits, tool fingerprints, font availability, and the timeline schema/OpenAPI URLs. Prefer seconds-based operations so the server converts time to frames and source timestamps:

```json
{
  "expectedRevision": 1,
  "idempotencyKey": "storyboard-pass-1",
  "operations": [
      {"type": "clip.append", "assetId": "<video-asset-id>", "sourceInSeconds": 1.2, "durationSeconds": 4.5},
      {"type": "image.add", "assetId": "<image-asset-id>", "durationSeconds": 3},
    {"type": "audio.add", "assetId": "<audio-asset-id>", "startSeconds": 0, "durationSeconds": 12, "gain": 0.65},
    {"type": "title.add", "text": "A better way to connect", "startSeconds": 0.5, "durationSeconds": 3}
  ]
}
```

On `409 REVISION_CONFLICT`, use `error.details.currentRevision`, re-read the timeline, and submit a new idempotency key. The same key with the same request is safe to replay; a different request with that key conflicts. `GET /v1/jobs?projectId=...` recovers jobs if the caller loses a job ID. `GET /v1/jobs/{id}/inspection` returns QC findings and still-frame URLs; `GET /v1/jobs/{id}/stills/{frame}` returns a JPEG. Do not treat a completed render as approved until inspection reports `qc.passed`.

Append an imported still image with `image.add`; `durationSeconds` defaults to five seconds. `clip.crop` works on either video or still-image clips and uses a normalized source rectangle. For example, `{"type":"clip.crop","clipId":"<visual-clip-id>","crop":{"x":0.25,"y":0,"width":0.5,"height":1}}` keeps the middle half of the source width. Use `{ "x": 0, "y": 0, "width": 1, "height": 1 }` to restore the full frame. Uncropped media keeps the existing fit-and-pad behavior.

Set an incoming crossfade on a video or image clip with `{"type":"clip.transition.set","clipId":"<incoming-clip-id>","durationFrames":24}`; remove it with `clip.transition.remove`. The first visual clip cannot have an incoming transition. A crossfade is limited to five seconds and cannot exceed either adjacent clip's duration. The timeline stores the resulting overlap in the incoming clip's `transitionIn` and `startFrame`; the shared edit executor reflows the affected visual sequence.

Set linear audio fades with `{"type":"audio.fade.set","clipId":"<audio-clip-id>","fadeInFrames":12,"fadeOutFrames":12}`. Durations use project frames, each may be zero, and together they must not exceed the clip duration. Set both to zero to remove fades; each fade is limited to five seconds.

For MCP, configure your client to start this local stdio process (paths vary by machine):

```powershell
.\target\release\studio.exe mcp serve --database .\workspace\projects.sqlite --workspace .\workspace `
  --ffmpeg 'C:\tools\ffmpeg\bin\ffmpeg.exe' --ffprobe 'C:\tools\ffmpeg\bin\ffprobe.exe' `
  --font 'C:\Windows\Fonts\arial.ttf'
```

Tools include `capabilities`, `import_media`, `list_assets`, `create_project`, `get_timeline`, `edit_timeline`, `list_jobs`, `render`, `get_job`, and `inspect_output`. MCP inspection returns inline still images; a successful job includes a local `outputFile` path for same-machine agents. Stdio is local process access, not a network service, and runs with the permissions of its OS user.

## Current limits

- One enabled visual track containing video and still-image clips with no gaps; overlaps are allowed only for a typed incoming crossfade of at most five seconds that fits both adjacent clips. Images are PNG, JPEG, BMP, WebP, or TIFF and hold for five seconds by default.
- Audio clips can overlap and mix, with optional bounded linear fade-in/out envelopes. Text overlays are supported; captions, effects, and non-default transforms are rejected.
- Even canvas dimensions up to 1080p, 24/25/30 or 30000/1001 fps, 48 kHz output audio, five-minute project limit.
- Local media only; imports are capped at 2 GiB. Render and probe subprocesses have timeouts (render default: 30 minutes) and generated FFmpeg arguments; arbitrary filtergraph strings and remote URLs are not accepted.
- Text requires a user-supplied local font. Fonts and media are untrusted inputs to native parsers; use files you trust.

The local SQLite revision/edit/restore/render workflow, asset-based project bootstrap, seconds-based edit operations, persisted export/preview jobs, source and render-output asset retrieval, bounded source-media uploads, retry/inspection routes, capabilities/OpenAPI documents, and stdio MCP tools are implemented Release A slices. The React editor uses the same API operations for its supported intermediate timeline workflow. Full editor playback parity and automated browser editing journeys remain planned.
