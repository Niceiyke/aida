# AI Video Studio

Local-first video studio. The CLI stages media, validates JSON timelines against the bundled [`1.0.0` schema](schemas/timeline-1.0.0.schema.json), and renders H.264/AAC MP4 with FFmpeg. An authenticated loopback API and React editor support a local media bin, revisioned video/audio/title edits, preview/export jobs, and verified downloads. MCP and full NLE parity are not implemented.

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

The FFmpeg integration tests run when `ffmpeg` and `ffprobe` are on `PATH`. They cover synthetic H.264 and H.265/VFR media, portrait scaling, preview downscaling, a 70-second audio source, exact output frame count/rate, audio onset, failed-render cleanup, SQLite edit rollback/idempotency/restart rendering, job recovery/cancellation/retries/atomic publication, output asset streaming, and (on Unix) symlink rejection. H.265 is skipped when the installed FFmpeg has no `libx265` encoder. If the tools are elsewhere, set their paths (and optionally a font) before testing:

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

Import copies media into a workspace using a generated asset ID and returns JSON containing the URI and checksum:

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

Apply a JSON batch using the expected revision and an idempotency key. The request file has an `operations` array and currently supports `audio.gain.set` and `text.update`:

```json
{
  "operations": [
    {"type": "audio.gain.set", "clipId": "music-clip", "gain": 0.75},
    {"type": "text.update", "clipId": "brand-title", "text": "Updated title"}
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

The database is local and single-user. The API and shared operation executor expose revision history/restore and typed clip, audio-gain, and title edits; MCP and audit records remain future work.

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

Open `http://127.0.0.1:5173`, paste the API token, and connect. Import multiple video/audio assets, then create a project from a video in the media bin or reopen a recent project. The editor supports one contiguous hard-cut video track, two audio tracks, and a title track: add/reorder/split/trim/remove clips, move audio/title clips, adjust audio gain and title properties, and undo/redo through immutable revisions. Queue a rendered preview or full-quality export and download the verified MP4 when the job succeeds. Source playback is labelled approximate for composite timelines; the rendered preview is authoritative for its revision. The token is held in browser session storage only; media, project data, and renders stay in the configured local workspace. Transitions, effects, captions/images, synchronized proxy playback, TTS, MCP, and collaboration remain deferred. For a different API port, set `VITE_API_TARGET` before `npm run dev`.

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

After uploading media or staging it with `studio import`, POST a revision-0 timeline JSON to `/v1/projects`; then GET the project or a pinned timeline revision, POST an edit batch to `/v1/projects/{id}/operations`, inspect `/v1/projects/{id}/revisions`, restore an older snapshot, or queue a render. The API runs one SQLite-backed render worker; render jobs pin an immutable revision and support `export` and `preview` presets. For example, from another terminal:

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

Upload sessions persist across server restarts but expire after 24 hours without activity. Expired sessions return `410 UPLOAD_EXPIRED`; a new upload request removes expired staging before checking the upload quota.

## Current limits

- One enabled video track with contiguous, cut-only clips; no gaps, overlaps, transitions, or speed changes.
- Audio clips can overlap and mix. Text overlays are supported; captions, images, effects, and non-default transforms are rejected.
- Even canvas dimensions up to 1080p, 24/25/30 or 30000/1001 fps, 48 kHz output audio, five-minute project limit.
- Local media only; imports are capped at 2 GiB. Render and probe subprocesses have timeouts and generated FFmpeg arguments; arbitrary filtergraph strings and remote URLs are not accepted.
- Text requires a user-supplied local font. Fonts and media are untrusted inputs to native parsers; use files you trust.

The local SQLite revision/edit/restore/render workflow, revision history, persisted export/preview jobs, source and render-output asset retrieval, bounded source-media uploads, retry routes, and loopback project/job API are initial Release A slices. The React editor uses the same API operations for its supported intermediate timeline workflow. Full editor playback parity and MCP remain planned.
