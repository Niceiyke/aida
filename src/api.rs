use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{
        Request, StatusCode,
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
        },
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    AssetKind, EditOperation, EditResult, MediaProbe, ProjectRestoreResult, ProjectRevision,
    ProjectRevisionHistory, ProjectStore, ProjectSummary, RenderJob, Workspace,
    jobs::{JobCancellationRegistry, RenderJobRunner},
    workspace::validate_asset_id,
};

const MAX_API_BODY_BYTES: usize = 2 * 1024 * 1024;
const MIN_BEARER_TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone)]
pub struct ApiServerOptions {
    pub database: PathBuf,
    pub workspace: PathBuf,
    pub bind: SocketAddr,
    pub bearer_token: String,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub font: Option<PathBuf>,
    pub timeout_seconds: u64,
}

#[derive(Clone)]
struct ApiState {
    store: ProjectStore,
    workspace: PathBuf,
    cancellation: JobCancellationRegistry,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditRequest {
    expected_revision: u64,
    idempotency_key: String,
    operations: Vec<EditOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineQuery {
    revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreRequest {
    target_revision: u64,
    expected_revision: u64,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenderRequest {
    revision: u64,
    preset_id: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetryRequest {
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetMetadata {
    asset_id: String,
    kind: AssetKind,
    mime_type: String,
    sha256: String,
    bytes: u64,
    content_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    preset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe: Option<MediaProbe>,
}

struct ResolvedApiAsset {
    asset_id: String,
    kind: AssetKind,
    mime_type: String,
    sha256: String,
    bytes: u64,
    path: PathBuf,
    filename: String,
    preset_id: Option<String>,
    probe: Option<MediaProbe>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorDocument,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorDocument {
    code: String,
    message: String,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<ErrorDetails>,
    retryable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorDetails {
    current_revision: u64,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
    current_revision: Option<u64>,
}

impl ApiError {
    fn from_anyhow(error: anyhow::Error) -> Self {
        let details = format!("{error:#}");
        let (status, code, message) = if details.contains("REVISION_CONFLICT") {
            (StatusCode::CONFLICT, "REVISION_CONFLICT", details)
        } else if details.contains("IDEMPOTENCY_CONFLICT") {
            (StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT", details)
        } else if details.contains("PROJECT_EXISTS") {
            (StatusCode::CONFLICT, "PROJECT_EXISTS", details)
        } else if details.contains("PROJECT_NOT_FOUND") {
            (StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", details)
        } else if details.contains("REVISION_NOT_FOUND") {
            (StatusCode::NOT_FOUND, "REVISION_NOT_FOUND", details)
        } else if details.contains("JOB_NOT_FOUND") {
            (StatusCode::NOT_FOUND, "JOB_NOT_FOUND", details)
        } else if details.contains("ASSET_NOT_FOUND") {
            (StatusCode::NOT_FOUND, "ASSET_NOT_FOUND", details)
        } else if details.contains("JOB_NOT_RETRYABLE") {
            (StatusCode::CONFLICT, "JOB_NOT_RETRYABLE", details)
        } else if details.contains("JOB_RETRY_LIMIT") {
            (StatusCode::CONFLICT, "JOB_RETRY_LIMIT", details)
        } else if error
            .chain()
            .any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some())
        {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "The local project store could not complete the request".to_owned(),
            )
        } else {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "VALIDATION_FAILED",
                details,
            )
        };
        Self {
            status,
            code: code.to_owned(),
            message,
            current_revision: None,
        }
    }

    fn from_json_rejection(error: JsonRejection) -> Self {
        let status = error.status();
        let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "PAYLOAD_TOO_LARGE"
        } else {
            "INVALID_JSON"
        };
        Self {
            status,
            code: code.to_owned(),
            message: error.body_text(),
            current_revision: None,
        }
    }

    fn from_query_rejection(error: QueryRejection) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "INVALID_QUERY".to_owned(),
            message: error.body_text(),
            current_revision: None,
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "PERMISSION_DENIED".to_owned(),
            message: "A valid Bearer token is required".to_owned(),
            current_revision: None,
        }
    }

    fn internal(message: &str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL_ERROR".to_owned(),
            message: message.to_owned(),
            current_revision: None,
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::from_anyhow(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let details = self
            .current_revision
            .map(|current_revision| ErrorDetails { current_revision });
        let body = ErrorEnvelope {
            error: ErrorDocument {
                code: self.code,
                message: self.message,
                request_id: Uuid::new_v4().to_string(),
                details,
                retryable: false,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

pub async fn serve_api(options: ApiServerOptions) -> Result<()> {
    if !options.bind.ip().is_loopback() {
        bail!("the local API may bind only to a loopback address");
    }
    if !(1..=3600).contains(&options.timeout_seconds) {
        bail!("timeout-seconds must be between 1 and 3600");
    }
    validate_bearer_token(&options.bearer_token)?;
    let workspace = Workspace::open(&options.workspace)?;
    let store = ProjectStore::open(&options.database)?;
    let cancellation = JobCancellationRegistry::default();
    let runner = RenderJobRunner::new(
        store.clone(),
        workspace.root().to_path_buf(),
        options.ffmpeg,
        options.ffprobe,
        options.font,
        options.timeout_seconds,
        cancellation.clone(),
    );
    let app = build_router(
        store,
        workspace.root().to_path_buf(),
        options.bearer_token,
        cancellation,
    );
    let listener = TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("could not bind local API to {}", options.bind))?;
    let address = listener.local_addr()?;
    eprintln!("Avida local API listening on http://{address}");
    let worker_runner = runner.clone();
    let worker = tokio::spawn(async move { worker_runner.run().await });
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("local API server failed");
    runner.request_shutdown();
    let worker_result = worker
        .await
        .context("render job worker task did not complete")?;
    worker_result.context("render job worker failed")?;
    server_result
}

fn validate_bearer_token(token: &str) -> Result<()> {
    if token.len() < MIN_BEARER_TOKEN_BYTES || token.chars().any(char::is_control) {
        bail!("STUDIO_API_TOKEN must contain at least 32 bytes and no control characters");
    }
    Ok(())
}

fn build_router(
    store: ProjectStore,
    workspace: PathBuf,
    bearer_token: String,
    cancellation: JobCancellationRegistry,
) -> Router {
    let state = ApiState {
        store,
        workspace,
        cancellation,
    };
    Router::new()
        .route("/v1/projects", post(create_project))
        .route("/v1/projects/{project_id}", get(get_project))
        .route("/v1/projects/{project_id}/timeline", get(get_timeline))
        .route(
            "/v1/projects/{project_id}/revisions",
            get(get_project_revisions),
        )
        .route(
            "/v1/projects/{project_id}/restore",
            post(restore_project_revision),
        )
        .route(
            "/v1/projects/{project_id}/operations",
            post(apply_operations),
        )
        .route("/v1/projects/{project_id}/renders", post(enqueue_render))
        .route("/v1/jobs/{job_id}", get(get_render_job))
        .route("/v1/jobs/{job_id}/cancel", post(cancel_render_job))
        .route("/v1/jobs/{job_id}/retry", post(retry_render_job))
        .route("/v1/assets/{asset_id}", get(get_asset_metadata))
        .route("/v1/assets/{asset_id}/content", get(get_asset_content))
        .layer(DefaultBodyLimit::max(MAX_API_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(
            Arc::<[u8]>::from(bearer_token.into_bytes()),
            require_bearer,
        ))
        .with_state(state)
}

async fn require_bearer(
    State(token): State<Arc<[u8]>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, credential) = value.split_once(' ')?;
            (scheme.eq_ignore_ascii_case("Bearer") && !credential.is_empty()).then_some(credential)
        })
        .is_some_and(|provided| {
            provided.len() == token.len() && bool::from(provided.as_bytes().ct_eq(token.as_ref()))
        });
    if authorized {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

async fn create_project(
    State(state): State<ApiState>,
    payload: std::result::Result<Json<Value>, JsonRejection>,
) -> std::result::Result<(StatusCode, Json<ProjectRevision>), ApiError> {
    let Json(document) = payload.map_err(ApiError::from_json_rejection)?;
    let store = state.store;
    let workspace = state.workspace;
    let result =
        tokio::task::spawn_blocking(move || store.create_from_timeline_json(&workspace, document))
            .await
            .map_err(|_| ApiError::internal("The project worker did not complete the request"))??;
    Ok((StatusCode::CREATED, Json(result)))
}

async fn get_project(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
) -> std::result::Result<Json<ProjectSummary>, ApiError> {
    Ok(Json(state.store.get_project_summary(&project_id)?))
}

async fn get_timeline(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    query: std::result::Result<Query<TimelineQuery>, QueryRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(ApiError::from_query_rejection)?;
    let store = state.store;
    let result = if let Some(revision) = query.revision {
        store.load_revision_json(&project_id, revision)?
    } else {
        store.load_current_revision_json(&project_id)?.1
    };
    let document = serde_json::from_str(&result)
        .context("stored project timeline is invalid")
        .map_err(ApiError::from)?;
    Ok(Json(document))
}

async fn get_project_revisions(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
) -> std::result::Result<Json<ProjectRevisionHistory>, ApiError> {
    Ok(Json(state.store.get_project_revision_history(&project_id)?))
}

async fn restore_project_revision(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    payload: std::result::Result<Json<RestoreRequest>, JsonRejection>,
) -> std::result::Result<Response, ApiError> {
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    let store = state.store;
    let workspace = state.workspace;
    let conflict_project_id = project_id.clone();
    let (result, current_revision) = tokio::task::spawn_blocking(move || {
        let result = store.restore_revision(
            &workspace,
            &project_id,
            request.target_revision,
            request.expected_revision,
            &request.idempotency_key,
        );
        let current_revision = result
            .as_ref()
            .err()
            .filter(|error| format!("{error:#}").contains("REVISION_CONFLICT"))
            .and_then(|_| {
                store
                    .get_project_summary(&conflict_project_id)
                    .ok()
                    .map(|project| project.current_revision)
            });
        (result, current_revision)
    })
    .await
    .map_err(|_| ApiError::internal("The restore worker did not complete the request"))?;

    match result {
        Ok(result) => {
            let status = if result.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok((status, Json::<ProjectRestoreResult>(result)).into_response())
        }
        Err(error) => {
            let mut api_error = ApiError::from_anyhow(error);
            api_error.current_revision = current_revision;
            Err(api_error)
        }
    }
}

async fn apply_operations(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    payload: std::result::Result<Json<EditRequest>, JsonRejection>,
) -> std::result::Result<Json<EditResult>, ApiError> {
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    let store = state.store;
    let workspace = state.workspace;
    let project_id_for_conflict = project_id.clone();
    let (result, current_revision) = tokio::task::spawn_blocking(move || {
        let result = store.apply_edit_batch(
            &workspace,
            &project_id,
            request.expected_revision,
            &request.idempotency_key,
            &request.operations,
        );
        let current_revision = result
            .as_ref()
            .err()
            .filter(|error| format!("{error:#}").contains("REVISION_CONFLICT"))
            .and_then(|_| {
                store
                    .get_project_summary(&project_id_for_conflict)
                    .ok()
                    .map(|project| project.current_revision)
            });
        (result, current_revision)
    })
    .await
    .map_err(|_| ApiError::internal("The edit worker did not complete the request"))?;

    match result {
        Ok(result) => Ok(Json(result)),
        Err(error) => {
            let mut api_error = ApiError::from_anyhow(error);
            api_error.current_revision = current_revision;
            Err(api_error)
        }
    }
}

async fn enqueue_render(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    payload: std::result::Result<Json<RenderRequest>, JsonRejection>,
) -> std::result::Result<Response, ApiError> {
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    let submission = state.store.enqueue_render_job(
        &project_id,
        request.revision,
        &request.preset_id,
        &request.idempotency_key,
    )?;
    let status = if submission.replayed {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(submission)).into_response())
}

async fn get_render_job(
    State(state): State<ApiState>,
    Path(job_id): Path<String>,
) -> std::result::Result<Json<RenderJob>, ApiError> {
    Ok(Json(state.store.get_render_job(&job_id)?))
}

async fn cancel_render_job(
    State(state): State<ApiState>,
    Path(job_id): Path<String>,
) -> std::result::Result<Json<RenderJob>, ApiError> {
    let job = state.store.cancel_render_job(&job_id)?;
    if job.cancel_requested {
        state.cancellation.request_cancel(&job_id);
    }
    Ok(Json(job))
}

async fn retry_render_job(
    State(state): State<ApiState>,
    Path(job_id): Path<String>,
    payload: std::result::Result<Json<RetryRequest>, JsonRejection>,
) -> std::result::Result<Response, ApiError> {
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    let submission = state
        .store
        .retry_render_job(&job_id, &request.idempotency_key)?;
    let status = if submission.replayed {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(submission)).into_response())
}

fn resolve_api_asset(
    state: &ApiState,
    asset_id: &str,
) -> std::result::Result<ResolvedApiAsset, ApiError> {
    validate_asset_id(asset_id)?;
    let workspace = Workspace::open(&state.workspace)
        .map_err(|_| ApiError::internal("The media workspace is unavailable"))?;
    match workspace.load_asset(asset_id) {
        Ok((path, manifest)) => {
            let metadata = std::fs::metadata(&path)
                .map_err(|_| ApiError::internal("The imported source asset is unavailable"))?;
            if !metadata.is_file()
                || metadata.len() != manifest.bytes
                || !is_sha256(&manifest.sha256)
            {
                return Err(ApiError::internal(
                    "The imported source asset does not match its manifest",
                ));
            }
            return Ok(ResolvedApiAsset {
                asset_id: manifest.asset_id,
                kind: manifest.kind,
                mime_type: "application/octet-stream".to_owned(),
                sha256: manifest.sha256.to_ascii_lowercase(),
                bytes: manifest.bytes,
                path,
                filename: format!("asset-{asset_id}.media"),
                preset_id: None,
                probe: Some(manifest.probe),
            });
        }
        Err(error) if format!("{error:#}").contains("ASSET_NOT_FOUND") => {}
        Err(_) => {
            return Err(ApiError::internal(
                "The imported source asset could not be safely resolved",
            ));
        }
    }

    let asset = state.store.get_render_asset(asset_id)?;
    let path = workspace
        .resolve_render_output(&asset.job_id, asset.attempt)
        .map_err(|_| ApiError::internal("The published render artifact is unavailable"))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|_| ApiError::internal("The published render artifact is unavailable"))?;
    if !metadata.is_file() || !is_sha256(&asset.output_sha256) {
        return Err(ApiError::internal(
            "The published render artifact metadata is invalid",
        ));
    }
    Ok(ResolvedApiAsset {
        asset_id: asset.asset_id,
        kind: AssetKind::Video,
        mime_type: "video/mp4".to_owned(),
        sha256: asset.output_sha256.to_ascii_lowercase(),
        bytes: metadata.len(),
        path,
        filename: format!("render-{asset_id}.mp4"),
        preset_id: Some(asset.preset_id),
        probe: None,
    })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn get_asset_metadata(
    State(state): State<ApiState>,
    Path(asset_id): Path<String>,
) -> std::result::Result<Json<AssetMetadata>, ApiError> {
    let asset = resolve_api_asset(&state, &asset_id)?;
    Ok(Json(AssetMetadata {
        asset_id: asset.asset_id.clone(),
        kind: asset.kind,
        mime_type: asset.mime_type,
        sha256: asset.sha256,
        bytes: asset.bytes,
        content_url: format!("/v1/assets/{}/content", asset.asset_id),
        preset_id: asset.preset_id,
        probe: asset.probe,
    }))
}

async fn get_asset_content(
    State(state): State<ApiState>,
    Path(asset_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let asset = resolve_api_asset(&state, &asset_id)?;
    let path = asset.path.clone();
    let expected_sha256 = asset.sha256.clone();
    let expected_bytes = asset.bytes;
    let file = tokio::task::spawn_blocking(move || {
        open_and_verify_asset(&path, expected_bytes, &expected_sha256)
    })
    .await
    .map_err(|_| ApiError::internal("The asset verification task did not complete"))?
    .map_err(|_| ApiError::internal("The workspace asset failed integrity validation"))?;
    // Stream the same verified handle; do not reopen the path after checking it.
    let file = tokio::fs::File::from_std(file);

    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        asset
            .mime_type
            .parse()
            .map_err(|_| ApiError::internal("The asset MIME type is invalid"))?,
    );
    headers.insert(
        CONTENT_LENGTH,
        asset
            .bytes
            .to_string()
            .parse()
            .map_err(|_| ApiError::internal("The asset size is invalid"))?,
    );
    headers.insert(
        CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", asset.filename)
            .parse()
            .map_err(|_| ApiError::internal("The asset filename is invalid"))?,
    );
    headers.insert(
        ETAG,
        format!("\"{}\"", asset.sha256)
            .parse()
            .map_err(|_| ApiError::internal("The asset hash is invalid"))?,
    );
    headers.insert(
        CACHE_CONTROL,
        "private, no-store".parse().expect("static header"),
    );
    Ok(response)
}

fn open_and_verify_asset(
    path: &std::path::Path,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<std::fs::File> {
    let mut file = std::fs::File::open(path).context("could not open workspace asset")?;
    let metadata = file
        .metadata()
        .context("could not inspect opened workspace asset")?;
    if !metadata.is_file() || metadata.len() != expected_bytes {
        bail!("opened asset size does not match its recorded size");
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes_read = 0_u64;
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer)
            .context("could not read opened workspace asset")?;
        if count == 0 {
            break;
        }
        bytes_read = bytes_read
            .checked_add(u64::try_from(count).context("asset read size is out of range")?)
            .context("asset read size overflowed")?;
        if bytes_read > expected_bytes {
            bail!("opened asset grew during integrity validation");
        }
        hasher.update(&buffer[..count]);
    }
    if bytes_read != expected_bytes {
        bail!("opened asset size changed during integrity validation");
    }
    let actual_sha256 = hex::encode(hasher.finalize());
    if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
        bail!("opened asset checksum does not match its recorded checksum");
    }
    if file
        .metadata()
        .context("could not re-inspect opened workspace asset")?
        .len()
        != expected_bytes
    {
        bail!("opened asset changed during integrity validation");
    }
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))
        .context("could not rewind verified workspace asset")?;
    Ok(file)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, path::PathBuf};

    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode},
        response::Response,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        media::{AssetKind, MediaProbe, StreamInfo},
        workspace::{AssetManifest, SOURCE_FILENAME},
    };

    const PROJECT_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0a";
    const ASSET_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0b";
    const TEST_TOKEN: &str = "local-test-token-0123456789abcdef012345";

    #[tokio::test]
    async fn server_rejects_non_loopback_bind_and_short_tokens() {
        let root = tempdir().unwrap();
        let non_loopback = serve_api(ApiServerOptions {
            database: root.path().join("projects.sqlite"),
            workspace: root.path().join("workspace"),
            bind: "0.0.0.0:8787".parse().unwrap(),
            bearer_token: TEST_TOKEN.to_owned(),
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            font: None,
            timeout_seconds: 300,
        })
        .await
        .unwrap_err();
        assert!(format!("{non_loopback:#}").contains("loopback"));

        let short_token = serve_api(ApiServerOptions {
            database: root.path().join("projects.sqlite"),
            workspace: root.path().join("workspace"),
            bind: "127.0.0.1:8787".parse().unwrap(),
            bearer_token: "short".to_owned(),
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            font: None,
            timeout_seconds: 300,
        })
        .await
        .unwrap_err();
        assert!(format!("{short_token:#}").contains("at least 32 bytes"));
    }

    #[tokio::test]
    async fn authenticated_api_creates_reads_and_edits_project_revisions() {
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let workspace = Workspace::open(&workspace_path).unwrap();
        install_video_fixture(&workspace, root.path());
        let app = build_router(
            ProjectStore::open(root.path().join("projects.sqlite")).unwrap(),
            workspace_path,
            TEST_TOKEN.to_owned(),
            JobCancellationRegistry::default(),
        );
        let timeline = timeline_document();

        let unauthorized = request(
            &app,
            Method::POST,
            "/v1/projects",
            None,
            Some(timeline.to_string()),
        )
        .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let unauthorized_history = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}/revisions"),
            None,
            None,
        )
        .await;
        assert_eq!(unauthorized_history.status(), StatusCode::UNAUTHORIZED);

        let unauthorized_restore = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/restore"),
            None,
            None,
        )
        .await;
        assert_eq!(unauthorized_restore.status(), StatusCode::UNAUTHORIZED);

        let created = request(
            &app,
            Method::POST,
            "/v1/projects",
            Some(TEST_TOKEN),
            Some(timeline.to_string()),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: Value = response_json(created).await;
        assert_eq!(created["projectId"], PROJECT_ID);
        assert_eq!(created["revision"], 1);

        let render_request = json!({
            "revision": 1,
            "presetId": "export",
            "idempotencyKey": "api-render-1"
        });
        let queued = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/renders"),
            Some(TEST_TOKEN),
            Some(render_request.to_string()),
        )
        .await;
        assert_eq!(queued.status(), StatusCode::ACCEPTED);
        let queued: Value = response_json(queued).await;
        let job_id = queued["job"]["jobId"].as_str().unwrap().to_owned();
        assert_eq!(queued["job"]["status"], "queued");
        assert_eq!(queued["job"]["revision"], 1);
        assert_eq!(queued["job"]["presetId"], "export");
        assert_eq!(queued["replayed"], false);

        let preview_request = json!({
            "revision": 1,
            "presetId": "preview",
            "idempotencyKey": "api-preview-1"
        });
        let preview = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/renders"),
            Some(TEST_TOKEN),
            Some(preview_request.to_string()),
        )
        .await;
        assert_eq!(preview.status(), StatusCode::ACCEPTED);
        let preview: Value = response_json(preview).await;
        assert_eq!(preview["job"]["presetId"], "preview");

        let replay = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/renders"),
            Some(TEST_TOKEN),
            Some(render_request.to_string()),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        let replay: Value = response_json(replay).await;
        assert_eq!(replay["job"]["jobId"], job_id);
        assert_eq!(replay["replayed"], true);

        let cancelled = request(
            &app,
            Method::POST,
            &format!("/v1/jobs/{job_id}/cancel"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(cancelled.status(), StatusCode::OK);
        assert_eq!(response_json(cancelled).await["status"], "cancelled");

        let fetched_job = request(
            &app,
            Method::GET,
            &format!("/v1/jobs/{job_id}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(fetched_job.status(), StatusCode::OK);
        let fetched_job: Value = response_json(fetched_job).await;
        assert_eq!(fetched_job["jobId"], job_id);
        assert_eq!(fetched_job["status"], "cancelled");
        assert_eq!(fetched_job["cancelRequested"], true);

        let summary = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(summary.status(), StatusCode::OK);
        assert_eq!(response_json(summary).await["currentRevision"], 1);

        let operation = json!({
            "expectedRevision": 1,
            "idempotencyKey": "api-text-update-1",
            "operations": [{"type": "text.update", "clipId": "title", "text": "Updated title"}]
        });
        let edit = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/operations"),
            Some(TEST_TOKEN),
            Some(operation.to_string()),
        )
        .await;
        assert_eq!(edit.status(), StatusCode::OK);
        let edit: Value = response_json(edit).await;
        assert_eq!(edit["revision"], 2);
        assert_eq!(edit["replayed"], false);

        let replay = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/operations"),
            Some(TEST_TOKEN),
            Some(operation.to_string()),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(response_json(replay).await["replayed"], true);

        let current_timeline = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}/timeline"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(current_timeline.status(), StatusCode::OK);
        let current_timeline = response_json(current_timeline).await;
        assert_eq!(current_timeline["revision"], 2);
        assert_eq!(
            current_timeline["tracks"][1]["clips"][0]["text"],
            "Updated title"
        );

        let old_timeline = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}/timeline?revision=1"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(old_timeline.status(), StatusCode::OK);
        let old_timeline = response_json(old_timeline).await;
        assert_eq!(old_timeline["revision"], 1);
        assert_eq!(
            old_timeline["tracks"][1]["clips"][0]["text"],
            "Original title"
        );

        let stale = json!({
            "expectedRevision": 1,
            "idempotencyKey": "api-stale-edit",
            "operations": [{"type": "text.update", "clipId": "title", "text": "Stale title"}]
        });
        let conflict = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/operations"),
            Some(TEST_TOKEN),
            Some(stale.to_string()),
        )
        .await;
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let conflict: Value = response_json(conflict).await;
        assert_eq!(conflict["error"]["code"], "REVISION_CONFLICT");
        assert_eq!(conflict["error"]["details"]["currentRevision"], 2);

        let history = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}/revisions"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(history.status(), StatusCode::OK);
        let history: Value = response_json(history).await;
        assert_eq!(history["currentRevision"], 2);
        assert_eq!(history["revisions"].as_array().unwrap().len(), 2);
        assert_eq!(history["revisions"][0]["revision"], 2);
        assert_eq!(history["revisions"][0]["isCurrent"], true);
        assert!(history["revisions"][0]["createdAt"].is_string());
        assert_eq!(history["revisions"][1]["revision"], 1);
        assert_eq!(history["revisions"][1]["isCurrent"], false);

        let restore_request = json!({
            "targetRevision": 1,
            "expectedRevision": 2,
            "idempotencyKey": "api-restore-1"
        });
        let restored = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/restore"),
            Some(TEST_TOKEN),
            Some(restore_request.to_string()),
        )
        .await;
        assert_eq!(restored.status(), StatusCode::CREATED);
        let restored: Value = response_json(restored).await;
        assert_eq!(restored["projectId"], PROJECT_ID);
        assert_eq!(restored["revision"], 3);
        assert_eq!(restored["replayed"], false);

        let restore_replay = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/restore"),
            Some(TEST_TOKEN),
            Some(restore_request.to_string()),
        )
        .await;
        assert_eq!(restore_replay.status(), StatusCode::OK);
        let restore_replay: Value = response_json(restore_replay).await;
        assert_eq!(restore_replay["revision"], 3);
        assert_eq!(restore_replay["replayed"], true);

        let restored_timeline = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}/timeline"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(restored_timeline.status(), StatusCode::OK);
        let restored_timeline: Value = response_json(restored_timeline).await;
        assert_eq!(restored_timeline["revision"], 3);
        assert_eq!(
            restored_timeline["tracks"][1]["clips"][0]["text"],
            "Original title"
        );

        let restored_history = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}/revisions"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        let restored_history: Value = response_json(restored_history).await;
        assert_eq!(restored_history["currentRevision"], 3);
        assert_eq!(restored_history["revisions"].as_array().unwrap().len(), 3);
        assert_eq!(restored_history["revisions"][0]["revision"], 3);
        assert_eq!(restored_history["revisions"][0]["isCurrent"], true);

        let restore_conflict = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/restore"),
            Some(TEST_TOKEN),
            Some(
                json!({
                    "targetRevision": 1,
                    "expectedRevision": 2,
                    "idempotencyKey": "api-stale-restore"
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(restore_conflict.status(), StatusCode::CONFLICT);
        let restore_conflict: Value = response_json(restore_conflict).await;
        assert_eq!(restore_conflict["error"]["code"], "REVISION_CONFLICT");
        assert_eq!(restore_conflict["error"]["details"]["currentRevision"], 3);

        let idempotency_conflict = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/restore"),
            Some(TEST_TOKEN),
            Some(
                json!({
                    "targetRevision": 2,
                    "expectedRevision": 2,
                    "idempotencyKey": "api-restore-1"
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(idempotency_conflict.status(), StatusCode::CONFLICT);
        let idempotency_conflict: Value = response_json(idempotency_conflict).await;
        assert_eq!(
            idempotency_conflict["error"]["code"],
            "IDEMPOTENCY_CONFLICT"
        );

        let missing_revision = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/restore"),
            Some(TEST_TOKEN),
            Some(
                json!({
                    "targetRevision": 99,
                    "expectedRevision": 3,
                    "idempotencyKey": "api-missing-restore"
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(missing_revision.status(), StatusCode::NOT_FOUND);
        let missing_revision: Value = response_json(missing_revision).await;
        assert_eq!(missing_revision["error"]["code"], "REVISION_NOT_FOUND");
    }

    #[tokio::test]
    async fn render_outputs_are_addressed_by_asset_id_and_retried_jobs_are_idempotent() {
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let workspace = Workspace::open(&workspace_path).unwrap();
        install_video_fixture(&workspace, root.path());
        let store = ProjectStore::open(root.path().join("projects.sqlite")).unwrap();
        store
            .create_from_timeline_json(&workspace_path, timeline_document())
            .unwrap();
        let app = build_router(
            store.clone(),
            workspace_path.clone(),
            TEST_TOKEN.to_owned(),
            JobCancellationRegistry::default(),
        );

        let source_bytes = b"fixture source bytes";
        let source_metadata = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{ASSET_ID}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(source_metadata.status(), StatusCode::OK);
        let source_metadata: Value = response_json(source_metadata).await;
        assert_eq!(source_metadata["assetId"], ASSET_ID);
        assert_eq!(source_metadata["kind"], "video");
        assert_eq!(source_metadata["mimeType"], "application/octet-stream");
        assert_eq!(source_metadata["bytes"], source_bytes.len());
        assert_eq!(
            source_metadata["sha256"],
            hex::encode(Sha256::digest(source_bytes))
        );
        assert_eq!(source_metadata["probe"]["video"]["width"], 320);
        assert!(source_metadata["presetId"].is_null());

        let source_content = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{ASSET_ID}/content"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(source_content.status(), StatusCode::OK);
        assert_eq!(
            source_content.headers()[CONTENT_TYPE],
            "application/octet-stream"
        );
        let source_content = to_bytes(source_content.into_body(), 1024).await.unwrap();
        assert_eq!(&source_content[..], source_bytes);

        let source_path = workspace_path
            .join("assets")
            .join(ASSET_ID)
            .join(SOURCE_FILENAME);
        let changed_source_bytes = b"changed source bytes";
        assert_eq!(changed_source_bytes.len(), source_bytes.len());
        fs::write(&source_path, changed_source_bytes).unwrap();
        let changed_source = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{ASSET_ID}/content"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(changed_source.status(), StatusCode::INTERNAL_SERVER_ERROR);
        fs::write(&source_path, source_bytes).unwrap();

        let missing_asset = request(
            &app,
            Method::GET,
            "/v1/assets/018f2e40-4d28-7e31-8b63-611d49c43b0d",
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(missing_asset.status(), StatusCode::NOT_FOUND);
        let missing_asset: Value = response_json(missing_asset).await;
        assert_eq!(missing_asset["error"]["code"], "ASSET_NOT_FOUND");

        let submitted = store
            .enqueue_render_job(PROJECT_ID, 1, "export", "asset-output-job")
            .unwrap();
        let claimed = store
            .claim_next_render_job("asset-output-worker", 45)
            .unwrap()
            .unwrap();
        let attempt_dir = workspace
            .create_render_attempt_dir(&claimed.job_id, claimed.attempt)
            .unwrap();
        let output_bytes = b"validated mp4 fixture";
        fs::write(attempt_dir.join("output.mp4"), output_bytes).unwrap();
        fs::write(attempt_dir.join("output.mp4.provenance.json"), b"{}").unwrap();
        let final_dir = workspace
            .publish_render_attempt(&claimed.job_id, claimed.attempt)
            .unwrap();
        let output = final_dir.join("output.mp4");
        let output_asset_id = "018f2e40-4d28-7e31-8b63-611d49c43b0c";
        let result = crate::RenderResult {
            output: output.clone(),
            provenance: PathBuf::from(format!("{}.provenance.json", output.display())),
            timeline_sha256: "1".repeat(64),
            output_sha256: hex::encode(Sha256::digest(output_bytes)),
            output_duration_seconds: 1.0,
            output_width: 320,
            output_height: 240,
            output_frame_count: 24,
            output_frame_rate: "24/1".to_owned(),
            video_codec: "h264".to_owned(),
            audio_codec: None,
        };
        assert!(
            store
                .complete_render_job(
                    &claimed.job_id,
                    "asset-output-worker",
                    output_asset_id,
                    &result,
                )
                .unwrap()
        );

        let job_response = request(
            &app,
            Method::GET,
            &format!("/v1/jobs/{}", submitted.job.job_id),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(job_response.status(), StatusCode::OK);
        let job_response: Value = response_json(job_response).await;
        assert_eq!(job_response["outputAssetId"], output_asset_id);
        assert!(job_response["result"]["output"].is_null());

        let metadata = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{output_asset_id}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(metadata.status(), StatusCode::OK);
        let metadata: Value = response_json(metadata).await;
        assert_eq!(metadata["kind"], "video");
        assert_eq!(metadata["mimeType"], "video/mp4");
        assert_eq!(metadata["presetId"], "export");
        assert_eq!(metadata["bytes"], output_bytes.len());
        assert_eq!(
            metadata["sha256"],
            hex::encode(Sha256::digest(output_bytes))
        );

        let unauthorized = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{output_asset_id}/content"),
            None,
            None,
        )
        .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let content = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{output_asset_id}/content"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(content.status(), StatusCode::OK);
        assert_eq!(content.headers()[CONTENT_TYPE], "video/mp4");
        let content = to_bytes(content.into_body(), 1024).await.unwrap();
        assert_eq!(&content[..], output_bytes);

        let changed_output_bytes = b"corrupted mp4 fixture";
        assert_eq!(changed_output_bytes.len(), output_bytes.len());
        fs::write(&output, changed_output_bytes).unwrap();
        let changed_output = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{output_asset_id}/content"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(changed_output.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let failed = store
            .enqueue_render_job(PROJECT_ID, 1, "export", "failed-render-for-retry")
            .unwrap();
        let failed_claim = store
            .claim_next_render_job("failed-render-worker", 45)
            .unwrap()
            .unwrap();
        assert_eq!(failed_claim.job_id, failed.job.job_id);
        assert!(
            store
                .fail_render_job(&failed_claim.job_id, "failed-render-worker", "test failure")
                .unwrap()
        );
        let retry_request = json!({"idempotencyKey": "retry-render-1"});
        let retried = request(
            &app,
            Method::POST,
            &format!("/v1/jobs/{}/retry", failed.job.job_id),
            Some(TEST_TOKEN),
            Some(retry_request.to_string()),
        )
        .await;
        assert_eq!(retried.status(), StatusCode::ACCEPTED);
        let retried: Value = response_json(retried).await;
        assert_eq!(retried["job"]["status"], "queued");
        assert_eq!(retried["job"]["retryNumber"], 1);

        let replay = request(
            &app,
            Method::POST,
            &format!("/v1/jobs/{}/retry", failed.job.job_id),
            Some(TEST_TOKEN),
            Some(retry_request.to_string()),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        let replay: Value = response_json(replay).await;
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["job"]["jobId"], retried["job"]["jobId"]);
    }

    fn install_video_fixture(workspace: &Workspace, root: &Path) {
        let source = b"fixture source bytes";
        let directory = root.join("workspace").join("assets").join(ASSET_ID);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(SOURCE_FILENAME), source).unwrap();
        let manifest = AssetManifest {
            schema_version: 1,
            asset_id: ASSET_ID.to_owned(),
            sha256: hex::encode(Sha256::digest(source)),
            bytes: source.len() as u64,
            kind: AssetKind::Video,
            probe: MediaProbe {
                format_name: "mov,mp4".to_owned(),
                duration: "1.0".to_owned(),
                duration_seconds: 1.0,
                has_video: true,
                has_audio: false,
                video: Some(StreamInfo {
                    codec: "h264".to_owned(),
                    duration_seconds: Some(1.0),
                    frame_rate: Some("24/1".to_owned()),
                    width: Some(320),
                    height: Some(240),
                    sample_rate: None,
                    channels: None,
                }),
                audio: None,
            },
        };
        fs::write(
            directory.join("asset.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(workspace.load_asset(ASSET_ID).is_ok());
    }

    fn timeline_document() -> Value {
        json!({
            "schemaVersion": "1.0.0",
            "projectId": PROJECT_ID,
            "revision": 0,
            "canvas": {
                "width": 320, "height": 240,
                "fps": {"num": 24, "den": 1}, "background": "#101820"
            },
            "assets": {
                ASSET_ID: {
                    "kind": "video",
                    "uri": format!("asset://{ASSET_ID}"),
                    "sha256": hex::encode(Sha256::digest(b"fixture source bytes"))
                }
            },
            "tracks": [
                {
                    "id": "video-main", "kind": "video", "order": 0,
                    "clips": [{
                        "id": "picture", "kind": "video", "assetId": ASSET_ID,
                        "startFrame": 0, "durationFrames": 24,
                        "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 24},
                        "sourceOut": {"ticks": 24, "timebase_num": 1, "timebase_den": 24}
                    }]
                },
                {
                    "id": "titles", "kind": "overlay", "order": 1,
                    "clips": [{
                        "id": "title", "kind": "text", "startFrame": 0,
                        "durationFrames": 24, "text": "Original title", "fontSize": 32,
                        "color": "#FFFFFF", "x": 10, "y": 20
                    }]
                }
            ]
        })
    }

    async fn request(
        app: &Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<String>,
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.unwrap_or_default())).unwrap())
            .await
            .unwrap()
    }

    async fn response_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), MAX_API_BODY_BYTES)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
