use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::{EditOperation, EditResult, ProjectRevision, ProjectStore, ProjectSummary, Workspace};

const MAX_API_BODY_BYTES: usize = 2 * 1024 * 1024;
const MIN_BEARER_TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone)]
pub struct ApiServerOptions {
    pub database: PathBuf,
    pub workspace: PathBuf,
    pub bind: SocketAddr,
    pub bearer_token: String,
}

#[derive(Clone)]
struct ApiState {
    store: ProjectStore,
    workspace: PathBuf,
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
    validate_bearer_token(&options.bearer_token)?;
    let workspace = Workspace::open(&options.workspace)?;
    let store = ProjectStore::open(&options.database)?;
    let app = build_router(store, workspace.root().to_path_buf(), options.bearer_token);
    let listener = TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("could not bind local API to {}", options.bind))?;
    let address = listener.local_addr()?;
    eprintln!("Avida local API listening on http://{address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("local API server failed")
}

fn validate_bearer_token(token: &str) -> Result<()> {
    if token.len() < MIN_BEARER_TOKEN_BYTES || token.chars().any(char::is_control) {
        bail!("STUDIO_API_TOKEN must contain at least 32 bytes and no control characters");
    }
    Ok(())
}

fn build_router(store: ProjectStore, workspace: PathBuf, bearer_token: String) -> Router {
    let state = ApiState { store, workspace };
    Router::new()
        .route("/v1/projects", post(create_project))
        .route("/v1/projects/{project_id}", get(get_project))
        .route("/v1/projects/{project_id}/timeline", get(get_timeline))
        .route(
            "/v1/projects/{project_id}/operations",
            post(apply_operations),
        )
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

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
        })
        .await
        .unwrap_err();
        assert!(format!("{non_loopback:#}").contains("loopback"));

        let short_token = serve_api(ApiServerOptions {
            database: root.path().join("projects.sqlite"),
            workspace: root.path().join("workspace"),
            bind: "127.0.0.1:8787".parse().unwrap(),
            bearer_token: "short".to_owned(),
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
