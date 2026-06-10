//! `iphone-remote-mcp` — MCP stdio server wrapping the iphone-remote daemon's
//! agent HTTP API.
//!
//! # Usage
//!
//! ```
//! PHONE_REMOTE_URL=http://192.168.1.x:8787 \
//! PHONE_REMOTE_TOKEN=your-password \
//!   iphone-remote-mcp
//! ```
//!
//! The process speaks the Model Context Protocol over stdin/stdout.  Add it to
//! your MCP client (Claude Desktop, Claude Code, etc.) as a stdio server — see
//! `crates/mcp/README.md` for the exact config snippet.

use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod client;
mod server;
mod types;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Log to stderr so it does not interfere with the MCP stdio protocol on
    // stdout/stdin.  MCP clients typically capture stderr for diagnostics.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    // Build the daemon client from env.
    let daemon = client::DaemonClient::from_env();
    tracing::info!(url = %daemon.base_url(), "iphone-remote-mcp starting");

    // Run until the MCP client closes the pipe.
    let handler = server::PhoneHandler::new(daemon);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;

    tracing::info!("iphone-remote-mcp exiting");
    Ok(())
}
