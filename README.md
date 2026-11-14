# AI Video Studio

Early local-first build of a headless video editor. The CLI stages local MP4/MOV video and MP3/WAV/AAC audio, validates JSON timelines against the bundled [`1.0.0` schema](schemas/timeline-1.0.0.schema.json), renders H.264/AAC MP4 with FFmpeg, writes provenance sidecars, and optionally stores projects as immutable SQLite revisions. A small authenticated REST API is available on loopback; MCP, persisted render jobs, and the browser editor are not implemented yet.

## Requirements

- Rust toolchain (edition 2024)
- Recent `ffmpeg` and `ffprobe` on `PATH`, or pass their executable paths explicitly
- An FFmpeg build with `libx264`, AAC, and `drawtext` support for text overlays
- A `.ttf`, `.otf`, or `.ttc` font file when the timeline contains text

## Build and test

```powershell
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

The FFmpeg integration tests run when `ffmpeg` and `ffprobe` are on `PATH`. They cover synthetic H.264 and H.265/VFR media, portrait scaling, a 70-second audio source, exact output frame count/rate, audio onset, failed-render cleanup, SQLite edit rollback/idempotency/restart rendering and (on Unix) symlink rejection. H.265 is skipped when the installed FFmpeg has no `libx265` encoder. If the tools are elsewhere, set their paths (and optionally a font) before testing:

```powershell
$env:STUDIO_TEST_FFMPEG = 'C:\tools\ffmpeg\bin\ffmpeg.exe'
$env:STUDIO_TEST_FFPROBE = 'C:\tools\ffmpeg\bin\ffprobe.exe'
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

The database is local and single-user; MCP, jobs, revision restore, and additional edit operation types remain future work.

### Loopback REST API

Start the API with a random token of at least 32 bytes. It refuses non-loopback bind addresses and requires `Authorization: Bearer <token>` on every route. Keep the server terminal open; set the same token in the client terminal.

```powershell
$token = [guid]::NewGuid().ToString('N') + [guid]::NewGuid().ToString('N')
$token # copy this value into the client terminal
$env:STUDIO_API_TOKEN = $token
.\target\release\studio.exe api serve --database .\workspace\projects.sqlite `
  --workspace .\workspace
```

After staging media with `studio import`, POST a revision-0 timeline JSON to `/v1/projects`; then GET the project or a pinned timeline revision, or POST an edit batch to `/v1/projects/{id}/operations`. For example, from another terminal:

```powershell
$env:STUDIO_API_TOKEN = '<paste the same token>'
$headers = @{ Authorization = "Bearer $env:STUDIO_API_TOKEN" }
$project = Invoke-RestMethod -Method Post -Uri 'http://127.0.0.1:8787/v1/projects' `
  -Headers $headers -ContentType 'application/json' -InFile .\timeline.json
Invoke-RestMethod -Method Get `
  -Uri "http://127.0.0.1:8787/v1/projects/$($project.projectId)/timeline?revision=1" `
  -Headers $headers
```

The API exposes project creation, current project metadata, current or pinned timelines, and atomic operations only. It does not accept media uploads or render requests; use the local import and render commands for those workflows.

## Current limits

- One enabled video track with contiguous, cut-only clips; no gaps, overlaps, transitions, or speed changes.
- Audio clips can overlap and mix. Text overlays are supported; captions, images, effects, and non-default transforms are rejected.
- Even canvas dimensions up to 1080p, 24/25/30 or 30000/1001 fps, 48 kHz output audio, five-minute project limit.
- Local media only; imports are capped at 2 GiB. Render and probe subprocesses have timeouts and generated FFmpeg arguments; arbitrary filtergraph strings and remote URLs are not accepted.
- Text requires a user-supplied local font. Fonts and media are untrusted inputs to native parsers; use files you trust.

The local SQLite revision/edit/render workflow and loopback project API are initial Release A slices. MCP, persisted render jobs, revision restore, and the React editor are not implemented yet.
