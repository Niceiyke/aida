use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Weak},
    time::SystemTime,
};

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
    routing::{delete, get, post, put},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    AssetKind, EditOperation, EditResult, ImportedAsset, MediaProbe, ProjectRestoreResult,
    ProjectRevision, ProjectRevisionHistory, ProjectStore, ProjectSummary, RenderJob, Workspace,
    jobs::{JobCancellationRegistry, RenderJobRunner},
    media::{MAX_IMPORT_BYTES, complete_uploaded_asset},
    workspace::{sanitize_media_file_name, validate_asset_id},
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
    ffprobe: PathBuf,
    cancellation: JobCancellationRegistry,
    uploads: UploadControl,
}

#[derive(Clone, Default)]
struct UploadControl {
    creation_lock: Arc<tokio::sync::Mutex<()>>,
    session_locks: Arc<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
}

impl UploadControl {
    fn session_lock(&self, upload_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .session_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(upload_id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(upload_id.to_owned(), Arc::downgrade(&lock));
        lock
    }
}

const MAX_CONCURRENT_UPLOADS: usize = 4;

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateUploadRequest {
    expected_bytes: u64,
    #[serde(default)]
    file_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadSessionResponse {
    upload_id: String,
    expected_bytes: u64,
    upload_url: String,
    complete_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadReceivedResponse {
    upload_id: String,
    bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteUploadResponse {
    asset: ImportedAsset,
    replayed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetMetadata {
    asset_id: String,
    kind: AssetKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
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
    file_name: Option<String>,
    mime_type: String,
    sha256: String,
    bytes: u64,
    path: PathBuf,
    filename: String,
    preset_id: Option<String>,
    probe: Option<MediaProbe>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetListResponse {
    assets: Vec<AssetMetadata>,
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
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_owned(),
            message: message.into(),
            current_revision: None,
        }
    }

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
        } else if details.contains("UPLOAD_NOT_FOUND") {
            (StatusCode::NOT_FOUND, "UPLOAD_NOT_FOUND", details)
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
    let ffprobe = options.ffprobe.clone();
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
        ffprobe,
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
    ffprobe: PathBuf,
    bearer_token: String,
    cancellation: JobCancellationRegistry,
) -> Router {
    let state = ApiState {
        store,
        workspace,
        ffprobe,
        cancellation,
        uploads: UploadControl::default(),
    };
    Router::new()
        .route("/v1/health", get(get_health))
        .route("/v1/projects", get(list_projects).post(create_project))
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
        .route("/v1/assets/uploads", post(create_asset_upload))
        .route("/v1/assets", get(list_assets))
        .route(
            "/v1/assets/uploads/{upload_id}/content",
            put(put_asset_upload),
        )
        .route(
            "/v1/assets/uploads/{upload_id}/complete",
            post(complete_asset_upload),
        )
        .route(
            "/v1/assets/uploads/{upload_id}",
            delete(cancel_asset_upload),
        )
        .route("/v1/assets/{asset_id}", get(get_asset_metadata))
        .route("/v1/assets/{asset_id}/content", get(get_asset_content))
        .layer(DefaultBodyLimit::max(MAX_API_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(
            Arc::<[u8]>::from(bearer_token.into_bytes()),
            require_bearer,
        ))
        .with_state(state)
}

async fn get_health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn list_projects(
    State(state): State<ApiState>,
) -> std::result::Result<Json<Vec<ProjectSummary>>, ApiError> {
    Ok(Json(state.store.list_project_summaries()?))
}

async fn list_assets(
    State(state): State<ApiState>,
) -> std::result::Result<Json<AssetListResponse>, ApiError> {
    let workspace_path = state.workspace.clone();
    let manifests =
        tokio::task::spawn_blocking(move || Workspace::open(&workspace_path)?.list_assets())
            .await
            .map_err(|_| ApiError::internal("The asset library lookup did not complete"))??;
    let assets = manifests
        .into_iter()
        .map(|manifest| AssetMetadata {
            asset_id: manifest.asset_id.clone(),
            kind: manifest.kind,
            mime_type: media_mime_type(
                manifest.kind,
                manifest.file_name.as_deref(),
                &manifest.probe.format_name,
            ),
            file_name: manifest.file_name,
            sha256: manifest.sha256,
            bytes: manifest.bytes,
            content_url: format!("/v1/assets/{}/content", manifest.asset_id),
            preset_id: None,
            probe: Some(manifest.probe),
        })
        .collect();
    Ok(Json(AssetListResponse { assets }))
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

async fn create_asset_upload(
    State(state): State<ApiState>,
    payload: std::result::Result<Json<CreateUploadRequest>, JsonRejection>,
) -> std::result::Result<Response, ApiError> {
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    if request.expected_bytes == 0 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_FAILED",
            "expectedBytes must be greater than zero",
        ));
    }
    if request.expected_bytes > MAX_IMPORT_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "uploads are limited to 2 GiB",
        ));
    }
    let file_name = request
        .file_name
        .as_deref()
        .map(sanitize_media_file_name)
        .transpose()
        .map_err(|_| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "VALIDATION_FAILED",
                "fileName must be a plain filename up to 255 bytes",
            )
        })?;

    let _creation_guard = state.uploads.creation_lock.lock().await;
    let workspace_path = state.workspace.clone();
    let expired_sessions = tokio::task::spawn_blocking(move || {
        Workspace::open(&workspace_path)?.expired_upload_session_ids(SystemTime::now())
    })
    .await
    .map_err(|_| ApiError::internal("The upload cleanup scan did not complete"))??;
    for expired_id in expired_sessions {
        let session_lock = state.uploads.session_lock(&expired_id);
        let _session_guard = session_lock.lock().await;
        let workspace_path = state.workspace.clone();
        tokio::task::spawn_blocking(move || {
            Workspace::open(&workspace_path)?
                .remove_upload_session_if_expired(&expired_id, SystemTime::now())
        })
        .await
        .map_err(|_| ApiError::internal("The expired upload cleanup did not complete"))??;
    }

    let workspace_path = state.workspace.clone();
    let (workspace, session_count, reserved_bytes) = tokio::task::spawn_blocking(move || {
        let workspace = Workspace::open(&workspace_path)?;
        let (session_count, reserved_bytes) = workspace.upload_session_usage()?;
        Ok::<_, anyhow::Error>((workspace, session_count, reserved_bytes))
    })
    .await
    .map_err(|_| ApiError::internal("The upload workspace check did not complete"))??;
    let reservation_total = reserved_bytes.checked_add(request.expected_bytes);
    if session_count >= MAX_CONCURRENT_UPLOADS
        || reservation_total.is_none_or(|total| total > MAX_IMPORT_BYTES)
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "QUOTA_EXCEEDED",
            "The upload staging quota is full; complete or cancel an existing upload",
        ));
    }

    let upload_id = Uuid::new_v4().to_string();
    let created_id = upload_id.clone();
    tokio::task::spawn_blocking(move || {
        workspace.create_upload_session(&created_id, request.expected_bytes, file_name)
    })
    .await
    .map_err(|_| ApiError::internal("The upload session worker did not complete"))??;

    let body = UploadSessionResponse {
        upload_id: upload_id.clone(),
        expected_bytes: request.expected_bytes,
        upload_url: format!("/v1/assets/uploads/{upload_id}/content"),
        complete_url: format!("/v1/assets/uploads/{upload_id}/complete"),
    };
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

async fn put_asset_upload(
    State(state): State<ApiState>,
    Path(upload_id): Path<String>,
    body: Body,
) -> std::result::Result<Json<UploadReceivedResponse>, ApiError> {
    validate_asset_id(&upload_id).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "VALIDATION_FAILED",
            "uploadId must be a canonical UUID",
        )
    })?;
    let session_lock = state.uploads.session_lock(&upload_id);
    let _session_guard = session_lock.lock().await;
    let workspace_path = state.workspace.clone();
    let session_id = upload_id.clone();
    let session = tokio::task::spawn_blocking(move || {
        Workspace::open(&workspace_path)?.load_upload_session(&session_id)
    })
    .await
    .map_err(|_| ApiError::internal("The upload session lookup did not complete"))??;
    if session.is_expired(SystemTime::now()) {
        return Err(ApiError::new(
            StatusCode::GONE,
            "UPLOAD_EXPIRED",
            "The upload session expired after 24 hours of inactivity",
        ));
    }
    let workspace_path = state.workspace.clone();
    let touch_id = upload_id.clone();
    tokio::task::spawn_blocking(move || {
        Workspace::open(&workspace_path)?.touch_upload_session(&touch_id)
    })
    .await
    .map_err(|_| ApiError::internal("The upload activity update did not complete"))??;

    if tokio::fs::try_exists(&session.source)
        .await
        .map_err(|_| ApiError::internal("The upload destination could not be inspected"))?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "UPLOAD_ALREADY_RECEIVED",
            "Content has already been received for this upload session",
        ));
    }
    if tokio::fs::try_exists(&session.partial)
        .await
        .map_err(|_| ApiError::internal("The partial upload could not be inspected"))?
    {
        tokio::fs::remove_file(&session.partial)
            .await
            .map_err(|_| ApiError::internal("The previous partial upload could not be removed"))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&session.partial)
        .await
        .map_err(|_| ApiError::internal("The upload staging file could not be created"))?;
    let mut stream = body.into_data_stream();
    let receive_result = async {
        let mut received = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "UPLOAD_STREAM_FAILED",
                    "The request body stream ended unexpectedly",
                )
            })?;
            let chunk_bytes = u64::try_from(chunk.len()).map_err(|_| {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "PAYLOAD_TOO_LARGE",
                    "The upload body is too large",
                )
            })?;
            received = received.checked_add(chunk_bytes).ok_or_else(|| {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "PAYLOAD_TOO_LARGE",
                    "The upload body is too large",
                )
            })?;
            if received > session.expected_bytes || received > MAX_IMPORT_BYTES {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "PAYLOAD_TOO_LARGE",
                    "The upload body exceeds its reserved size",
                ));
            }
            file.write_all(&chunk)
                .await
                .map_err(|_| ApiError::internal("The upload body could not be staged"))?;
        }
        if received != session.expected_bytes {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "UPLOAD_SIZE_MISMATCH",
                format!(
                    "Expected {} bytes but received {received}",
                    session.expected_bytes
                ),
            ));
        }
        file.sync_all()
            .await
            .map_err(|_| ApiError::internal("The staged upload could not be flushed"))?;
        Ok::<_, ApiError>(received)
    }
    .await;

    let received = match receive_result {
        Ok(received) => received,
        Err(error) => {
            drop(file);
            let _ = tokio::fs::remove_file(&session.partial).await;
            return Err(error);
        }
    };
    drop(file);
    if tokio::fs::rename(&session.partial, &session.source)
        .await
        .is_err()
    {
        let _ = tokio::fs::remove_file(&session.partial).await;
        return Err(ApiError::internal(
            "The completed upload could not be staged",
        ));
    }
    let workspace_path = state.workspace.clone();
    let touch_id = upload_id.clone();
    tokio::task::spawn_blocking(move || {
        Workspace::open(&workspace_path)?.touch_upload_session(&touch_id)
    })
    .await
    .map_err(|_| ApiError::internal("The upload activity update did not complete"))??;
    Ok(Json(UploadReceivedResponse {
        upload_id,
        bytes: received,
    }))
}

async fn complete_asset_upload(
    State(state): State<ApiState>,
    Path(upload_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    validate_asset_id(&upload_id).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "VALIDATION_FAILED",
            "uploadId must be a canonical UUID",
        )
    })?;
    let session_lock = state.uploads.session_lock(&upload_id);
    let _session_guard = session_lock.lock().await;

    let workspace_path = state.workspace.clone();
    let existing_id = upload_id.clone();
    let existing_asset = tokio::task::spawn_blocking(move || {
        let workspace = Workspace::open(&workspace_path)?;
        match workspace.load_asset(&existing_id) {
            Ok((_, manifest)) => Ok(Some(ImportedAsset {
                asset_id: manifest.asset_id.clone(),
                uri: format!("asset://{}", manifest.asset_id),
                sha256: manifest.sha256,
                kind: manifest.kind,
                bytes: manifest.bytes,
                file_name: manifest.file_name,
                probe: manifest.probe,
            })),
            Err(error) if format!("{error:#}").contains("ASSET_NOT_FOUND") => Ok(None),
            Err(error) => Err(error),
        }
    })
    .await
    .map_err(|_| ApiError::internal("The asset lookup worker did not complete"))??;
    if let Some(asset) = existing_asset {
        return Ok((
            StatusCode::OK,
            Json(CompleteUploadResponse {
                asset,
                replayed: true,
            }),
        )
            .into_response());
    }

    let workspace_path = state.workspace.clone();
    let session_id = upload_id.clone();
    let session = tokio::task::spawn_blocking(move || {
        Workspace::open(&workspace_path)?.load_upload_session(&session_id)
    })
    .await
    .map_err(|_| ApiError::internal("The upload session lookup did not complete"))??;
    if session.is_expired(SystemTime::now()) {
        return Err(ApiError::new(
            StatusCode::GONE,
            "UPLOAD_EXPIRED",
            "The upload session expired after 24 hours of inactivity",
        ));
    }
    let workspace_path = state.workspace.clone();
    let touch_id = upload_id.clone();
    tokio::task::spawn_blocking(move || {
        Workspace::open(&workspace_path)?.touch_upload_session(&touch_id)
    })
    .await
    .map_err(|_| ApiError::internal("The upload activity update did not complete"))??;

    let workspace = state.workspace.clone();
    let ffprobe = state.ffprobe.clone();
    let complete_id = upload_id.clone();
    let asset = tokio::task::spawn_blocking(move || {
        complete_uploaded_asset(&workspace, &complete_id, &ffprobe)
    })
    .await
    .map_err(|_| ApiError::internal("The uploaded media probe did not complete"))??;
    Ok((
        StatusCode::CREATED,
        Json(CompleteUploadResponse {
            asset,
            replayed: false,
        }),
    )
        .into_response())
}

async fn cancel_asset_upload(
    State(state): State<ApiState>,
    Path(upload_id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    validate_asset_id(&upload_id).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "VALIDATION_FAILED",
            "uploadId must be a canonical UUID",
        )
    })?;
    let session_lock = state.uploads.session_lock(&upload_id);
    let _session_guard = session_lock.lock().await;
    let workspace_path = state.workspace.clone();
    tokio::task::spawn_blocking(move || {
        Workspace::open(&workspace_path)?.remove_upload_session(&upload_id)
    })
    .await
    .map_err(|_| ApiError::internal("The upload cancellation did not complete"))??;
    Ok(StatusCode::NO_CONTENT)
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
                mime_type: media_mime_type(
                    manifest.kind,
                    manifest.file_name.as_deref(),
                    &manifest.probe.format_name,
                ),
                file_name: manifest.file_name,
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
        file_name: None,
        mime_type: "video/mp4".to_owned(),
        sha256: asset.output_sha256.to_ascii_lowercase(),
        bytes: metadata.len(),
        path,
        filename: format!("render-{asset_id}.mp4"),
        preset_id: Some(asset.preset_id),
        probe: None,
    })
}

fn media_mime_type(kind: AssetKind, file_name: Option<&str>, format_name: &str) -> String {
    let extension = file_name
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime_type = match (kind, extension.as_str()) {
        (AssetKind::Video, "mp4" | "m4v") => "video/mp4",
        (AssetKind::Video, "mov") => "video/quicktime",
        (AssetKind::Video, "mkv") => "video/x-matroska",
        (AssetKind::Audio, "mp3") => "audio/mpeg",
        (AssetKind::Audio, "wav") => "audio/wav",
        (AssetKind::Audio, "aac") => "audio/aac",
        (AssetKind::Audio, "m4a") => "audio/mp4",
        (AssetKind::Video, _) if format_name.contains("matroska") => "video/x-matroska",
        (AssetKind::Video, _) => "video/mp4",
        (AssetKind::Audio, _) if format_name.contains("wav") => "audio/wav",
        (AssetKind::Audio, _) if format_name.contains("mp3") => "audio/mpeg",
        (AssetKind::Audio, _) => "audio/mp4",
    };
    mime_type.to_owned()
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
        file_name: asset.file_name,
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
    use std::{fs, path::Path, path::PathBuf, process::Command};

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
    async fn health_endpoint_reports_liveness_and_requires_authentication() {
        let root = tempdir().unwrap();
        let app = build_router(
            ProjectStore::open(root.path().join("projects.sqlite")).unwrap(),
            root.path().join("workspace"),
            "ffprobe".into(),
            TEST_TOKEN.to_owned(),
            JobCancellationRegistry::default(),
        );

        let unauthorized = request(&app, Method::GET, "/v1/health", None, None).await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let response = request(&app, Method::GET, "/v1/health", Some(TEST_TOKEN), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await["status"], "ok");
    }

    #[tokio::test]
    async fn editor_library_and_clip_operations_are_revisioned_and_idempotent() {
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let workspace = Workspace::open(&workspace_path).unwrap();
        install_video_fixture(&workspace, root.path());
        let app = build_router(
            ProjectStore::open(root.path().join("projects.sqlite")).unwrap(),
            workspace_path.clone(),
            "ffprobe".into(),
            TEST_TOKEN.to_owned(),
            JobCancellationRegistry::default(),
        );

        let unsafe_name = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": 1, "fileName": "../outside.wav"}).to_string()),
        )
        .await;
        assert_eq!(unsafe_name.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let reservation = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": 1, "fileName": "voice.wav"}).to_string()),
        )
        .await;
        assert_eq!(reservation.status(), StatusCode::CREATED);
        let reservation: Value = response_json(reservation).await;
        let upload_id = reservation["uploadId"].as_str().unwrap();
        assert_eq!(
            Workspace::open(&workspace_path)
                .unwrap()
                .load_upload_session(upload_id)
                .unwrap()
                .file_name
                .as_deref(),
            Some("voice.wav")
        );
        let cancelled = request(
            &app,
            Method::DELETE,
            &format!("/v1/assets/uploads/{upload_id}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);

        let assets = request(&app, Method::GET, "/v1/assets", Some(TEST_TOKEN), None).await;
        assert_eq!(assets.status(), StatusCode::OK);
        let assets: Value = response_json(assets).await;
        assert_eq!(assets["assets"].as_array().unwrap().len(), 1);
        assert_eq!(assets["assets"][0]["fileName"], "fixture.mp4");
        assert_eq!(assets["assets"][0]["mimeType"], "video/mp4");

        let created = request(
            &app,
            Method::POST,
            "/v1/projects",
            Some(TEST_TOKEN),
            Some(timeline_document().to_string()),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);

        let projects = request(&app, Method::GET, "/v1/projects", Some(TEST_TOKEN), None).await;
        assert_eq!(projects.status(), StatusCode::OK);
        let projects: Value = response_json(projects).await;
        assert_eq!(projects.as_array().unwrap().len(), 1);
        assert_eq!(projects[0]["projectId"], PROJECT_ID);
        assert_eq!(projects[0]["currentRevision"], 1);

        let operations = json!({
            "expectedRevision": 1,
            "idempotencyKey": "editor-multiclip-batch",
            "operations": [
                {
                    "type":"clip.add", "trackId":"video-main", "index":1,
                    "clip":{
                        "id":"video-second","kind":"video","assetId":ASSET_ID,
                        "startFrame":24,"durationFrames":24,
                        "sourceIn":{"ticks":0,"timebase_num":1,"timebase_den":1},
                        "sourceOut":{"ticks":1,"timebase_num":1,"timebase_den":1}
                    }
                },
                {"type":"clip.split","clipId":"video-second","splitFrame":36},
                {"type":"clip.reorder","clipId":"picture","destinationIndex":2},
                {
                    "type":"clip.trim","clipId":"picture","startFrame":24,
                    "durationFrames":12,
                    "sourceIn":{"ticks":0,"timebase_num":1,"timebase_den":1},
                    "sourceOut":{"ticks":500_000,"timebase_num":1,"timebase_den":1_000_000}
                },
                {"type":"clip.remove","clipId":"video-second"}
            ]
        });
        let edit = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{PROJECT_ID}/operations"),
            Some(TEST_TOKEN),
            Some(operations.to_string()),
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
            Some(operations.to_string()),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(response_json(replay).await["replayed"], true);

        let timeline = request(
            &app,
            Method::GET,
            &format!("/v1/projects/{PROJECT_ID}/timeline"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(timeline.status(), StatusCode::OK);
        let timeline: Value = response_json(timeline).await;
        assert_eq!(timeline["revision"], 2);
        assert_eq!(timeline["tracks"][0]["clips"].as_array().unwrap().len(), 2);
        assert_eq!(timeline["tracks"][0]["clips"][1]["id"], "picture");
        assert_eq!(timeline["tracks"][0]["clips"][1]["startFrame"], 12);
    }

    #[tokio::test]
    async fn uploads_survive_restart_and_are_bounded_validated_idempotent_and_downloadable() {
        let Some(ffmpeg) = configured_tool("STUDIO_TEST_FFMPEG", "ffmpeg") else {
            eprintln!("skipping media upload integration test: ffmpeg not found");
            return;
        };
        let Some(ffprobe) = configured_tool("STUDIO_TEST_FFPROBE", "ffprobe") else {
            eprintln!("skipping media upload integration test: ffprobe not found");
            return;
        };
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let source_path = root.path().join("uploaded.mp4");
        make_upload_fixture(&ffmpeg, &source_path);
        let source_bytes = fs::read(&source_path).unwrap();
        assert!(source_bytes.len() > MAX_API_BODY_BYTES);
        let app = build_router(
            ProjectStore::open(root.path().join("projects.sqlite")).unwrap(),
            workspace_path.clone(),
            ffprobe.clone(),
            TEST_TOKEN.to_owned(),
            JobCancellationRegistry::default(),
        );

        let unauthorized = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            None,
            Some(json!({"expectedBytes": source_bytes.len()}).to_string()),
        )
        .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let too_large = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": MAX_IMPORT_BYTES + 1}).to_string()),
        )
        .await;
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let reservation = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": MAX_IMPORT_BYTES}).to_string()),
        )
        .await;
        assert_eq!(reservation.status(), StatusCode::CREATED);
        let reservation: Value = response_json(reservation).await;
        let reservation_id = reservation["uploadId"].as_str().unwrap();
        let over_quota = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": 1}).to_string()),
        )
        .await;
        assert_eq!(over_quota.status(), StatusCode::TOO_MANY_REQUESTS);
        let cancelled = request(
            &app,
            Method::DELETE,
            &format!("/v1/assets/uploads/{reservation_id}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);

        let bounded = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": 4}).to_string()),
        )
        .await;
        let bounded: Value = response_json(bounded).await;
        let excessive = raw_request(
            &app,
            Method::PUT,
            bounded["uploadUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            b"12345",
        )
        .await;
        assert_eq!(excessive.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let retried = raw_request(
            &app,
            Method::PUT,
            bounded["uploadUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            b"1234",
        )
        .await;
        assert_eq!(retried.status(), StatusCode::OK);
        let cancelled = request(
            &app,
            Method::DELETE,
            &format!(
                "/v1/assets/uploads/{}",
                bounded["uploadId"].as_str().unwrap()
            ),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);

        let created = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(
                json!({"expectedBytes": source_bytes.len(), "fileName": "uploaded.mp4"})
                    .to_string(),
            ),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: Value = response_json(created).await;
        let upload_id = created["uploadId"].as_str().unwrap().to_owned();
        assert_eq!(created["expectedBytes"], source_bytes.len());
        let resumed_app = build_router(
            ProjectStore::open(root.path().join("projects.sqlite")).unwrap(),
            workspace_path,
            ffprobe,
            TEST_TOKEN.to_owned(),
            JobCancellationRegistry::default(),
        );

        let incomplete = raw_request(
            &resumed_app,
            Method::PUT,
            created["uploadUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            &source_bytes[..source_bytes.len() - 1],
        )
        .await;
        assert_eq!(incomplete.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let uploaded = raw_request(
            &resumed_app,
            Method::PUT,
            created["uploadUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            &source_bytes,
        )
        .await;
        assert_eq!(uploaded.status(), StatusCode::OK);
        assert_eq!(response_json(uploaded).await["bytes"], source_bytes.len());

        let complete_url = created["completeUrl"].as_str().unwrap();
        let completed = request(
            &resumed_app,
            Method::POST,
            complete_url,
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(completed.status(), StatusCode::CREATED);
        let completed: Value = response_json(completed).await;
        assert_eq!(completed["replayed"], false);
        assert_eq!(completed["asset"]["assetId"], upload_id);
        assert_eq!(completed["asset"]["kind"], "video");
        assert_eq!(completed["asset"]["fileName"], "uploaded.mp4");
        let expected_hash = hex::encode(Sha256::digest(&source_bytes));
        assert_eq!(completed["asset"]["sha256"], expected_hash);

        let replay = request(
            &resumed_app,
            Method::POST,
            complete_url,
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        let replay: Value = response_json(replay).await;
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["asset"]["assetId"], upload_id);

        let download = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{upload_id}/content"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(download.status(), StatusCode::OK);
        assert_eq!(download.headers()[CONTENT_TYPE], "video/mp4");
        let downloaded = to_bytes(download.into_body(), source_bytes.len() + 1)
            .await
            .unwrap();
        assert_eq!(&downloaded[..], &source_bytes);
        assert_eq!(hex::encode(Sha256::digest(&downloaded)), expected_hash);

        let invalid_bytes = b"not a media file";
        let invalid = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": invalid_bytes.len()}).to_string()),
        )
        .await;
        let invalid: Value = response_json(invalid).await;
        let invalid_id = invalid["uploadId"].as_str().unwrap();
        let invalid_content = raw_request(
            &app,
            Method::PUT,
            invalid["uploadUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            invalid_bytes,
        )
        .await;
        assert_eq!(invalid_content.status(), StatusCode::OK);
        let invalid_complete = request(
            &app,
            Method::POST,
            invalid["completeUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(invalid_complete.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let cancelled = request(
            &app,
            Method::DELETE,
            &format!("/v1/assets/uploads/{invalid_id}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn expired_uploads_are_rejected_and_cleaned_before_new_reservations() {
        use std::time::Duration;

        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let app = build_router(
            ProjectStore::open(root.path().join("projects.sqlite")).unwrap(),
            workspace_path.clone(),
            "ffprobe".into(),
            TEST_TOKEN.to_owned(),
            JobCancellationRegistry::default(),
        );
        let created = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": 8}).to_string()),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: Value = response_json(created).await;
        let upload_id = created["uploadId"].as_str().unwrap();
        let session_dir = workspace_path.join("tmp").join("uploads").join(upload_id);
        let old_activity = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
        fs::OpenOptions::new()
            .write(true)
            .open(session_dir.join("upload.json"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_activity))
            .unwrap();

        let expired_put = raw_request(
            &app,
            Method::PUT,
            created["uploadUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            b"12345678",
        )
        .await;
        assert_eq!(expired_put.status(), StatusCode::GONE);
        assert_eq!(
            response_json(expired_put).await["error"]["code"],
            "UPLOAD_EXPIRED"
        );
        let expired_complete = request(
            &app,
            Method::POST,
            created["completeUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(expired_complete.status(), StatusCode::GONE);

        let next = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": 1}).to_string()),
        )
        .await;
        assert_eq!(next.status(), StatusCode::CREATED);
        assert!(!session_dir.exists());
    }

    #[tokio::test]
    async fn uploaded_asset_can_be_used_to_render_and_download_a_verified_output() {
        let Some(ffmpeg) = configured_tool("STUDIO_TEST_FFMPEG", "ffmpeg") else {
            eprintln!("skipping API render flow test: ffmpeg not found");
            return;
        };
        let Some(ffprobe) = configured_tool("STUDIO_TEST_FFPROBE", "ffprobe") else {
            eprintln!("skipping API render flow test: ffprobe not found");
            return;
        };
        let root = tempdir().unwrap();
        let workspace_path = root.path().join("workspace");
        let database = root.path().join("projects.sqlite");
        let source_path = root.path().join("render-source.mp4");
        make_simple_render_fixture(&ffmpeg, &source_path);
        let source_bytes = fs::read(&source_path).unwrap();
        let store = ProjectStore::open(&database).unwrap();
        let cancellation = JobCancellationRegistry::default();
        let app = build_router(
            store.clone(),
            workspace_path.clone(),
            ffprobe.clone(),
            TEST_TOKEN.to_owned(),
            cancellation.clone(),
        );

        let created_upload = request(
            &app,
            Method::POST,
            "/v1/assets/uploads",
            Some(TEST_TOKEN),
            Some(json!({"expectedBytes": source_bytes.len()}).to_string()),
        )
        .await;
        assert_eq!(created_upload.status(), StatusCode::CREATED);
        let created_upload: Value = response_json(created_upload).await;
        let upload = raw_request(
            &app,
            Method::PUT,
            created_upload["uploadUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            &source_bytes,
        )
        .await;
        assert_eq!(upload.status(), StatusCode::OK);
        let completed = request(
            &app,
            Method::POST,
            created_upload["completeUrl"].as_str().unwrap(),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(completed.status(), StatusCode::CREATED);
        let completed: Value = response_json(completed).await;
        let uploaded_asset_id = completed["asset"]["assetId"].as_str().unwrap();
        let uploaded_asset_sha256 = completed["asset"]["sha256"].as_str().unwrap();

        let project = request(
            &app,
            Method::POST,
            "/v1/projects",
            Some(TEST_TOKEN),
            Some(timeline_for_uploaded_asset(uploaded_asset_id, uploaded_asset_sha256).to_string()),
        )
        .await;
        assert_eq!(project.status(), StatusCode::CREATED);
        let project: Value = response_json(project).await;
        assert_eq!(project["revision"], 1);
        let project_id = project["projectId"].as_str().unwrap();

        let queued = request(
            &app,
            Method::POST,
            &format!("/v1/projects/{project_id}/renders"),
            Some(TEST_TOKEN),
            Some(
                json!({
                    "revision": 1,
                    "presetId": "export",
                    "idempotencyKey": "uploaded-asset-render"
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(queued.status(), StatusCode::ACCEPTED);
        let queued: Value = response_json(queued).await;
        let job_id = queued["job"]["jobId"].as_str().unwrap();

        let runner = RenderJobRunner::new(
            store,
            workspace_path,
            ffmpeg,
            ffprobe,
            None,
            120,
            cancellation,
        );
        assert!(runner.run_once().await.unwrap());

        let job = request(
            &app,
            Method::GET,
            &format!("/v1/jobs/{job_id}"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(job.status(), StatusCode::OK);
        let job: Value = response_json(job).await;
        assert_eq!(job["status"], "succeeded");
        assert_eq!(job["revision"], 1);
        let output_asset_id = job["outputAssetId"].as_str().unwrap();

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
        assert_eq!(metadata["mimeType"], "video/mp4");
        assert_eq!(metadata["presetId"], "export");
        let expected_hash = job["result"]["outputSha256"].as_str().unwrap();
        assert_eq!(metadata["sha256"], expected_hash);

        let output = request(
            &app,
            Method::GET,
            &format!("/v1/assets/{output_asset_id}/content"),
            Some(TEST_TOKEN),
            None,
        )
        .await;
        assert_eq!(output.status(), StatusCode::OK);
        let output = to_bytes(output.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap();
        assert!(!output.is_empty());
        assert_eq!(hex::encode(Sha256::digest(&output)), expected_hash);
    }

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
            "ffprobe".into(),
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
            "ffprobe".into(),
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
        assert_eq!(source_metadata["mimeType"], "video/mp4");
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
        assert_eq!(source_content.headers()[CONTENT_TYPE], "video/mp4");
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
            file_name: Some("fixture.mp4".to_owned()),
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

    fn timeline_for_uploaded_asset(asset_id: &str, sha256: &str) -> Value {
        json!({
            "schemaVersion": "1.0.0",
            "projectId": PROJECT_ID,
            "revision": 0,
            "canvas": {
                "width": 320,
                "height": 240,
                "fps": {"num": 24, "den": 1},
                "background": "#101820"
            },
            "assets": {
                (asset_id): {
                    "kind": "video",
                    "uri": format!("asset://{asset_id}"),
                    "sha256": sha256
                }
            },
            "tracks": [{
                "id": "video-main",
                "kind": "video",
                "order": 0,
                "clips": [{
                    "id": "uploaded-clip",
                    "kind": "video",
                    "assetId": asset_id,
                    "startFrame": 0,
                    "durationFrames": 24,
                    "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 24},
                    "sourceOut": {"ticks": 24, "timebase_num": 1, "timebase_den": 24}
                }]
            }]
        })
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

    fn configured_tool(variable: &str, fallback: &str) -> Option<PathBuf> {
        let path = std::env::var_os(variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(fallback));
        Command::new(&path)
            .arg("-version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|_| path)
    }

    fn make_upload_fixture(ffmpeg: &Path, output_path: &Path) {
        let output = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=24:d=1",
                "-vf",
                "noise=alls=100:allf=t+u",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-crf",
                "0",
                "-pix_fmt",
                "yuv420p",
                "-an",
                "-y",
            ])
            .arg(output_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not create media upload fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn make_simple_render_fixture(ffmpeg: &Path, output_path: &Path) {
        let output = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=24:d=1",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(output_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not create render media fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

    async fn raw_request(
        app: &Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: &[u8],
    ) -> Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(CONTENT_TYPE, "application/octet-stream");
        if let Some(token) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.to_vec())).unwrap())
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
