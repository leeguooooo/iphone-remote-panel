//! MCP `ServerHandler` implementation — `PhoneHandler`.
//!
//! Each MCP tool maps 1-to-1 onto one of the daemon's agent API calls.
//!
//! Pattern:
//!   1. `#[tool_router]` on the `impl PhoneHandler` block generates the static
//!      `PhoneHandler::tool_router()` fn.
//!   2. `#[tool_handler]` on the `impl ServerHandler` block fills in
//!      `call_tool`, `list_tools`, and `get_tool`.  We add `get_info` manually
//!      inside the same block so the macro skips its default stub.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, Content, Implementation, InitializeResult, ProtocolVersion,
        ServerCapabilities,
    },
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{client::DaemonClient, types::InputMsg};

// ---------------------------------------------------------------------------
// Parameter types (one struct per tool that takes arguments)
// ---------------------------------------------------------------------------

/// Parameters for [`phone_tap`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TapParams {
    /// Horizontal position, normalized 0–1 (0 = left edge, 1 = right edge).
    pub x: f64,
    /// Vertical position, normalized 0–1 (0 = top edge, 1 = bottom edge).
    pub y: f64,
}

/// Parameters for [`phone_scroll`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrollParams {
    /// Horizontal anchor position, normalized 0–1.
    pub x: f64,
    /// Vertical anchor position, normalized 0–1.
    pub y: f64,
    /// Horizontal scroll delta in CSS pixels.  Positive = scroll right.
    pub dx: f64,
    /// Vertical scroll delta in CSS pixels.
    /// **Negative dy scrolls content up** (the view moves down — iOS natural
    /// scroll).  Typical values: −30 to −120 for a swipe.
    pub dy: f64,
}

/// Parameters for [`phone_type`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeParams {
    /// US-ASCII text to type.  The daemon injects one CGEvent per character.
    /// Non-ASCII / CJK must be entered via the on-phone software keyboard IME.
    pub text: String,
}

/// Parameters for [`phone_key`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyParams {
    /// Key name.  Valid values: `return`, `escape`, `space`, `tab`, `delete`,
    /// `up`, `down`, `left`, `right`.
    pub name: String,
}

/// Parameters for [`phone_shortcut`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShortcutParams {
    /// Shortcut name.  Valid values: `home` (Home Screen), `spotlight`
    /// (Spotlight search), `switcher` (App Switcher).
    pub name: String,
}

// ---------------------------------------------------------------------------
// PhoneHandler
// ---------------------------------------------------------------------------

/// MCP server that forwards tool calls to the iphone-remote daemon.
#[derive(Clone)]
pub struct PhoneHandler {
    daemon: DaemonClient,
}

impl PhoneHandler {
    pub fn new(daemon: DaemonClient) -> Self {
        Self { daemon }
    }
}

// `#[tool_router]` (no `server_handler` flag) generates the static
// `PhoneHandler::tool_router()` fn used by `#[tool_handler]` below.
#[tool_router]
impl PhoneHandler {
    // -----------------------------------------------------------------------
    // phone_screenshot
    // -----------------------------------------------------------------------

    #[tool(description = "Capture the current iPhone screen and return it as a PNG image. \
        The phone must have an active iPhone Mirroring session on the host Mac. \
        Returns an image/png content block. Fails if Mirroring is not running.")]
    async fn phone_screenshot(&self) -> CallToolResult {
        match self.daemon.screenshot().await {
            Ok(bytes) if !bytes.is_empty() => {
                let b64 = B64.encode(&bytes);
                CallToolResult::success(vec![Content::image(b64, "image/png")])
            }
            Ok(_) => CallToolResult::error(vec![Content::text(
                "screenshot returned empty data — is iPhone Mirroring running?",
            )]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("screenshot failed: {e:#}"))])
            }
        }
    }

    // -----------------------------------------------------------------------
    // phone_tap
    // -----------------------------------------------------------------------

    #[tool(description = "Tap the iPhone screen at a normalized position. \
        x and y are in the range 0–1 where (0,0) is the top-left corner and \
        (1,1) is the bottom-right corner. Use phone_screenshot first to identify \
        tap targets.")]
    async fn phone_tap(
        &self,
        Parameters(TapParams { x, y }): Parameters<TapParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Tap { x, y }).await
    }

    // -----------------------------------------------------------------------
    // phone_scroll
    // -----------------------------------------------------------------------

    #[tool(description = "Scroll the iPhone screen with a scroll-wheel gesture. \
        x and y are the normalized anchor position (0–1). \
        dx is the horizontal delta in CSS pixels (positive = scroll right). \
        dy is the vertical delta — NEGATIVE dy scrolls content UP \
        (the view moves down, iOS natural scroll). \
        Example: dy=-80 scrolls down one screen-length; dy=80 scrolls back up.")]
    async fn phone_scroll(
        &self,
        Parameters(ScrollParams { x, y, dx, dy }): Parameters<ScrollParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Scroll { x, y, dx, dy }).await
    }

    // -----------------------------------------------------------------------
    // phone_type
    // -----------------------------------------------------------------------

    #[tool(description = "Type US-ASCII text on the iPhone. The daemon injects one \
        CGEvent per character via the macOS input pipeline. \
        CAVEAT: Only printable US-ASCII characters are reliably supported. \
        CJK and other non-ASCII characters must be entered via the on-phone \
        software keyboard IME — this tool cannot type them directly.")]
    async fn phone_type(
        &self,
        Parameters(TypeParams { text }): Parameters<TypeParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Text { text }).await
    }

    // -----------------------------------------------------------------------
    // phone_key
    // -----------------------------------------------------------------------

    #[tool(description = "Send a named key press to the iPhone. \
        Valid key names: return, escape, space, tab, delete, up, down, left, right.")]
    async fn phone_key(
        &self,
        Parameters(KeyParams { name }): Parameters<KeyParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Key { name }).await
    }

    // -----------------------------------------------------------------------
    // phone_shortcut
    // -----------------------------------------------------------------------

    #[tool(description = "Trigger an iOS system shortcut. \
        Valid shortcut names: \
        'home' — go to the iOS Home Screen; \
        'spotlight' — open Spotlight search; \
        'switcher' — open the App Switcher.")]
    async fn phone_shortcut(
        &self,
        Parameters(ShortcutParams { name }): Parameters<ShortcutParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Shortcut { name }).await
    }

    // -----------------------------------------------------------------------
    // phone_status
    // -----------------------------------------------------------------------

    #[tool(description = "Query the iphone-remote daemon status. \
        Returns a JSON object: \
        'ok' (bool) — daemon is reachable; \
        'phone_target' (bool) — an iPhone Mirroring window is currently on-screen. \
        Use this before other tools to confirm the daemon is running and the phone \
        is mirrored.")]
    async fn phone_status(&self) -> CallToolResult {
        match self.daemon.status().await {
            Ok(s) => {
                let json = serde_json::to_string(&s)
                    .unwrap_or_else(|_| r#"{"ok":true}"#.to_string());
                CallToolResult::success(vec![Content::text(json)])
            }
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("status failed: {e:#}"))])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl
//
// `#[tool_handler]` fills in call_tool / list_tools / get_tool automatically.
// We provide get_info() ourselves so the macro skips its default stub.
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for PhoneHandler {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_server_info(Implementation::new(
                "iphone-remote-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Control an iPhone via iPhone Mirroring on a Mac. \
                 Start with phone_status() to verify the daemon is running and \
                 phone_target is true, then use phone_screenshot() to see the \
                 current screen before sending taps, text, or shortcuts.",
            )
    }
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

/// Send a single input event and map daemon errors to MCP tool errors.
async fn send_input(daemon: &DaemonClient, msg: &InputMsg) -> CallToolResult {
    match daemon.input(msg).await {
        Ok(()) => CallToolResult::success(vec![Content::text("ok")]),
        Err(e) => CallToolResult::error(vec![Content::text(format!("input failed: {e:#}"))]),
    }
}
