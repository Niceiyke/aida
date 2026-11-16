pub mod api;
mod assemble;
mod capabilities;
mod error;
mod inspect;
mod jobs;
pub mod mcp;
mod media;
mod process;
pub mod project;
mod render;
pub mod timeline;
mod workspace;

pub use api::{ApiServerOptions, serve_api};
pub use mcp::{McpOptions, serve_mcp};
pub use media::{AssetKind, ImportOptions, ImportedAsset, MediaProbe, StreamInfo, import_asset};
pub use project::{
    EditOperation, EditRequest, EditResult, ProjectRestoreResult, ProjectRevision,
    ProjectRevisionEntry, ProjectRevisionHistory, ProjectStore, ProjectSummary, RenderJob,
    RenderJobPhase, RenderJobResult, RenderJobStatus, RenderJobSubmission, RenderRevisionOptions,
    read_edit_request, render_revision,
};
pub use render::{RenderOptions, RenderResult, render_timeline};
pub use timeline::ImageMotion;
pub use workspace::Workspace;
