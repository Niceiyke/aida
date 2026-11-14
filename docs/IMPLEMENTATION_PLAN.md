# AI Video Studio — Implementation Plan

**Version:** 1.2 | **Delivery model:** solo developer + optional coding agents | **Estimate:** milestone-driven; no fixed calendar commitment

## First usable build — one-page checklist
**Goal:** From approved local files and a structured JSON timeline, produce and verify a 15-second two-clip H.264/AAC MP4 without a browser, server, TTS provider or handwritten FFmpeg command.

- [x] Provide reproducible synthetic video, music and narration-placeholder fixtures plus a font and document local FFmpeg/ffprobe commands; the same CLI accepts the user's approved media.
- [x] Define project frames, rational source ranges, a machine-readable timeline JSON Schema with valid/invalid fixtures, and semantic validation; use the rounding/trim policy in `SPEC.md`.
- [x] Create a CLI that imports local files into isolated staging, probes them, writes an internal asset-ID manifest, and rejects oversized/invalid/remote inputs.
- [x] Generate a sample JSON timeline with two clips, title and audio; validate bounds, time mapping and references before rendering. The file can then be hand-edited without a DB or revision API.
- [x] Compile a typed render plan to validated FFmpeg arguments. Bound probe/render time and output size; kill subprocess groups on timeout; never run a shell or accept a user filtergraph. Verify codec, dimensions, duration, exact frame rate/count, then publish from a temporary path.
- [x] Document `studio import`/`studio render` commands and automated checks for duration, dimensions, H.264/AAC streams, audio onset within one frame and provenance (timeline/asset/font hashes, tool fingerprints and preset). Keep a known-limitations list.

**Done means:** a fresh checkout can reproduce the sample export through documented CLI commands and tests; no REST, MCP, queue, browser preview or provider credentials are required. This is P0a, not Release A.

## 1. Execution principles
- Build vertical slices that produce a working video, not isolated infrastructure.
- A phase cannot close without automated tests, documented commands and a reproducible demo.
- If coding agents are used, give them bounded issues with explicit files, acceptance tests and interfaces; the project owner reviews cross-crate changes.
- All rendering uses the validated canonical timeline and render IR. Once Release A adds revisioned operations, all CLI/API/MCP/GUI timeline commands use them; the first build can read an edited JSON file directly.
- Prefer CPU-compatible baseline; GPU acceleration, Tauri packaging and native playback are later optimizations.

## 2. Milestones and dependencies
| Phase | Depends on | Deliverable |
|---|---|---|
| 0: feasibility | none | FFmpeg no-GUI spike; identify codec and timing risks |
| 1: local core | 0 | Minimal timeline schema, time math and safe import |
| 2: first usable build (P0a) | 1 | CLI 15-second export and ffprobe validation |
| 3: agent API (P0b) | 2 | SQLite revisions/operations, in-process jobs, rendered preview files, REST/MCP and recovery; Release A |
| 4: AI production (P1) | 3 | TTS, captions, transcript search, brand kit, templates; Release B |
| 5: React editor (P2) | 3–4 | Browser playback, timeline edits, preview parity; Release C |
| 6: later hardening | as needed | Packaging, performance, backup and deployment improvements |

Milestones are evidence-driven rather than week-based. Phases 4 and 5 can overlap if time allows; basic safety is part of phases 1–2, not deferred to phase 6.

## 3. Phase 0 — feasibility spike
- [x] Add generated H.265/VFR portrait and >60-second audio fixtures to the test matrix; actual user-provided footage still needs broader compatibility testing.
- [x] Build a cut-based FFmpeg proof-of-concept: two clips, title and separate narration-placeholder/music tracks.
- [x] Measure frame-to-sample audio offset within one output frame; overlap/fade remains a later renderer test.
- [x] Document codec/font limits and add Linux/Windows headless CI jobs. Defer browser preview experiment until before Phase 5; it is not a gate for the first export.
**Gate:** no-GUI FFmpeg prototype renders a composition; known unsupported codecs and timing risks are documented.

## 4. Phase 1 — foundation
### Tasks
- [x] Bootstrap Cargo workspace and Linux headless formatting/lint/test CI; TypeScript workspace begins with the editor/API client.
- [x] Implement domain IDs, rational time arithmetic, project canvas and asset models for the first-build subset.
- [x] Define and runtime-check the versioned JSON Schema with valid/invalid fixtures; generate TypeScript client types when a TypeScript client is added.
- [x] Implement tagged track/clip types and validation; unsupported effects fail instead of being ignored.
- [x] Build minimal validated JSON fixtures for the first export; defer editing operations, revision persistence and SQLite migrations to Phase 3.
### Tests
- Unit tests cover rational frame/sample conversion and source-duration bounds; the integration test covers missing assets.
**Gate:** a two-clip JSON timeline with title/audio is validated and invalid ranges fail clearly; no database is needed yet.

## 5. Phase 2 — headless rendering
### Tasks
- [x] Implement safe local import, size/container checks and isolated staging; content-addressed storage and HTTP upload can arrive in Phase 3.
- [x] ffprobe adapter and stream metadata; thumbnails and browser proxies can wait for Phase 5.
- [x] Build a typed in-memory render plan and compiler passes for the first-build subset.
- [x] Generate FFmpeg argv/filter graph for trim, scale/pad, sequence, text, audio gain/mix. Fades and transitions follow in Release A; captions in B.
- [x] Bound FFmpeg/ffprobe execution, stage outputs, validate and publish atomically. Persisted job runner and cancellation arrive in Phase 3.
- [x] Probe output and check codec/duration/audio; write a local provenance sidecar. Release A registers the file as a downloadable asset.
### Tests
- [x] Integration tests cover two-clip video/audio/text export, audio onset within one frame, silent single-clip export, unsupported effects, missing assets, malformed import cleanup and subprocess timeout.
- [x] Add symlink/path traversal, malformed-container, process-group timeout and failed-render cleanup cases (Unix symlink cases run on Linux CI; additional fuzzing remains future hardening).
**Gate / first usable build:** CLI validates an edited JSON timeline and exports a 15-second multitrack 1080p30 MP4 without handwritten FFmpeg commands. No database, API or browser is required.

## 6. Phase 3 — agent interface
### Implemented first slice: local revisioned CLI
- [x] Add a transactional SQLite migration for projects, immutable timeline snapshots and idempotency records.
- [x] Add `studio project create`, `studio edit` and `studio render-revision`; implement `text.update` and `audio.gain.set` with full-timeline validation, stale-revision conflicts and idempotent replay.
- [x] Exercise invalid-batch rollback, conflicting retries, stale revisions, a concurrent same-revision race, persistence after reopening the database, and rendering a stored revision.

This remains a partial Release A. It uses bundled SQLite through `rusqlite`; the database is single-user, jobs/MCP are absent, and the HTTP layer is limited to authenticated loopback project/timeline/operation routes.

### Tasks
- [x] Add authenticated loopback REST routes for project creation/metadata, current or pinned timeline retrieval, and atomic operations.
- [ ] Publish OpenAPI contract and generated client.
- [ ] Extend persistence with API operation/audit records, revision restore and concurrent-writer load tests; keep CLI and API mutations on the same application service.
- [ ] Add remaining asset and render endpoints, plus project revision history.
- [ ] Add SQLite-backed in-process jobs, bounded retries, lease recovery, cancellation and status polling; rendered preview files use a preview preset and are retrievable through the asset endpoint.
- [ ] Implement MCP tools: create_project, import_media, get_timeline, edit_timeline, render, get_job, inspect_output.
- [ ] Add token scopes, audit trail, structured error mapping and usage limits.
- [ ] Build scripted agent test with revision conflict recovery.
**Gate / Release A:** an external agent creates, edits, retrieves a rendered preview file and exports a valid video through tools alone; jobs recover after restart without partial outputs.

## 7. Phase 4 — voice, media intelligence and templates
### Tasks
- [ ] Add `studio-voice` provider trait and capability schema.
- [ ] Integrate one TTS provider with validated voice quality, region, cost and licensing; add a local adapter later only if offline use is needed.
- [ ] Voice catalog, short previews, async synthesis jobs, caching, normalization and provenance.
- [ ] Brand voice profile CRUD and provider secret configuration.
- [ ] Alignment via provider timestamps or transcription/forced alignment fallback.
- [ ] Narration-first storyboard: script segments -> audio asset -> aligned clip suggestions -> operations.
- [ ] Transcription and transcript search; basic scene boundary detection.
- [ ] Caption track generation and burn-in; brand kit and 9:16, 16:9, 1:1 templates.
- [ ] Deterministic QC for duration, missing media, format, audio peaks/silence and caption bounds.
### Tests
- Provider timeout/unsupported capability, no duplicate synthesis, voice/timeline sync, brand consistency and complete agent-produced promo.
**Gate / Release B:** 30–60 second branded promo with generated narration, captions, music, rendered preview files, two bounded revision attempts and export.

## 8. Phase 5 — React editor
### Tasks
- [ ] Build media browser, thumbnail grid, preview monitor and proxy playback coordinator.
- [ ] Build multitrack timeline, selection, drag, trim, split, zoom and scrubbing.
- [ ] Add property inspector, title editor, audio gain and captions editing.
- [ ] Integrate revision history, undo/redo, stale-preview warning and render progress.
- [ ] Add accessible keyboard controls and error surfaces.
### Tests
- Playwright editing journeys; browser timing tests; golden-frame comparison at cut/transition/caption boundaries, with measured parity thresholds.
**Gate / Release C:** human corrects AI-created edit and exports it without editing JSON; stale or approximate preview is labelled.

## 9. Phase 6 — production hardening
- [ ] Improve subprocess sandboxing and deployment resource caps beyond the mandatory limits and path/URL protections already in phases 1–3.
- [ ] Concurrency/load tests, memory profiling, TTS cost limits and retention policies.
- [ ] FFmpeg build/codec compatibility matrix for Windows/Linux; macOS when targeted.
- [ ] Recovery tests for API/worker restart and storage failures.
- [ ] API docs, sample SDK scripts, migration/backup guide, operator runbook.
- [ ] Optional container packaging or separate worker only if needed; keep local installation simple.
**Gate:** document measured limits and recovery paths before expanding deployment scope.

## 10. First 10 implementable issues
| Issue | Area | Deliverable | Acceptance |
|---|---|---|---|
| AVS-001 | Platform | Rust workspace + CI | Rust checks pass |
| AVS-002 | Domain | Rational time types | Unit/property tests pass |
| AVS-003 | Domain | Timeline schema v1 | JSON validation fixtures pass |
| AVS-004 | Persistence | SQLite migrations | Fresh DB migrate/rollback tested |
| AVS-005 | Domain | Operation executor | Atomic batch and conflict tests pass |
| AVS-006 | Media | ffprobe asset inspector | Valid/invalid codec fixtures pass |
| AVS-007 | Rendering | IR nodes and validation | Graph tests pass |
| AVS-008 | Rendering | FFmpeg basic exporter | 15-second MP4 output verified |
| AVS-009 | Integration | First local end-to-end sample | CLI edit -> export + ffprobe test passes |
| AVS-010 | Persistence | Release A job leasing and status | Cancellation/retry tests pass |

For the first CLI build, tackle AVS-001–003 and AVS-006–009; AVS-004, AVS-005 and AVS-010 belong to Release A. The issue numbers identify work, not a mandatory execution order.

## 11. Coding-agent task contract
When delegating an issue to a coding agent, include objective, permitted modules, dependencies, input/output contracts, error semantics, test fixtures, acceptance commands and prohibited scope expansion. The project owner reviews API/schema changes and runs relevant checks; parallel agents must not independently invent timeline models or provider contracts. Solo development does not require agent handoff paperwork.

### Sample agent task
**AVS-005: Implement atomic operation executor**
- Inputs: project snapshot, `expectedRevision`, idempotency key, typed operations.
- Outputs: new immutable revision and operation IDs or structured validation/conflict error.
- Constraints: no FFmpeg/HTTP dependencies; no partial commits; locked tracks immutable.
- Tests: valid batch, invalid second operation rollback, stale revision, duplicate key, duplicate key different payload, missing clip.
- Done when `cargo test -p studio-operations` and SQLite integration tests pass.

## 12. CI and quality gates
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- TypeScript typecheck and lint when TS is introduced; Playwright smoke tests begin in Phase 5.
- Schema generation diff check from Phase 1; OpenAPI generation check from Phase 3.
- FFmpeg integration tests using pinned test build and golden fixtures.
- Security checks: dependency audit, secrets scan, malformed input fuzz cases.
- Render throughput/RAM/CPU measurements on reference hardware for the first build; seek-latency benchmarks begin with the browser editor.

## 13. Definition of done
No feature is done without tests, error handling, docs, validation and integration with canonical operations. The first build needs a working CLI demo, reproducible fixtures and known limitations; sample agent calls, API configuration and job diagnostics arrive in Release A.

## 14. Project risks and change control
Top near-term risks: media timebase errors, FFmpeg filter complexity and large-file resource exhaustion. Later risks: preview/export mismatch and TTS variance. Escalate scope change when it introduces a new core renderer, queue or persistence system; defer it until a measurable failure of the current approach is demonstrated.

## Normative conventions
- **MUST** is mandatory for the indicated release; **SHOULD** is recommended; **MAY** is optional.
- P0a is required for the first local export; P0b completes Release A; P1 for AI Production; P2 for Hybrid Editor.
- Versioned API root: `/v1`; timeline schema version: `1.0.0`.
- All times in the canonical timeline are integer project frames; media source positions use rational timestamps. Audio sample positions are retained for sample-accurate processing.
- From Release A, all command/API timeline mutations (including agent edits and restores) use expected project revision and an idempotency key; render requests pin a revision and have their own idempotency key. The first CLI build validates a manually edited JSON file instead.

## 15. Later work, only when needed
- Before Phase 5, prototype browser playback and compare cut, fade, title and caption frames to export. If an effect is not faithful, use a rendered segment or label preview approximate. This parity spike is not a gate for the first CLI build.
- Before separating the job runner, measure SQLite contention and restart behavior. Do not introduce PostgreSQL, a queue service, native compositor or desktop shell solely for anticipated scale.
- If coding agents contribute, the project owner approves changes to timeline schema, errors, IR and provider interfaces. The optional issue handoff should state owned files, contracts, fixtures, acceptance commands and prohibited scope changes.
