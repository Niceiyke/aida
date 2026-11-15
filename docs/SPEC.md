# AI Video Studio — Technical Specification

**Version:** 1.2 | **Audience:** Rust/React implementers and AI coding agents

This document is the canonical wire/API contract. The PRD defines release scope; the implementation plan defines order. Earlier examples and later sections do not override each other.

## 1. Contract rules
- Project, asset and job IDs are UUIDv7 or UUIDv4 serialized as strings; track, clip and operation IDs are nonempty project-scoped strings.
- API dates use RFC 3339 UTC. Frame positions/durations are nonnegative integers.
- `fps = {num, den}` with both positive and reduced; 30000/1001 is valid.
- Rational source timestamp is `{ticks: integer, timebase_num: integer, timebase_den: integer}` with positive denominator; negative timestamps disallowed for clips in v1.
- JSON schema is authoritative and MUST be checked on API ingress and before rendering. The versioned timeline schema lives at `schemas/timeline-1.0.0.schema.json`; rendering also applies semantic checks that JSON Schema cannot express (asset availability, source duration, track layout and renderer support).
- Use discriminated unions for clip/effect/operation types; reject unknown types rather than ignore.
- Timeline clip intervals are `[startFrame, startFrame + durationFrames)`; source intervals are `[sourceIn, sourceOut)`. At speed 1, the rational source duration and `durationFrames * fps.den / fps.num` seconds must differ by no more than half an output frame. Trims MUST specify `durationFrames` (or be rejected); the server never silently changes speed. At 30 fps, a source trim from 1s to 4s requires `durationFrames: 90`.
- Convert output frame boundary `n` to audio sample boundary `floor(n * audioSampleRate * fps.den / fps.num)`; use rational arithmetic, not floating-point accumulation. VFR sources are located by source timestamps. Resampling and frame rounding must use this policy in both tests and exports.

### 1.1 First-build CLI contract
- `studio import --workspace <dir> --file <local-path> [--ffprobe <executable>]` stages a human-selected local MP4/MOV video or MP3/WAV/AAC audio file, probes it and returns JSON containing `assetId`, `uri`, `sha256`, kind and media metadata. The workspace stores the file and manifest under a generated UUID; user paths never enter timeline JSON. Imports are limited to 2 GiB, five minutes, 4K source video and 192 kHz/eight-channel audio. Remote URLs and unsupported containers are rejected.
- `studio render --workspace <dir> --timeline <json-path> --output <local-mp4> [--ffmpeg <executable>] [--ffprobe <executable>] [--font <font-file>] [--timeout-seconds 300]` validates against the bundled JSON Schema and asset manifests, compiles a typed render plan into generated FFmpeg arguments, renders to a same-directory temporary file, probes codec/dimensions/rate/duration/frame count and publishes it without overwriting existing files. It writes `<output>.provenance.json` with timeline/asset/font digests, renderer/FFmpeg identities, preset and output digest. No shell or user filtergraph is accepted.
- Timeline files are UTF-8 JSON, optionally with a UTF-8 BOM, no larger than 2 MiB. An unpersisted timeline used to create a project has `revision: 0`; project creation stores revision 1. The original JSON-to-render workflow remains available.

### 1.2 First-build supported timeline subset
- One enabled video track, with clips contiguous from frame 0 (hard cuts only); source ranges are rational and must match project duration within half a frame. A gap, overlap or non-default transform fails validation.
- Zero or more enabled audio tracks; clips may overlap and mix with linear gain `0..4`. Audio output is 48 kHz AAC. Start offsets use the exact frame-to-sample floor rule above.
- Zero or more overlay tracks containing text clips shaped as `{kind:"text", id, startFrame, durationFrames, text, fontSize?, color?, x?, y?}`. Defaults are 48 px, white, x=0, y=80. A trusted local `.ttf`, `.otf`, or `.ttc` is supplied with `--font`; text is passed through a staged text file, not interpolated into a filter string.
- Video clips may include the canonical default identity transform and an empty `effects` list; other effects, captions, images, speed changes and transitions are rejected rather than ignored. Odd canvas dimensions are rejected. Canvas is at most 1080p, with 24/25/30 or 30000/1001 fps.
- Named presets accepted by the first build are `landscape_720p30` (1280x720), `landscape_1080p30` (1920x1080), `vertical_1080p30` (1080x1920) and `square_1080p30` (1080x1080), all at 30 fps. A preset must match the timeline canvas exactly; if omitted the canvas is treated as custom.

### 1.3 Local revisioned CLI slice
- `studio project create --database <sqlite-path> --workspace <dir> --timeline <json-path>` validates a revision-0 timeline and creates the project and its immutable revision 1 in one SQLite transaction.
- `studio edit --database <sqlite-path> --workspace <dir> --project-id <uuid> --expected-revision <n> --idempotency-key <key> --operations <json-path>` accepts a JSON object shaped as `{"operations":[...]}`. It validates the complete resulting timeline before storing a new immutable snapshot. Keys are unique per project; an identical retry returns the original revision, a changed request with the same key conflicts, and a new stale request conflicts. The local CLI has a stable single-user identity, so actor/endpoint scoping is not needed in this slice.
- Implemented operation schemas: `text.update` accepts `clipId` and at least one of `text`, `fontSize`, `color`, `x`, or `y`; `audio.gain.set` accepts `clipId` and `gain` in `0..=4`. Both reject locked tracks. Other operation types are rejected.
- `studio render-revision --database <sqlite-path> --workspace <dir> --project-id <uuid> --revision <n> --output <local-mp4> [...]` renders a stored snapshot through the same validator, renderer and provenance flow as `studio render`.
- This is only the local persistence slice of Release A: it does not provide MCP, operation/audit history, revision restore or persisted jobs. A two-writer same-revision conflict test exists; load/stress coverage remains future work.

### 1.4 Loopback REST API slice
- `studio api serve --database <sqlite-path> --workspace <dir> [--bind 127.0.0.1:8787]` starts the local API. The bind address MUST be loopback. `STUDIO_API_TOKEN` is required and MUST contain at least 32 bytes; each request MUST send `Authorization: Bearer <token>`. Request bodies are limited to 2 MiB.
- `POST /v1/projects` accepts a revision-0 timeline JSON document directly and returns `{projectId, revision}` with status 201. It uses the same schema, asset-manifest and semantic validation as the CLI.
- `GET /v1/projects/{id}` returns `{projectId, currentRevision}`. `GET /v1/projects/{id}/timeline` returns the current timeline; `?revision=<n>` retrieves a pinned immutable snapshot.
- `POST /v1/projects/{id}/operations` accepts `{expectedRevision, idempotencyKey, operations}` and returns `{projectId, revision, replayed}`. It uses the same operation executor, validation, idempotency and conflict rules as `studio edit`.
- This API slice does not upload media, render output, create jobs, expose MCP, or bind to non-loopback interfaces.

## 2. Canonical timeline JSON example
```json
{
  "schemaVersion":"1.0.0",
  "projectId":"018f2e40-4d28-7e31-8b63-611d49c43b0a",
  "revision":3,
  "canvas":{"width":1080,"height":1920,"fps":{"num":30,"den":1},"background":"#09090B"},
  "assets":{"018f2e40-4d28-7e31-8b63-611d49c43b0b":{"kind":"video","uri":"asset://018f2e40-4d28-7e31-8b63-611d49c43b0b","sha256":"<64-hex-digest>"}},
  "tracks":[
    {"id":"t1","kind":"video","order":0,"enabled":true,"locked":false,
      "clips":[{"id":"c1","kind":"video","assetId":"018f2e40-4d28-7e31-8b63-611d49c43b0b","startFrame":0,"durationFrames":150,
       "sourceIn":{"ticks":0,"timebase_num":1,"timebase_den":90000},
       "sourceOut":{"ticks":450000,"timebase_num":1,"timebase_den":90000},
       "transform":{"x":0,"y":0,"scaleX":1,"scaleY":1,"rotationDeg":0,"opacity":1},"effects":[]}]}
  ],
  "settings":{"audioSampleRate":48000,"colorSpace":"rec709","exportPreset":"vertical_1080p30"}
}
```
This example illustrates the Release A wire shape, not a complete runnable fixture; the referenced asset must exist and its source range must be verified. For the first CLI build set `revision` to 0 and provide the internal asset manifest described above. The timeline never contains an arbitrary filesystem path. The editing-command example below assumes a `titles` track and `brand-title` style already exist.

### 2.1 Track and clip rules
- Track kinds: `video`, `audio`, `overlay`, `caption`; order ascending, higher visual order composites on top.
- Clip kinds: `video`, `image`, `audio`, `text`, `caption`. Media clips require assetId; generated text/caption clips require typed content payload.
- `startFrame >= 0`, `durationFrames > 0`, source out > source in; source range must be inside probed media duration, except still images.
- For speed=1, source duration and project duration must match within defined rounding tolerance; speed effects alter this mapping explicitly.
- Video overlaps require explicit `composite` or `transition` behavior; audio clips may overlap and mix.
- Clip references, track references and asset references must resolve within the same authorized project.
- Effect union v1: `opacity`, `scale_crop`, `position`, `rotate`, `fade_in`, `fade_out`, `crossfade`, `text_style`, `audio_gain`; each has typed limits and preview/export support flags.

## 3. Editing command protocol
```json
{
  "expectedRevision":3,
  "idempotencyKey":"promo-edit-0004",
  "operations":[
    {"type":"clip.trim","clipId":"c1","sourceIn":{"ticks":90000,"timebase_num":1,"timebase_den":90000},"sourceOut":{"ticks":360000,"timebase_num":1,"timebase_den":90000},"durationFrames":90},
    {"type":"text.add","trackId":"titles","startFrame":15,"durationFrames":90,"text":"Your Church. Beautifully Connected.","styleId":"brand-title"}
  ]
}
```
**Semantics:** validate operation schema, permission, revision, asset availability and all postconditions; commit as one transaction; return `{projectId, revision, operationIds}`. Idempotency identity is `(workspaceId, actorId, endpoint, idempotencyKey)` with a canonical request digest and persisted result; for single-user CLI calls use a stable local actor ID. Check an existing key before checking the expected revision: same key + same payload returns the original result even after later revisions; same key + different payload returns HTTP 409 `IDEMPOTENCY_CONFLICT`. A new key with a stale revision returns HTTP 409 `REVISION_CONFLICT` with current revision. Serialize concurrent writes so only one writer can advance a given revision. The entire resulting timeline is validated before any change is committed.

### 3.1 Operation inventory
`track.add/remove/reorder`, `clip.add/remove/move/trim/split`, `clip.properties.set`, `effect.add/update/remove`, `transition.add/remove`, `text.add/update`, `caption.add/update`, `audio.gain.set`, `audio.fade.set`, `project.settings.update`. These are dotted operation names, e.g. `clip.move`, not `move_clip`. Each implemented operation MUST have a schema, validator, undo-via-new-revision behavior and unit tests. Locked tracks reject mutations. `revision.restore` is a separate restore request with `expectedRevision` and `idempotencyKey`, creating a new revision from the chosen snapshot.

## 4. REST API
| Method | Endpoint | Purpose |
|---|---|---|
| POST | `/v1/projects` | Create project |
| GET | `/v1/projects/{id}` | Get metadata |
| GET | `/v1/projects/{id}/timeline` | Get timeline/revision |
| POST | `/v1/projects/{id}/operations` | Atomic edits |
| GET | `/v1/projects/{id}/revisions` | Revision history |
| POST | `/v1/projects/{id}/restore` | New revision from prior snapshot; body `{targetRevision, expectedRevision, idempotencyKey}` |
| POST | `/v1/assets/uploads` | Begin validated upload (Release A; CLI imports local files) |
| POST | `/v1/assets/uploads/{id}/complete` | Finalize and enqueue probe |
| GET | `/v1/assets/{id}` | Asset and derivatives |
| GET | `/v1/assets/{id}/content` | Download authorized source, preview or output artifact |
| POST | `/v1/projects/{id}/renders` | Queue an export or preview pinned to a revision |
| GET | `/v1/jobs/{id}` | Render job status, diagnostics and output details |
| POST | `/v1/jobs/{id}/cancel` | Idempotent cancellation |
| POST | `/v1/jobs/{id}/retry` | Explicit retry for failed render job within policy |
| GET | `/v1/jobs/{id}/events` | SSE status (optional; polling is sufficient for A) |
| GET | `/v1/voices` | Available provider voices |
| POST | `/v1/voices/preview` | Queue short preview |
| POST | `/v1/voice-jobs` | Queue synthesis |
| GET | `/v1/voice-jobs/{id}` | Status/output/alignment |
| POST | `/v1/transcriptions` | Queue transcript/alignment |
| GET | `/v1/health` | Liveness |

REST routes in this table first appear in Release A, except P1 voice/transcription and the optional SSE route. `GET /v1/projects/{id}/revisions` returns revision summaries newest-first, including creation time and whether each is current. `POST /v1/projects/{id}/restore` accepts `{targetRevision, expectedRevision, idempotencyKey}` and creates a new immutable revision from the selected snapshot; it does not change or delete prior revisions. It is idempotent, and a stale expected revision returns `REVISION_CONFLICT` with `details.currentRevision`. The local API accepts `{revision, presetId: "export" | "preview", idempotencyKey}` at `POST /v1/projects/{id}/renders`; the revision is immutable and pinned, enqueue is idempotent, and unsupported presets are rejected. `export` retains the timeline canvas and export encoder settings. `preview` scales the canvas down to a maximum 640-pixel edge without upscaling, maintains the aspect ratio within even dimensions, and uses a faster/lower-quality H.264 encoder configuration; frame rate, duration, and audio are unchanged. `GET /v1/jobs/{id}` returns persisted status and returns `outputAssetId` only after successful validation and atomic publication; it does not expose a filesystem path. `GET /v1/assets/{id}` returns metadata and probe information for imported source assets, or output metadata including `presetId` for render assets. `/content` validates the opened regular file's size and SHA-256 against recorded metadata before streaming from that same handle, preventing path replacement after validation from changing which file is streamed. Source assets are currently imported through the CLI; API uploads remain future work. `POST /v1/jobs/{id}/cancel` cancels queued jobs immediately or requests cancellation of running work. `POST /v1/jobs/{id}/retry` accepts `{idempotencyKey}` and creates a new job pinned to the same revision and preset; only the latest failed job in a retry chain may be retried, at most twice, and repeated keys return the same job. The single in-process worker requeues an expired running job after restart, cleans that attempt's files, and fails recovery after the bounded automatic-attempt limit. The CLI first build uses the same core without requiring HTTP. MCP maps to these endpoints without introducing a second command vocabulary.

### 4.1 Standard error
```json
{"error":{"code":"REVISION_CONFLICT","message":"Expected revision 3; current revision 4","requestId":"req-123","details":{"currentRevision":4},"retryable":false}}
```
Codes include `VALIDATION_FAILED`, `ASSET_NOT_FOUND`, `ASSET_NOT_READY`, `UNSUPPORTED_CODEC`, `UNSUPPORTED_EFFECT`, `REVISION_CONFLICT`, `IDEMPOTENCY_CONFLICT`, `JOB_NOT_RETRYABLE`, `JOB_RETRY_LIMIT`, `PERMISSION_DENIED`, `QUOTA_EXCEEDED`, `PROVIDER_UNAVAILABLE`, `RENDER_FAILED`, `JOB_CANCELLED`. Use 400/401/403/404/409/422/429/5xx consistently. For revision conflicts, include `details.currentRevision`; for reused keys, return `IDEMPOTENCY_CONFLICT` (never `IDEMPOTENCY_KEY_REUSED`).

## 5. Job state machines
Render: `queued -> running -> succeeded`; `queued -> cancelled`; `running -> failed | cancelled | queued` (lease recovery or bounded automatic retry); `failed -> queued` only on an explicit retry within policy. `preparing`, `rendering` and `validating` are phases of `running`, not public states. A job records pinned revision, timeline/asset digests, renderer/FFmpeg fingerprint, attempt, cancellation request, progress, phase, timestamps, error code and output asset ID on success. Progress is best-effort and monotonic within an attempt; terminal states are authoritative. The first synchronous CLI build does not need a job table; Release A does.
Voice (P1): the same public states with `synthesizing`, `normalizing` and `aligning` as phases. If alignment fails, completion without word timing is permitted only if the request allows it and a warning is returned; never fabricate precise timestamps.

## 6. Render IR specification
```rust
struct RenderPlan {
    project_revision: u64,
    fps: Rational,
    width: u32,
    height: u32,
    nodes: Vec<RenderNode>,
    audio_sample_rate: u32,
    output: OutputPreset,
}
// RenderNode is a tagged enum: Source, Trim, ScaleCrop, Position,
// Composite, Fade, Text, Gain, Mix, Caption, Encode.
```
Compiler passes: resolve assets; normalize time; verify graph acyclic; compute output time ranges; validate effect support; lower to FFmpeg filters; construct argv array (never shell); execute; inspect output via ffprobe. Generated filters MUST escape arbitrary user text and paths; prefer text files for complex captions. IR nodes are tagged/versioned, with a shared effect registry defining parameter limits, interpolation and preview/export support. Unsupported effects fail before FFmpeg starts. Only validated, pinned local staged paths reach FFmpeg.

## 7. Voice provider interface and contracts
```rust
#[async_trait::async_trait]
pub trait VoiceProvider: Send + Sync {
    async fn list_voices(&self) -> Result<Vec<VoiceDescriptor>, VoiceError>;
    async fn synthesize(&self, req: VoiceRequest) -> Result<VoiceResult, VoiceError>;
}
```
```json
{
 "projectId":"project-uuid",
 "provider":"configured-provider",
 "voiceId":"provider-voice-id",
 "text":"Introducing Wordlyte Pro.",
 "language":"en",
 "settings":{"speed":1.0,"style":"professional"},
 "output":{"format":"wav","sampleRate":48000,"align":true,"insertIntoTimeline":false},
 "idempotencyKey":"voice-job-001"
}
```
- Voice listing returns provider, voiceId, display name, language/locale, capabilities (`speed`, `style`, `timestamps`, `streaming`) and availability.
- Reject unsupported settings with `VALIDATION_FAILED` and capability details; do not silently drop parameters.
- Provider audio is normalized to project PCM workflow (48 kHz WAV derivative) and preserved as immutable source asset when licensing allows.
- Alignment returns word/segment timestamps in source audio sample or rational time; if provider lacks timing, enqueue forced alignment or transcription.
- Voice profile: `{id, name, provider, voiceId, locale, speed, style?, pronunciationHints?, consentRef?}`; API credentials are references to server secrets, never embedded in profile.
- TTS output cache keyed by text hash + provider + model/version + voice + settings; access scoped by owner/tenant.
- Voice cloning excluded from v1; consent and provenance required before future enablement.

## 8. Media intelligence and narration-driven edit
Transcription result: `{assetId, language, segments:[{start, end, text, confidence?, words?}]}`; scene analysis: `{assetId, scenes:[{start, end, score?}]}`. Search API ranks transcript spans and returns source time ranges, not timeline positions. Narration workflow: script -> voice job -> alignment -> beat/segment map -> edit decision list -> timeline operations -> preview -> QC. Agent may propose visuals but server validates source ranges and overlap semantics.

## 9. Browser preview synchronization
Player clock is mapped to integer project frames; each clip maps project time to source rational timestamp. On seek, pause/synchronize relevant HTML media elements and update overlays. Drift above a configurable threshold triggers resync; complex effects are cached as proxy segments. `previewRevision` must equal timeline revision or UI shows stale preview. Golden tests compare sampled frames at cuts, transitions and caption boundaries against export.

## 10. Persistence and security constraints
- The original JSON-to-render CLI works without a database. The local revisioned CLI slice uses SQLite migrations and transactions: unique `(project_id, revision)`, unique `(project_id, idempotency_key)`, immutable revision triggers and a canonical request digest. It serializes writes with a SQLite immediate transaction. A future multi-actor API may extend idempotency scope to `(workspace_id, actor_id, endpoint, idempotency_key)`. PostgreSQL is not required for the personal-project baseline.
- By Release A, persist job ownership/lease expiry, attempt, cancellation requested, output checksum and engine version so interrupted jobs can be recovered; one in-process runner is sufficient initially.
- From the first local import/render: size limits, MIME sniffing, isolated staged paths, bounded probe/render subprocesses and no arbitrary user FFmpeg filters or remote URL inputs. Release A adds scoped asset access and secrets redaction; an antivirus hook is optional where deployed. External URLs are disabled unless an allowlisted downloader with SSRF protections is built.
- Provider rate limits, per-project cost budgets, timeout and retries with jitter.

## 11. Minimum test matrix
1. Frame math at 24/25/30/30000:1001/60 fps and VFR source clips.
2. Reject out-of-range trims, missing assets, locked tracks, invalid effects and ambiguous overlaps.
3. Batch atomicity, concurrent edit conflict, idempotent retries and revision restore.
4. Mixed video/audio export with subtitles and voiceover; inspect codec/duration/sync.
5. TTS provider timeout, unsupported style, cache hit, normalization, alignment fallback.
6. Job lease expiry, cancellation, crash recovery and output quarantine.
7. Preview seeks across cuts and captions; export golden-frame comparison.
8. Malicious filenames, user text escaping, malformed media and forbidden remote URLs.

## Normative conventions
- **MUST** is mandatory for the indicated release; **SHOULD** is recommended; **MAY** is optional.
- P0a is required for the first local export; P0b completes Release A; P1 for AI Production; P2 for Hybrid Editor.
- Versioned API root: `/v1`; timeline schema version: `1.0.0`.
- All times in the canonical timeline are integer project frames; media source positions use rational timestamps. Audio sample positions are retained for sample-accurate processing.
- From Release A, all command/API timeline mutations (including agent edits and restores) use expected project revision and an idempotency key; render requests pin a revision and have their own idempotency key. The first CLI build validates a manually edited JSON file instead.

## 12. Release-specific verification
- **First local build:** headless 15-second composition, ffprobe validation, exact frame-boundary tests, exact encoded frame count/rate, audio onset checks, H.265/VFR portrait input, longer audio input, malformed media rejection, subprocess-group timeout and safe local staging. Linux and Windows headless CI require no display server.
- **Local revisioned CLI slice:** invalid edit batches leave the current revision unchanged; stale revisions conflict; identical idempotent retry returns its original revision; reused keys with different payloads conflict; a stored revision renders after the database is reopened.
- **Remaining Release A gate:** preview presets/output download and MCP access to a rendered preview without a browser; run the Linux and Windows GitHub Actions checks. The first export-job API slice already covers revision pinning, idempotent enqueue, polling, cancellation, restart recovery and atomic output publication.
- **Release B:** provider timeout, duplicate request, unsupported language/style and missing alignment all have stable results.
- **Release C:** compare golden frames at transitions/text/caption boundaries; audio onset remains within the declared sample/frame tolerance and stale proxies are visibly labelled. Keep fixtures, fonts and calibrated visual thresholds with the tests.
