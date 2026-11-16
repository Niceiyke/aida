use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use studio::{
    ApiServerOptions, ImportOptions, McpOptions, ProjectStore, RenderOptions,
    RenderRevisionOptions, import_asset, read_edit_request, render_revision, render_timeline,
    serve_api, serve_mcp,
};

#[derive(Debug, Parser)]
#[command(name = "studio", version, about = "Local-first headless video editing")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Probe and safely stage a local video, audio, or still-image file in a workspace.
    Import {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "ffprobe")]
        ffprobe: PathBuf,
    },
    /// Validate a timeline JSON and render an MP4 without a GUI.
    Render {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        timeline: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: PathBuf,
        #[arg(long, default_value = "ffprobe")]
        ffprobe: PathBuf,
        /// Required when the timeline contains text clips.
        #[arg(long)]
        font: Option<PathBuf>,
        #[arg(long, default_value_t = 1800)]
        timeout_seconds: u64,
    },
    /// Create a revisioned project from an imported-media timeline.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Apply a typed, atomic edit batch to a project.
    Edit {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long)]
        idempotency_key: String,
        #[arg(long)]
        operations: PathBuf,
    },
    /// Render an immutable revision stored in the project database.
    RenderRevision {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: PathBuf,
        #[arg(long, default_value = "ffprobe")]
        ffprobe: PathBuf,
        #[arg(long)]
        font: Option<PathBuf>,
        #[arg(long, default_value_t = 1800)]
        timeout_seconds: u64,
    },
    /// Run the authenticated loopback REST API.
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Run the agent tools over stdio using the Model Context Protocol.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Store revision 1 from a timeline JSON whose revision is 0.
    Create {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        timeline: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ApiCommand {
    /// Start the local project API (requires STUDIO_API_TOKEN).
    Serve {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: PathBuf,
        #[arg(long, default_value = "ffprobe")]
        ffprobe: PathBuf,
        /// Required when stored timelines contain text clips.
        #[arg(long)]
        font: Option<PathBuf>,
        #[arg(long, default_value_t = 1800)]
        timeout_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Start the local stdio MCP server.
    Serve {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: PathBuf,
        #[arg(long, default_value = "ffprobe")]
        ffprobe: PathBuf,
        /// Required to edit or render projects containing titles.
        #[arg(long)]
        font: Option<PathBuf>,
        #[arg(long, default_value_t = 1800)]
        timeout_seconds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Import {
            workspace,
            file,
            ffprobe,
        } => {
            let asset = import_asset(ImportOptions {
                workspace,
                file,
                ffprobe,
            })?;
            println!("{}", serde_json::to_string_pretty(&asset)?);
        }
        Command::Render {
            workspace,
            timeline,
            output,
            ffmpeg,
            ffprobe,
            font,
            timeout_seconds,
        } => {
            let result = render_timeline(RenderOptions {
                workspace,
                timeline,
                output,
                ffmpeg,
                ffprobe,
                font,
                timeout_seconds,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Project {
            command:
                ProjectCommand::Create {
                    database,
                    workspace,
                    timeline,
                },
        } => {
            let result =
                ProjectStore::open(database)?.create_from_timeline(&workspace, &timeline)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Edit {
            database,
            workspace,
            project_id,
            expected_revision,
            idempotency_key,
            operations,
        } => {
            let request = read_edit_request(&operations)?;
            let result = ProjectStore::open(database)?.apply_edit_batch(
                &workspace,
                &project_id,
                expected_revision,
                &idempotency_key,
                &request.operations,
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::RenderRevision {
            database,
            workspace,
            project_id,
            revision,
            output,
            ffmpeg,
            ffprobe,
            font,
            timeout_seconds,
        } => {
            let result = render_revision(RenderRevisionOptions {
                database,
                workspace,
                project_id,
                revision,
                output,
                ffmpeg,
                ffprobe,
                font,
                timeout_seconds,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Api {
            command:
                ApiCommand::Serve {
                    database,
                    workspace,
                    bind,
                    ffmpeg,
                    ffprobe,
                    font,
                    timeout_seconds,
                },
        } => {
            let bearer_token = std::env::var("STUDIO_API_TOKEN")
                .context("set STUDIO_API_TOKEN to a random value of at least 32 bytes")?;
            serve_api(ApiServerOptions {
                database,
                workspace,
                bind,
                bearer_token,
                ffmpeg,
                ffprobe,
                font,
                timeout_seconds,
            })
            .await?;
        }
        Command::Mcp {
            command:
                McpCommand::Serve {
                    database,
                    workspace,
                    ffmpeg,
                    ffprobe,
                    font,
                    timeout_seconds,
                },
        } => {
            tokio::task::spawn_blocking(move || {
                serve_mcp(McpOptions {
                    database,
                    workspace,
                    ffmpeg,
                    ffprobe,
                    font,
                    timeout_seconds,
                })
            })
            .await??;
        }
    }
    Ok(())
}
