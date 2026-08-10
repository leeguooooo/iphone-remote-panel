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
mod flow;
mod server;
mod types;

#[derive(Debug, Parser)]
#[command(
    name = "iphone-use-mcp",
    about = "MCP bridge and deterministic flow runner for iphone-use"
)]
struct Cli {
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
    /// Validate a flow file offline without contacting the daemon or phone.
    Validate {
        /// JSON flow file to validate.
        file: PathBuf,
    },
    /// Run a validated flow once; never retries an unknown or failed result.
    Run {
        /// JSON flow file to execute.
        file: PathBuf,
        /// Ephemeral flow input in KEY=VALUE form. Repeat for multiple inputs.
        /// Values are used for this run only and are never written to the flow.
        #[arg(long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
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
            FlowCommand::Validate { file } => flow::validate_command(&file),
            FlowCommand::Run { file, inputs } => flow::run_command(&file, &inputs).await,
        };
    }

    // Build the daemon client from env.
    let daemon = client::DaemonClient::from_env();
    tracing::info!(url = %daemon.base_url(), "iphone-use-mcp starting");

    // Run until the MCP client closes the pipe.
    let handler = server::PhoneHandler::new(daemon);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;

    tracing::info!("iphone-use-mcp exiting");
    Ok(())
}
