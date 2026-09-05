//! `iphone-use-mcp` — MCP stdio server wrapping the iphone-use daemon's
//! agent HTTP API.
//!
//! # Usage
//!
//! ```
//! PHONE_REMOTE_URL=http://192.168.1.x:44321 \
//! PHONE_REMOTE_TOKEN=your-password \
//!   iphone-use-mcp
//! ```
//!
//! The process speaks the Model Context Protocol over stdin/stdout.  Add it to
//! your MCP client (Claude Desktop, Claude Code, etc.) as a stdio server — see
//! `crates/mcp/README.md` for the exact config snippet.

use clap::{Parser, Subcommand};
use rmcp::{transport::stdio, ServiceExt};
use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod client;
mod contrib;
mod flow;
mod registry;
mod server;
mod types;

#[derive(Debug, Parser)]
#[command(
    name = "iphone-use-mcp",
    about = "MCP bridge and deterministic flow runner for iphone-use"
)]
struct Cli {
    /// Free-form identity tag shown in `ps` output (issue #46). The server
    /// ignores the value; passing e.g. `--label my-project` from your MCP
    /// client config makes each of many otherwise-identical resident
    /// `iphone-use-mcp` processes attributable to its session/project.
    #[arg(long, value_name = "TAG")]
    label: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate or run a saved, versioned multi-step flow.
    Flow {
        #[command(subcommand)]
        command: FlowCommand,
    },
}

#[derive(Debug, Subcommand)]
enum FlowCommand {
    /// Validate a flow file or installed registry flow offline (no daemon, no phone).
    Validate {
        /// JSON flow file, or a registry id such as `health/export-all`.
        target: String,
    },
    /// Run a flow once; never retries an unknown or failed result.
    Run {
        /// JSON flow file, or a registry id such as `health/export-all`.
        target: String,
        /// Ephemeral flow input in KEY=VALUE form. Repeat for multiple inputs.
        /// Values are used for this run only and are never written to the flow.
        #[arg(long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
        /// Required for flows declared `risk: side_effect` (send/publish/pay/delete).
        #[arg(long)]
        confirm: bool,
    },
    /// Mirror the official flow registry into the local store (~/.iphone-use/flows).
    Update,
    /// List installed flows.
    List {
        /// Only flows in this category (e.g. health, system, finance, im).
        #[arg(long)]
        category: Option<String>,
        /// Only flows for this app directory (e.g. health) or bundle id.
        #[arg(long)]
        app: Option<String>,
        /// Only flows with at least one recorded hardware verification.
        #[arg(long)]
        verified: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Show one flow's metadata, inputs, and step templates.
    Info {
        /// Registry id such as `health/export-all`, or a flow file.
        target: String,
    },
    /// Install a local flow file into the store under a registry id (survives `update`).
    Add {
        /// JSON flow file to install.
        file: PathBuf,
        /// Registry id to install it as, e.g. `myapp/daily-check`.
        #[arg(long = "as", value_name = "APP/FLOW")]
        id: String,
    },
    /// Remove an installed flow (an official flow returns on the next `update`).
    Remove {
        /// Registry id such as `myapp/daily-check`.
        id: String,
    },
    /// Show the official source, any override, and the local store path.
    Sources,
    /// Open a pull request adding a validated flow to the official registry (uses `gh`).
    Publish {
        /// Flow file, or an id installed with `flow add`.
        source: String,
        /// Registry id to publish as, e.g. `health/export-all-zh-cn`.
        #[arg(long = "as", value_name = "APP/FLOW")]
        id: String,
        /// Human app name; only used when the app is new to the registry.
        #[arg(long)]
        app_name: Option<String>,
        /// Foreground-app label per language (repeatable), e.g. --alias Health --alias 健康.
        #[arg(long = "alias", value_name = "LABEL")]
        aliases: Vec<String>,
        /// What you verified and where; goes into the PR body.
        #[arg(long)]
        note: Option<String>,
        /// Open as a draft PR.
        #[arg(long)]
        draft: bool,
    },
    /// File an issue on the official registry for a flow that failed (uses `gh`).
    Report {
        /// Registry id that failed.
        id: String,
        /// JSON result printed by the failing `flow run` (inline, or @path to a file).
        #[arg(long, value_name = "JSON|@FILE")]
        result: Option<String>,
        /// What you expected vs what the phone showed.
        #[arg(long)]
        note: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Log to stderr so it does not interfere with the MCP stdio protocol on
    // stdout/stdin.  MCP clients typically capture stderr for diagnostics.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    if let Some(Command::Flow { command }) = cli.command {
        return match command {
            FlowCommand::Validate { target } => flow::validate_command(&target),
            FlowCommand::Run {
                target,
                inputs,
                confirm,
            } => flow::run_command(&target, &inputs, confirm).await,
            FlowCommand::Update => {
                let report = registry::update().await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                Ok(())
            }
            FlowCommand::List {
                category,
                app,
                verified,
                json,
            } => {
                let filter = registry::ListFilter {
                    category,
                    app,
                    verified_only: verified,
                };
                let (entries, index) = registry::list(&filter)?;
                if json {
                    println!("{}", registry::list_json(&entries, &index));
                } else {
                    print!("{}", registry::list_text(&entries, &index));
                }
                Ok(())
            }
            FlowCommand::Info { target } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&registry::info(&target)?)?
                );
                Ok(())
            }
            FlowCommand::Add { file, id } => {
                println!("{}", registry::add(&file, &id)?);
                Ok(())
            }
            FlowCommand::Remove { id } => {
                println!("{}", registry::remove(&id)?);
                Ok(())
            }
            FlowCommand::Sources => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&registry::sources_json()?)?
                );
                Ok(())
            }
            FlowCommand::Publish {
                source,
                id,
                app_name,
                aliases,
                note,
                draft,
            } => {
                let path = contrib::publish_source(&source)?;
                let options = contrib::PublishOptions {
                    id,
                    app_name,
                    aliases,
                    note,
                    draft,
                };
                let report = tokio::task::spawn_blocking(move || contrib::publish(&path, &options))
                    .await??;
                println!("{}", serde_json::to_string_pretty(&report)?);
                Ok(())
            }
            FlowCommand::Report { id, result, note } => {
                let result = match result {
                    Some(text) if text.starts_with('@') => Some(serde_json::from_slice(
                        &std::fs::read(&text[1..])
                            .map_err(|e| anyhow::anyhow!("read {}: {e}", &text[1..]))?,
                    )?),
                    Some(text) => Some(serde_json::from_str(&text)?),
                    None => None,
                };
                if result.is_none() && note.as_deref().is_none_or(|n| n.trim().is_empty()) {
                    anyhow::bail!("pass --result (the failing `flow run` JSON) or --note");
                }
                let status = client::DaemonClient::from_env()
                    .status()
                    .await
                    .ok()
                    .and_then(|s| serde_json::to_value(s).ok());
                let context = contrib::ReportContext {
                    id,
                    result,
                    status,
                    application: None,
                    note,
                };
                let outcome =
                    tokio::task::spawn_blocking(move || contrib::report(&context)).await??;
                println!("{}", serde_json::to_string_pretty(&outcome)?);
                Ok(())
            }
        };
    }

    // Build the daemon client from env.
    let daemon = client::DaemonClient::from_env();
    tracing::info!(
        url = %daemon.base_url(),
        label = cli.label.as_deref().unwrap_or(""),
        "iphone-use-mcp starting"
    );

    // Run until the MCP client closes the pipe.
    let handler = server::PhoneHandler::new(daemon);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;

    tracing::info!("iphone-use-mcp exiting");
    Ok(())
}
