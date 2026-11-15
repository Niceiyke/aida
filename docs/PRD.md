# AI Video Studio — Product Requirements Document

**Status:** Implementation baseline | **Version:** 1.2 | **Product:** AI Video Studio

## 1. Vision
A standalone AI-first video production platform where external autonomous agents can import and understand footage, generate narration, assemble non-destructive multitrack edits, inspect previews, revise timelines, and export branded videos. A lightweight React editor lets a human inspect and correct the same project. The initial customer is the Wordlyte marketing workflow; the platform MUST be brand-neutral.

## 2. Outcomes and non-goals
**Primary outcome:** Given supplied approved media and a structured brief, an agent produces a coherent 30–60 second promotional video, complete with narration, captions, brand elements, background audio, preview, revisions, and H.264/AAC MP4 export without manual timeline manipulation.

**Success metrics (release acceptance targets):**
- 100% of successful exports pass ffprobe checks for codec, duration tolerance (within one output frame for video), dimensions and audio presence when requested.
- A scripted agent can create, revise, render and inspect a project through API/MCP without GUI access.
- For Release C, simple edits refresh in browser preview within 500 ms and proxy seek typically within 300 ms on reference hardware; targets require measurement and do not imply universal guarantees.
- Failed or cancelled renders do not mutate project revisions; recoverable jobs are demonstrable by Release A.
- Repeated editing requests with the same idempotency key do not create duplicate revisions.

**Not in v1:** full Premiere/Resolve feature parity, real-time broadcast switching, collaborative editing, 3D effects, advanced grading, native GPU compositor, mobile editor, auto-posting, cloud render farms, AI-generated footage, third-party plugin marketplace.

## 3. Users and jobs to be done
1. **Automation agent:** convert a creative brief and approved assets into a validated timeline, request narration/captions, preview and revise.
2. **Content reviewer:** play the current project, adjust text/trim/placement, compare revisions, approve export.
3. **Operator/developer:** inspect jobs, diagnose render failures, configure providers, enforce permissions.

## 4. Release boundaries
| Release | Priority | Required outcome |
|---|---|---|
| First usable build: Local export | P0a | Local media import, JSON timeline edits, text/audio, FFmpeg export and ffprobe validation through a CLI; no server or browser required |
| A: Headless agent API | P0b | Projects, assets, revisioned timeline operations, REST/MCP tools, rendered preview files and recoverable jobs |
| B: AI Production | P1 | Cloud TTS + local provider adapter, narration-driven edits, transcript search, captions, brand kit/templates, quality checks |
| C: Hybrid Editor | P2 | React media browser, proxy playback, multitrack timeline, property panel, revision and export views |

### 4.1 Feature requirements
| ID | Feature | Priority | Acceptance |
|---|---|---|---|
| PR-001 | Projects/revisions | P0b | Create, retrieve and restore immutable revisions in Release A; duplication may follow A |
| PR-002 | Asset import | P0a/P0b | P0a validate/probe/checksum local MP4/MOV video and MP3/WAV/AAC audio into safe asset URIs; P0b add HTTP uploads and remaining v1 formats |
| PR-003 | Editing | P0a/P0b | P0a validate manual JSON timeline edits; P0b atomic add/remove/move/trim/split and track commands |
| PR-004 | Rendering | P0a/P0b | P0a H.264/AAC MP4 from 720p/1080p canvases; P0b add queued jobs, more presets and QC |
| PR-005 | Agent gateway | P0b | Typed REST/MCP tools with local token scopes, structured errors, job polling |
| PR-006 | Text/audio | P0a/P0b | P0a text overlay, voice/music mix and gain; P0b add fades and simple transitions |
| PR-007 | Voice generation | P1 | Voice catalog, preview, synthesize, cache, normalize, insert into timeline |
| PR-008 | Narration-driven editing | P1 | Script segments become timed audio and aligned visual/caption suggestions |
| PR-009 | Captions | P1 | Transcript import/generation, segment editing, burn-in export |
| PR-010 | Brand kit | P1 | Approved logos, font references, colors, title styles, voice profile |
| PR-011 | Media intelligence | P1 | Transcript-based retrieval and basic scene boundaries |
| PR-012 | Templates | P1 | 9:16, 16:9, 1:1 promotional templates |
| PR-013 | QC | P1 | Missing media, black frames, silence/peak checks, duration and format |
| PR-014 | Interactive browser preview | P2 | Play/pause/seek, proxy media, overlays, stale-preview indicator; Release A provides rendered preview files instead |
| PR-015 | Visual edits | P2 | Drag, trim, split, select, change properties, undo/redo |

### 4.2 Intermediate editor boundary
The first browser-editor increment targets practical, non-destructive editing without attempting a full NLE feature set:
- Import and browse multiple local video/audio assets; create, reopen, and switch between local projects.
- Edit one contiguous hard-cut video track, up to two audio tracks, and one title track. Support adding/removing clips, video reordering, clip split/trim, audio/title positioning, basic audio gain, and title text/style/position properties.
- Keep every edit revisioned and validated through the same API operations used by other clients. Undo, redo, and history restore append revisions rather than mutating prior snapshots.
- Display source preview as approximate for a multi-clip composition; use rendered preview output as the authoritative composite preview and label its revision.
- Defer image/caption tracks, transitions, fades, effects, overlapping video layers, speed changes, synchronized proxy playback, TTS, MCP, collaboration, and remote rendering. This boundary is intentionally below Premiere/Resolve parity.

## 5. Supported media and limits
- Eventual import targets: MP4/MOV video (H.264/H.265 where installed FFmpeg supports them), PNG/JPEG/WebP images, MP3/WAV/AAC audio. The first CLI build supports MP4/MOV video and MP3/WAV/AAC audio only; actual codec support is probed at runtime.
- Output: MP4/H.264/AAC, 720p and 1080p, 24/25/30 fps; 30000/1001 is supported by the first CLI, and 60 fps is conditional on capability testing.
- Initial project limit: 5 minutes. First CLI target: 1080p30 export; Release C interactive proxy preview targets 720p30 on documented reference hardware.
- Frame-based editing; source timestamps retain rational time base; audio synchronization uses sample-accurate positions.

## 6. Primary user journeys
### 6.1 Agent-driven promo
1. Agent creates project and uploads/links approved assets.
2. Studio probes assets and generates metadata; browser proxies and transcripts are generated when those features become available.
3. Agent drafts narration and requests voice preview and synthesis.
4. Narration audio is stored as an asset with provider provenance and timing/alignment metadata.
5. Agent constructs visual timeline around narration segments and applies brand kit.
6. Studio validates edits and renders a preview file; QC returns structured findings.
7. Agent makes at most two automated correction rounds by default; reviewer approves or requests changes.
8. Export references frozen revision and produces downloadable MP4.

### 6.2 Human correction
Reviewer opens project, previews proxies, edits a title and clip trim, sees revision increment, compares with previous revision, and exports.

### 6.3 Voice generation
User or agent selects voice from catalog, generates short preview, submits full script with speed/style/language settings supported by provider, receives audio asset and alignment, then inserts it into narration track. Voice cloning is excluded from v1; future cloning requires documented consent and authorization.

## 7. UX requirements
- Headless API is the canonical interaction surface; GUI MUST use the same operations.
- UI must display render state and errors, timeline revision, proxy/stale state, audio waveforms and asset availability.
- Brand voice profile stores provider, voice ID, locale, speed, style, pronunciation guidance, and approved usage policy; it must not store API secrets.
- Review/approval is explicit before publishing; automatic export does not imply external publishing permission.

## 8. Security, privacy and compliance
- User-uploaded files are untrusted; MIME sniffing, size limits, path isolation and bounded FFmpeg execution are mandatory from the first local import/render slice, not a later hardening phase.
- Provider API keys are server-side secrets; logs redact secrets and user narration content where practical.
- Generated speech must preserve provenance and usage permissions; impersonation/cloning requires consent.
- Track provider usage/cost, data retention, and deletion of original/generated assets.
- Only licensed/approved fonts, footage, voices and music may be used in production.

## 9. Release exit criteria
**First usable build:** Local CLI imports approved files, constructs/edits a 15-second two-clip timeline, exports H.264/AAC MP4 without a GUI and validates the result with ffprobe.
**A:** Headless integration test produces and revises 15-second multi-track MP4 and a rendered preview file; REST/MCP tools work without a GUI; revision conflict and retry tests pass.
**B:** Agent produces a branded 30–60-second promo with narration, captions and QC; provider failure is recoverable.
**C:** Reviewer can play, seek, trim, edit text, undo and export; preview/export comparison tests pass on supported effects.

## 10. Open decisions
- Which initial cloud TTS provider and supported regions/voices?
- Which single-user host environments (Windows first, Linux headless for CI) must be supported before broader packaging?
- Reference hardware and performance thresholds must be measured with representative Wordlyte clips.

## Normative conventions
- **MUST** is mandatory for the indicated release; **SHOULD** is recommended; **MAY** is optional.
- P0a is required for the first local export; P0b completes Release A; P1 for AI Production; P2 for Hybrid Editor.
- Versioned API root: `/v1`; timeline schema version: `1.0.0`.
- All times in the canonical timeline are integer project frames; media source positions use rational timestamps. Audio sample positions are retained for sample-accurate processing.
- From Release A, all command/API timeline mutations (including agent edits and restores) use expected project revision and an idempotency key; render requests pin a revision and have their own idempotency key. The first CLI build validates a manually edited JSON file instead.

## 11. Product ownership and implementation boundaries
- AI Video Studio MUST be independently implemented; existing editors (including WeftCut) are research references, not runtime or source dependencies. Reuse established codecs, model runtimes and standards instead of building them from scratch.
- The headless workflow MUST run without Electron, Tauri, React, an interactive desktop session, or a visible GPU window. The web editor is an optional client of the same API.
- Agents and humans MUST mutate timelines through identical validated commands. Publishing or distribution to external platforms remains outside v1 and requires separate authorization.
- The first CLI export MUST record the hash of its validated JSON timeline, input asset hashes, render configuration and renderer build identity. From Release A, successful exports also bind to an immutable project revision. Re-running on a pinned environment SHOULD yield frame-equivalent results; byte-identical encoded MP4 files are not guaranteed.
- Voice-first production MUST support narration synthesis, timing/alignment, storyboard-to-clip placement and caption synchronization, while preserving a manual correction path.

## 12. First-build acceptance
A feasibility spike MUST demonstrate: (a) a 15-second two-video composition with title, voice and music; (b) no-GUI export through a command-line entry point; (c) ffprobe checks for streams, dimensions and duration; (d) measured audio synchronization; (e) repeatable rendering using a documented FFmpeg version and fonts. An overlap/fade and browser parity experiment are follow-on risks to test before committing to interactive preview. If browser preview later cannot faithfully reproduce an effect, the editor MUST label it approximate or use a rendered proxy rather than silently diverging.

## 13. Operational quality and safety
- Agent edits MUST be atomic, scoped to a specific expected revision, idempotent and auditable; concurrent stale edits MUST return a conflict rather than overwrite work.
- By Release A, export jobs MUST support queued/running/succeeded/failed/cancelled states, cancellation, bounded retries, restart recovery, diagnostic logs and machine-readable QC results. The first local CLI build may render synchronously.
- Agents MUST NOT provide arbitrary shell commands, FFmpeg filter strings or untrusted filesystem paths to the renderer.
- Initial performance targets are measurement gates on a documented reference machine, not product-wide guarantees.

## 14. Deployment baseline
- This is a single-user personal project. Start with a JSON-to-MP4 CLI and isolated local media; Release A adds SQLite revision/job metadata, a loopback-only Axum API by default, validated operations and an in-process job runner. A separate worker, PostgreSQL, object storage, remote exposure and multi-user tenancy are future options only when measured needs justify them.
- The CLI, REST API, MCP tools and later editor share the same timeline validation and rendering code; local-first does not relax untrusted-media protections.
