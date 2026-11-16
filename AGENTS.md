# Agent contribution guide

## Product and contracts

- This is a local-first, headless-first video studio. Agents are first-class users; the React editor is an optional client of the same validated services.
- Start with `README.md` for current setup and behavior, then `GET /v1/capabilities` for the running server's supported contract.
- `schemas/timeline-1.0.0.schema.json` defines the currently supported timeline shape. Semantic checks in `src/timeline.rs` are stricter than JSON Schema; unsupported media/effects must be rejected, never silently ignored.
- `docs/SPEC.md` includes planned and historical material. Do not implement an operation from its inventory unless it appears in `EditOperation` and the capabilities response.
- Rust owns persisted project state. Do not add a second timeline model or frame/source-time conversion implementation in TypeScript.

## Agent-safe editing

- Prefer `POST /v1/projects` with `assetIds`, optional canvas `presetId`, and optional `name`; raw revision-0 timeline creation remains an advanced path.
- Prefer `clip.append`, `image.add`, `audio.add`, and `title.add` seconds-based operations. The server converts to project frames and rational source timestamps; `image.add` defaults to a five-second hold.
- Use `clip.crop` to crop a video or still-image clip with normalized source coordinates (`x`, `y`, `width`, `height` from 0 to 1); the full-frame rectangle resets the crop.
- Every edit batch and revision restore uses `expectedRevision` and an `idempotencyKey`. Retry a stale edit only after re-reading the current timeline and using a new key. Never auto-retry a mutation with a changed body under the old key.
- Render requests pin an immutable revision. Inspect `GET /v1/jobs/{id}/inspection` or MCP `inspect_output`; QC warnings/errors and stills are part of the workflow, not optional diagnostics.
- A title requires the server/MCP process to be started with a valid `--font`. Check capabilities before adding text.
- MCP stdio is local process access. Never expose it as a network service or pass secrets/media credentials through tool arguments.

## Implementation rules

- Reuse the existing typed edit executor, validation, immutable revision store, and render worker. Keep REST, MCP, CLI, and editor as thin clients.
- Keep source media and generated artifacts inside the workspace. Do not accept remote URLs, shell snippets, arbitrary FFmpeg filters, or unvalidated filesystem paths.
- Keep errors machine-actionable (`code`, optional JSON pointer/expected/actual, revision details, and retryability). Preserve stable idempotency and conflict semantics.
- Update `README.md`, capability/OpenAPI output, and tests together when changing the public contract.

## Verification

```powershell
cargo fmt --all -- --check
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
cd apps/web-editor; npm run build
```

FFmpeg integration tests require `ffmpeg` and `ffprobe` on `PATH` or the `STUDIO_TEST_FFMPEG` / `STUDIO_TEST_FFPROBE` variables described in `README.md`.
