//! MCP `ServerHandler` implementation — `PhoneHandler`.
//!
//! Each MCP tool maps onto a deliberately supported subset of the daemon's
//! agent API. Device-target changes and destructive maintenance stay outside
//! this surface.
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
use serde::{Deserialize, Serialize};

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
    /// Horizontal scroll delta. Positive reveals content to the right.
    pub dx: f64,
    /// Vertical scroll delta. **Positive dy reveals content farther down**;
    /// negative dy reveals content above. Typical magnitude: 30–120.
    pub dy: f64,
}

/// Parameters for [`phone_type`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeParams {
    /// Unicode text to send through the device-side input service. Focus the
    /// intended field and verify it before typing.
    pub text: String,
}

/// Parameters for [`phone_tap_label`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TapLabelParams {
    /// The element's visible accessibility label, exactly as shown by
    /// `phone_elements` (e.g. "新备忘录", "Connect").
    pub label: String,
}

/// Parameters for [`phone_tap_element`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TapElementParams {
    /// Zero-based element index from `phone_elements`.
    pub element: usize,
    /// Snapshot token from the same `phone_elements` response.
    pub snapshot: String,
}

/// Parameters for [`phone_key`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyParams {
    /// Supported names: `return`/`enter`, `escape`, `space`, `tab`,
    /// `delete`/`backspace`, `up`, `down`, `left`, `right`.
    pub name: String,
}

/// Parameters for [`phone_shortcut`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShortcutParams {
    /// Supported names: `home` (Home Screen) and `spotlight` (search).
    /// App Switcher is unsupported by the Direct/WDA backend.
    pub name: String,
}

/// Parameters for [`phone_run_steps`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunStepsParams {
    /// Ordered steps. The daemon validates the whole list before sending the
    /// first action and stops immediately when any action or wait condition
    /// fails.
    pub steps: Vec<PhoneStep>,
}

/// One step in a bounded multi-step Direct/WDA sequence.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PhoneStep {
    /// Tap normalized screen coordinates. Prefer `tap_label` when possible.
    Tap {
        x: f64,
        y: f64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Long-press normalized screen coordinates.
    Longpress {
        x: f64,
        y: f64,
        #[serde(default = "default_phone_longpress_ms")]
        duration_ms: u64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Swipe once from one normalized screen point to another.
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[serde(default = "default_phone_swipe_ms")]
        duration_ms: u64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Hold, then drag once between two normalized screen points.
    Drag {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[serde(default = "default_phone_drag_hold_ms")]
        hold_ms: u64,
        #[serde(default = "default_phone_swipe_ms")]
        duration_ms: u64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Tap one exact, unique accessibility label.
    TapLabel {
        label: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Tap the one current element matching every supplied semantic locator
    /// field. Zero or multiple matches send no tap.
    TapLocator {
        locator: PhoneElementLocator,
        #[serde(default)]
        after_ms: u64,
    },
    /// Type Unicode text into the focused field. `clear=true` clears that field
    /// immediately before inserting the text as one compound action.
    Type {
        text: String,
        #[serde(default)]
        clear: bool,
        #[serde(default)]
        after_ms: u64,
    },
    /// Send one supported device-native key.
    Key {
        name: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Trigger Home or Spotlight.
    Shortcut {
        name: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Scroll with the same normalized anchor and deltas as `phone_scroll`.
    Scroll {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Launch or foreground an installed app by its exact bundle identifier.
    LaunchApp {
        bundle: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Navigate back inside the current application.
    Back {
        #[serde(default)]
        after_ms: u64,
    },
    /// Select a value in a native picker wheel.
    Picker {
        #[serde(default)]
        column: usize,
        value: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Poll the current WDA element tree until the semantic expectation holds.
    WaitFor {
        expect: PhoneUiExpectation,
        #[serde(default = "default_phone_wait_ms")]
        timeout_ms: u64,
        #[serde(default = "default_phone_poll_ms")]
        poll_ms: u64,
    },
    /// A short animation pause. Prefer `wait_for` for correctness.
    Pause { ms: u64 },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneUiExpectation {
    /// Exact foreground Application label, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    /// Every locator in this list must match at least one current element.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub present: Vec<PhoneElementLocator>,
    /// Every locator in this list must match no current element.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent: Vec<PhoneElementLocator>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneElementLocator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}

fn default_phone_wait_ms() -> u64 {
    5_000
}

fn default_phone_longpress_ms() -> u64 {
    600
}

fn default_phone_swipe_ms() -> u64 {
    300
}

fn default_phone_drag_hold_ms() -> u64 {
    500
}

fn default_phone_poll_ms() -> u64 {
    250
}

// ---------------------------------------------------------------------------
// PhoneHandler
// ---------------------------------------------------------------------------

/// MCP server that forwards tool calls to the iphone-use daemon.
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

    #[tool(
        description = "Capture the current iPhone screen through the configured device \
        backend and return it as an image/png content block. Direct/WDA capture is the \
        default and does not require iPhone Mirroring. Capture only when a current \
        user-requested task needs phone pixels; do not capture or reconnect for \
        initialization, health checks, or to keep the phone ready. Idle release is \
        intentional. If that task cannot proceed because Direct is released/offline, \
        check phone_status recovery_owner=daemon and hint/setup_blocked_on. Do not \
        reconnect while releasing/reconnecting or a blocker remains. When appropriate, \
        call phone_reconnect once, then poll phone_status until drivable=true."
    )]
    async fn phone_screenshot(&self) -> CallToolResult {
        match self.daemon.screenshot().await {
            Ok(bytes) if !bytes.is_empty() => {
                let b64 = B64.encode(&bytes);
                CallToolResult::success(vec![Content::image(b64, "image/png")])
            }
            Ok(_) => CallToolResult::error(vec![Content::text(
                "screenshot returned empty data — inspect phone_status device_state, \
                 screen_state, hint, setup_blocked_on, setup_phase, and setup_message",
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
        (1,1) is the bottom-right corner. Prefer phone_elements + \
        phone_tap_element for semantic controls; use phone_screenshot for \
        pixel-only targets.")]
    async fn phone_tap(
        &self,
        Parameters(TapParams { x, y }): Parameters<TapParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Tap { x, y }).await
    }

    // -----------------------------------------------------------------------
    // phone_scroll
    // -----------------------------------------------------------------------

    #[tool(description = "Scroll the iPhone screen with a device-side swipe. \
        x and y are the normalized anchor position (0–1). \
        Positive dx reveals content to the right. Positive dy reveals content farther \
        down; negative dy reveals content above. Example: dy=80 scrolls down roughly \
        one screen-length; dy=-80 scrolls back up.")]
    async fn phone_scroll(
        &self,
        Parameters(ScrollParams { x, y, dx, dy }): Parameters<ScrollParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Scroll { x, y, dx, dy }).await
    }

    // -----------------------------------------------------------------------
    // phone_type
    // -----------------------------------------------------------------------

    #[tool(
        description = "Type Unicode text into the currently focused iPhone field \
        through Direct/WDA. Check phone_status.drivable and verify the focused element \
        before typing; text lands in whichever field currently owns keyboard focus."
    )]
    async fn phone_type(
        &self,
        Parameters(TypeParams { text }): Parameters<TypeParams>,
    ) -> CallToolResult {
        send_input(&self.daemon, &InputMsg::Text { text }).await
    }

    // -----------------------------------------------------------------------
    // phone_key
    // -----------------------------------------------------------------------

    #[tool(description = "Send a device-native named key. Supported names: \
        return/enter, escape, space, tab, delete/backspace, up, down, left, right. \
        Other names return an explicit unsupported error.")]
    async fn phone_key(
        &self,
        Parameters(KeyParams { name }): Parameters<KeyParams>,
    ) -> CallToolResult {
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "return" | "enter" | "escape" | "space" | "tab" | "delete" | "backspace" | "up"
            | "down" | "left" | "right" => send_input(&self.daemon, &InputMsg::Key { name }).await,
            _ => CallToolResult::error(vec![Content::text(format!(
                "unsupported key '{name}'; supported: return/enter, escape, space, tab, \
                 delete/backspace, up, down, left, right"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_shortcut
    // -----------------------------------------------------------------------

    #[tool(description = "Trigger a Direct/WDA-supported iOS system shortcut. \
        Supported names: \
        'home' — go to the iOS Home Screen; \
        'spotlight' — open Spotlight search. \
        'switcher' is explicitly unsupported because WDA cannot synthesize the \
        system App Switcher gesture.")]
    async fn phone_shortcut(
        &self,
        Parameters(ShortcutParams { name }): Parameters<ShortcutParams>,
    ) -> CallToolResult {
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "home" | "spotlight" => send_input(&self.daemon, &InputMsg::Shortcut { name }).await,
            "switcher" => CallToolResult::error(vec![Content::text(
                "unsupported shortcut 'switcher': the Direct/WDA backend cannot open \
                 the iOS App Switcher; use home, spotlight, or launch an app by a \
                 supported device action instead",
            )]),
            _ => CallToolResult::error(vec![Content::text(format!(
                "unsupported shortcut '{name}'; supported: home, spotlight"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_run_steps
    // -----------------------------------------------------------------------

    #[tool(
        description = "Execute a bounded sequence of iPhone actions in ONE MCP \
        call through Direct/WDA. Supported step kinds: tap, longpress, swipe, drag, \
        tap_label, tap_locator, type, key, shortcut, scroll, launch_app, back, picker, \
        wait_for, and pause. The daemon validates \
        the complete sequence before dispatch, holds one WDA control lock, and \
        stops immediately on the first failure. DEFAULT TO THIS TOOL when two or more \
        consecutive actions are already understood, safe, and verifiable; reserve \
        atomic action tools for exploring an unknown screen, waiting for human \
        confirmation, or isolating a failed checkpoint. Use wait_for semantic gates \
        between page transitions; never batch an unverified, changing, or irreversible flow. \
        Start from phone_status(drivable=true) and a recent phone_elements read. \
        The response reports completed/applied counts and the exact failed step; \
        retry_safe=false means DO NOT replay the whole sequence."
    )]
    async fn phone_run_steps(
        &self,
        Parameters(RunStepsParams { steps }): Parameters<RunStepsParams>,
    ) -> CallToolResult {
        let request = match phone_steps_request(steps) {
            Ok(request) => request,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        match self.daemon.actions(&request).await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(error) => CallToolResult::error(vec![Content::text(format!(
                "multi-step sequence failed: {error:#}"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_status
    // -----------------------------------------------------------------------

    #[tool(
        description = "Read Direct/WDA status without taking control. The JSON preserves \
        backend, target_configured, managed_wda, managed_wda_pending, recovery_owner, \
        device_state, screen_state, wda, wda_actionable, locked, drivable, released, \
        hint, setup_blocked_on, setup_phase, and setup_message. Gate actions on drivable=true, not on \
        phone_target. For initialization, status/health checks, or no unfinished \
        user-requested task needing phone access, report the state and stop; do not \
        reconnect or hold the phone. Idle release is intentional. Only if a current \
        user-requested phone operation or screen/UI read cannot proceed because \
        Direct is released/offline, check recovery_owner=daemon and hint/setup_blocked_on. \
        Do not reconnect while releasing/reconnecting or a blocker remains. When \
        appropriate, call phone_reconnect once, then poll until drivable=true; never \
        switch to Mirroring implicitly."
    )]
    async fn phone_status(&self) -> CallToolResult {
        match self.daemon.status().await {
            Ok(s) => {
                let json =
                    serde_json::to_string(&s).unwrap_or_else(|_| r#"{"ok":true}"#.to_string());
                CallToolResult::success(vec![Content::text(json)])
            }
            Err(e) => CallToolResult::error(vec![Content::text(format!("status failed: {e:#}"))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_elements (L2)
    // -----------------------------------------------------------------------

    #[tool(
        description = "Read the iPhone's current UI as a flattened element list \
        (requires Direct/WDA with drivable=true in phone_status). Returns JSON \
        with an ephemeral snapshot plus elements in document order. Rows include \
        kind, label, rect, depth and, when useful, identifier, disabled/hidden \
        state, accessibility/focus state, value, and placeholder. PREFER this over \
        phone_screenshot for reasoning: it is text (an order of magnitude cheaper), \
        carries semantic locator candidates, and does not depend on a Mirroring \
        window. Snapshot indexes are current-read refs only; never persist them \
        in a reusable flow."
    )]
    async fn phone_elements(&self) -> CallToolResult {
        match self.daemon.elements().await {
            Ok(json) => CallToolResult::success(vec![Content::text(json)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "elements failed (is WDA set up? see docs/wda-setup.html): {e:#}"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_tap_element (L2)
    // -----------------------------------------------------------------------

    #[tool(
        description = "Tap one element by its zero-based index and snapshot token \
        from the SAME phone_elements response. The daemon re-reads the tree and \
        refuses the action if the UI changed, so a stale element reference cannot \
        silently tap a different control. Use identifier/kind/label/state fields to \
        choose the index. Snapshot refs are ephemeral: never persist them in a flow."
    )]
    async fn phone_tap_element(
        &self,
        Parameters(TapElementParams { element, snapshot }): Parameters<TapElementParams>,
    ) -> CallToolResult {
        match self.daemon.tap_element(element, &snapshot).await {
            Ok(()) => CallToolResult::success(vec![Content::text(format!(
                "tapped element #{element} from the supplied snapshot"
            ))]),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "tap_element #{element} failed: {e:#}"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_tap_label (L2)
    // -----------------------------------------------------------------------

    #[tool(description = "Tap an iPhone UI element by an EXACT visible label \
        (requires Direct/WDA with drivable=true in phone_status). Reads a fresh \
        phone_elements snapshot, requires exactly one match, then performs a \
        snapshot-bound tap. Zero or multiple matches return an error and send NO \
        action. For duplicate labels, choose by identifier/kind/state from \
        phone_elements and call phone_tap_element with that response's snapshot.")]
    async fn phone_tap_label(
        &self,
        Parameters(TapLabelParams { label }): Parameters<TapLabelParams>,
    ) -> CallToolResult {
        match self.daemon.tap_label(&label).await {
            Ok(()) => {
                CallToolResult::success(vec![Content::text(format!("tapped element: {label}"))])
            }
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "tap_label '{label}' failed: {e:#}"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_reconnect
    // -----------------------------------------------------------------------

    #[tool(
        description = "Start/restart on-device automation for the canonical Direct/WDA \
        target. This occupies the phone and may require the operator to unlock it. \
        Use only to continue a current user-requested phone operation or screen/UI \
        read that cannot proceed; never reconnect for initialization, status/health \
        checks, a completed task, or to keep the phone ready. Idle release is intentional. \
        Require phone_status released/offline and recovery_owner=daemon; inspect \
        hint/setup_blocked_on first. Do not reconnect while releasing/reconnecting \
        or a blocker remains. Once these conditions are met, call once, \
        then poll phone_status until reconnecting=false and drivable=true; while it is \
        reconnecting, report setup_phase/setup_message and obey setup_blocked_on. This tool \
        never accepts a UDID and cannot switch devices or fall back to Mirroring. \
        External WDA returns an explicit operator-owned recovery error."
    )]
    async fn phone_reconnect(&self) -> CallToolResult {
        match self.daemon.reconnect().await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("reconnect failed: {e:#}"))])
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
                "iphone-use-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Control an iPhone through the daemon's Direct/WDA backend; iPhone \
                 Mirroring is only an explicit legacy compatibility mode. phone_status() \
                 is read-only. For initialization, status/health checks, or no unfinished \
                 user-requested task needing phone access, report the state and stop; \
                 do not reconnect, hold, or poll screenshots/elements to keep the phone \
                 ready. Idle release is intentional. Before a current user-requested \
                 phone operation or screen/UI read, check phone_status() and require \
                 drivable=true. Prefer phone_elements() \
                 plus phone_tap_element(); phone_tap_label() is safe only when the \
                 exact label is unique. Use phone_screenshot() when pixels matter. \
                 Only if that task cannot proceed because Direct is released/offline, \
                 check recovery_owner=daemon and hint/setup_blocked_on. Do not reconnect \
                 while releasing/reconnecting or a blocker remains. When appropriate, \
                 call phone_reconnect() once, then poll status until drivable=true. \
                 Never fall back to Mirroring implicitly. App Switcher is unsupported.",
            )
    }
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn phone_locator_has_condition(locator: &PhoneElementLocator) -> bool {
    locator.label.is_some()
        || locator.identifier.is_some()
        || locator.kind.is_some()
        || locator.value.is_some()
        || locator.focused.is_some()
        || locator.enabled.is_some()
        || locator.visible.is_some()
}

pub(crate) fn phone_steps_request(steps: Vec<PhoneStep>) -> Result<serde_json::Value, String> {
    const MAX_STEPS: usize = 24;
    const MAX_AFTER_MS: u64 = 3_000;
    const MAX_WAIT_MS: u64 = 10_000;
    const MAX_DECLARED_WAIT_MS: u64 = 60_000;

    if steps.is_empty() {
        return Err("steps must contain at least one step; no action was sent".to_string());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!(
            "steps exceeds the maximum of {MAX_STEPS}; no action was sent"
        ));
    }

    let mut encoded = Vec::with_capacity(steps.len());
    let mut declared_wait_ms = 0_u64;
    for (index, step) in steps.into_iter().enumerate() {
        let mut action_step = |action: serde_json::Value, after_ms: u64| {
            declared_wait_ms = declared_wait_ms.saturating_add(after_ms);
            serde_json::json!({
                "kind": "action",
                "action": action,
                "after_ms": after_ms
            })
        };
        let validate_after = |after_ms: u64| -> Result<(), String> {
            if after_ms > MAX_AFTER_MS {
                Err(format!(
                    "steps[{index}].after_ms exceeds {MAX_AFTER_MS}; no action was sent"
                ))
            } else {
                Ok(())
            }
        };
        let encoded_step = match step {
            PhoneStep::Tap { x, y, after_ms } => {
                validate_after(after_ms)?;
                if !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                {
                    return Err(format!(
                        "steps[{index}] tap coordinates must be finite values from 0 to 1; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"tap","x":x,"y":y}), after_ms)
            }
            PhoneStep::Longpress {
                x,
                y,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                    || !(1..=10_000).contains(&duration_ms)
                {
                    return Err(format!(
                        "steps[{index}] longpress needs coordinates from 0 to 1 and duration_ms from 1 to 10000; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({
                        "type":"longpress",
                        "x":x,
                        "y":y,
                        "duration_ms":duration_ms
                    }),
                    after_ms,
                )
            }
            PhoneStep::Swipe {
                x1,
                y1,
                x2,
                y2,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if ![x1, y1, x2, y2].into_iter().all(f64::is_finite)
                    || ![x1, y1, x2, y2]
                        .into_iter()
                        .all(|value| (0.0..=1.0).contains(&value))
                    || !(1..=10_000).contains(&duration_ms)
                    || (x1 == x2 && y1 == y2)
                {
                    return Err(format!(
                        "steps[{index}] swipe needs distinct coordinates from 0 to 1 and duration_ms from 1 to 10000; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({
                        "type":"swipe",
                        "x1":x1,
                        "y1":y1,
                        "x2":x2,
                        "y2":y2,
                        "duration_ms":duration_ms
                    }),
                    after_ms,
                )
            }
            PhoneStep::Drag {
                x1,
                y1,
                x2,
                y2,
                hold_ms,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if ![x1, y1, x2, y2].into_iter().all(f64::is_finite)
                    || ![x1, y1, x2, y2]
                        .into_iter()
                        .all(|value| (0.0..=1.0).contains(&value))
                    || hold_ms > 10_000
                    || !(1..=10_000).contains(&duration_ms)
                    || (x1 == x2 && y1 == y2)
                {
                    return Err(format!(
                        "steps[{index}] drag needs distinct coordinates from 0 to 1, hold_ms at most 10000, and duration_ms from 1 to 10000; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({
                        "type":"drag",
                        "x1":x1,
                        "y1":y1,
                        "x2":x2,
                        "y2":y2,
                        "hold_ms":hold_ms,
                        "duration_ms":duration_ms
                    }),
                    after_ms,
                )
            }
            PhoneStep::TapLabel { label, after_ms } => {
                validate_after(after_ms)?;
                if label.trim().is_empty() || label.chars().count() > 500 {
                    return Err(format!(
                        "steps[{index}].label must contain 1 to 500 characters; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"tap","label":label}), after_ms)
            }
            PhoneStep::TapLocator { locator, after_ms } => {
                validate_after(after_ms)?;
                if !phone_locator_has_condition(&locator) {
                    return Err(format!(
                        "steps[{index}].locator must include at least one condition; no action was sent"
                    ));
                }
                let locator = serde_json::to_value(locator).map_err(|error| {
                    format!(
                        "steps[{index}] locator serialization failed: {error}; no action was sent"
                    )
                })?;
                action_step(
                    serde_json::json!({"type":"tap_locator","locator":locator}),
                    after_ms,
                )
            }
            PhoneStep::Type {
                text,
                clear,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if text.chars().count() > 1_000 {
                    return Err(format!(
                        "steps[{index}].text exceeds 1000 characters; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({"type":"text","text":text,"clear":clear}),
                    after_ms,
                )
            }
            PhoneStep::Key { name, after_ms } => {
                validate_after(after_ms)?;
                let name = name.trim().to_ascii_lowercase();
                if !matches!(
                    name.as_str(),
                    "return"
                        | "enter"
                        | "escape"
                        | "space"
                        | "tab"
                        | "delete"
                        | "backspace"
                        | "up"
                        | "down"
                        | "left"
                        | "right"
                        | "dismiss"
                        | "hide"
                ) {
                    return Err(format!(
                        "steps[{index}] has unsupported key {name:?}; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"key","name":name}), after_ms)
            }
            PhoneStep::Shortcut { name, after_ms } => {
                validate_after(after_ms)?;
                let name = name.trim().to_ascii_lowercase();
                if !matches!(name.as_str(), "home" | "spotlight") {
                    return Err(format!(
                        "steps[{index}] has unsupported shortcut {name:?}; supported: home, spotlight; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"shortcut","name":name}), after_ms)
            }
            PhoneStep::Scroll {
                x,
                y,
                dx,
                dy,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if ![x, y, dx, dy].into_iter().all(f64::is_finite)
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                    || dx.abs() > 1_000.0
                    || dy.abs() > 1_000.0
                    || (dx == 0.0 && dy == 0.0)
                {
                    return Err(format!(
                        "steps[{index}] has invalid scroll geometry; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({"type":"scroll","x":x,"y":y,"dx":dx,"dy":dy}),
                    after_ms,
                )
            }
            PhoneStep::LaunchApp { bundle, after_ms } => {
                validate_after(after_ms)?;
                if bundle.is_empty()
                    || bundle.len() > 200
                    || !bundle.chars().all(|character| {
                        character.is_ascii_alphanumeric() || character == '.' || character == '-'
                    })
                {
                    return Err(format!(
                        "steps[{index}].bundle must be a valid reverse-DNS identifier up to 200 bytes; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({"type":"launch_app","bundle":bundle}),
                    after_ms,
                )
            }
            PhoneStep::Back { after_ms } => {
                validate_after(after_ms)?;
                action_step(serde_json::json!({"type":"back"}), after_ms)
            }
            PhoneStep::Picker {
                column,
                value,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if column > 20 || value.trim().is_empty() || value.chars().count() > 500 {
                    return Err(format!(
                        "steps[{index}] picker needs column 0..20 and a non-empty value up to 500 characters; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({"type":"picker","column":column,"value":value}),
                    after_ms,
                )
            }
            PhoneStep::WaitFor {
                expect,
                timeout_ms,
                poll_ms,
            } => {
                if expect.application.is_none()
                    && expect.present.is_empty()
                    && expect.absent.is_empty()
                {
                    return Err(format!(
                        "steps[{index}].expect must include application, present, or absent; no action was sent"
                    ));
                }
                if expect
                    .application
                    .as_ref()
                    .is_some_and(|application| application.is_empty())
                {
                    return Err(format!(
                        "steps[{index}].expect.application must not be empty; no action was sent"
                    ));
                }
                if expect
                    .present
                    .iter()
                    .chain(expect.absent.iter())
                    .any(|locator| !phone_locator_has_condition(locator))
                {
                    return Err(format!(
                        "steps[{index}] contains an empty element locator; no action was sent"
                    ));
                }
                if timeout_ms == 0 || timeout_ms > MAX_WAIT_MS {
                    return Err(format!(
                        "steps[{index}].timeout_ms must be between 1 and {MAX_WAIT_MS}; no action was sent"
                    ));
                }
                if !(50..=1_000).contains(&poll_ms) {
                    return Err(format!(
                        "steps[{index}].poll_ms must be between 50 and 1000; no action was sent"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(timeout_ms);
                serde_json::json!({
                    "kind": "wait_for",
                    "expect": expect,
                    "timeout_ms": timeout_ms,
                    "poll_ms": poll_ms
                })
            }
            PhoneStep::Pause { ms } => {
                if ms == 0 || ms > MAX_AFTER_MS {
                    return Err(format!(
                        "steps[{index}].ms must be between 1 and {MAX_AFTER_MS}; no action was sent"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(ms);
                serde_json::json!({"kind":"pause","ms":ms})
            }
        };
        encoded.push(encoded_step);
    }
    if declared_wait_ms > MAX_DECLARED_WAIT_MS {
        return Err(format!(
            "declared waits exceed the batch maximum of {MAX_DECLARED_WAIT_MS}ms; no action was sent"
        ));
    }
    Ok(serde_json::json!({"steps": encoded}))
}

/// Send a single input event and map daemon errors to MCP tool errors.
async fn send_input(daemon: &DaemonClient, msg: &InputMsg) -> CallToolResult {
    match daemon.input(msg).await {
        Ok(()) => CallToolResult::success(vec![Content::text("ok")]),
        Err(e) => CallToolResult::error(vec![Content::text(format!("input failed: {e:#}"))]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_step_request_encodes_actions_and_semantic_waits() {
        let request = phone_steps_request(vec![
            PhoneStep::Shortcut {
                name: "home".to_string(),
                after_ms: 300,
            },
            PhoneStep::TapLabel {
                label: "搜索".to_string(),
                after_ms: 0,
            },
            PhoneStep::TapLocator {
                locator: PhoneElementLocator {
                    label: None,
                    identifier: Some("search-field".to_string()),
                    kind: Some("TextField".to_string()),
                    value: None,
                    focused: Some(true),
                    enabled: Some(true),
                    visible: Some(true),
                },
                after_ms: 0,
            },
            PhoneStep::WaitFor {
                expect: PhoneUiExpectation {
                    application: Some("聚焦".to_string()),
                    present: vec![PhoneElementLocator {
                        label: Some("搜索".to_string()),
                        identifier: None,
                        kind: Some("TextField".to_string()),
                        value: None,
                        focused: Some(true),
                        enabled: None,
                        visible: None,
                    }],
                    absent: vec![],
                },
                timeout_ms: 2_000,
                poll_ms: 100,
            },
        ])
        .unwrap();

        assert_eq!(request["steps"][0]["kind"], "action");
        assert_eq!(request["steps"][0]["action"]["type"], "shortcut");
        assert_eq!(request["steps"][1]["action"]["type"], "tap");
        assert_eq!(request["steps"][1]["action"]["label"], "搜索");
        assert_eq!(request["steps"][2]["action"]["type"], "tap_locator");
        assert_eq!(
            request["steps"][2]["action"]["locator"]["identifier"],
            "search-field"
        );
        assert_eq!(request["steps"][3]["kind"], "wait_for");
        assert_eq!(request["steps"][3]["expect"]["application"], "聚焦");
        assert_eq!(request["steps"][3]["expect"]["present"][0]["focused"], true);
    }

    #[test]
    fn multi_step_request_encodes_recordable_gestures() {
        let request = phone_steps_request(vec![
            PhoneStep::Longpress {
                x: 0.4,
                y: 0.5,
                duration_ms: 650,
                after_ms: 100,
            },
            PhoneStep::Swipe {
                x1: 0.5,
                y1: 0.8,
                x2: 0.5,
                y2: 0.2,
                duration_ms: 320,
                after_ms: 0,
            },
            PhoneStep::Drag {
                x1: 0.2,
                y1: 0.5,
                x2: 0.8,
                y2: 0.5,
                hold_ms: 500,
                duration_ms: 400,
                after_ms: 0,
            },
        ])
        .unwrap();

        assert_eq!(request["steps"][0]["action"]["type"], "longpress");
        assert_eq!(request["steps"][0]["action"]["duration_ms"], 650);
        assert_eq!(request["steps"][1]["action"]["type"], "swipe");
        assert_eq!(request["steps"][1]["action"]["y2"], 0.2);
        assert_eq!(request["steps"][2]["action"]["type"], "drag");
        assert_eq!(request["steps"][2]["action"]["hold_ms"], 500);
    }

    #[test]
    fn multi_step_request_rejects_every_invalid_step_before_sending() {
        let error = phone_steps_request(vec![
            PhoneStep::TapLabel {
                label: "搜索".to_string(),
                after_ms: 0,
            },
            PhoneStep::Shortcut {
                name: "switcher".to_string(),
                after_ms: 0,
            },
        ])
        .unwrap_err();
        assert!(error.contains("steps[1]"));
        assert!(error.contains("no action was sent"));
    }

    #[test]
    fn multi_step_launch_app_requires_a_valid_bundle_identifier() {
        let request = phone_steps_request(vec![PhoneStep::LaunchApp {
            bundle: "com.example.SampleApp".to_string(),
            after_ms: 500,
        }])
        .unwrap();
        assert_eq!(request["steps"][0]["action"]["type"], "launch_app");
        assert_eq!(
            request["steps"][0]["action"]["bundle"],
            "com.example.SampleApp"
        );

        let error = phone_steps_request(vec![PhoneStep::LaunchApp {
            bundle: "not a bundle".to_string(),
            after_ms: 0,
        }])
        .unwrap_err();
        assert!(error.contains("reverse-DNS"));
        assert!(error.contains("no action was sent"));
    }

    #[test]
    fn multi_step_request_rejects_invalid_waits_and_empty_locators_offline() {
        let invalid_timeout = phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: Some("设置".to_string()),
                present: vec![],
                absent: vec![],
            },
            timeout_ms: 10_001,
            poll_ms: 100,
        }])
        .unwrap_err();
        assert!(invalid_timeout.contains("timeout_ms"));
        assert!(invalid_timeout.contains("no action was sent"));

        let empty_locator = phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: None,
                present: vec![PhoneElementLocator {
                    label: None,
                    identifier: None,
                    kind: None,
                    value: None,
                    focused: None,
                    enabled: None,
                    visible: None,
                }],
                absent: vec![],
            },
            timeout_ms: 1_000,
            poll_ms: 100,
        }])
        .unwrap_err();
        assert!(empty_locator.contains("empty element locator"));
    }

    #[test]
    fn multi_step_request_rejects_excessive_total_declared_wait_offline() {
        let steps = (0..7)
            .map(|_| PhoneStep::WaitFor {
                expect: PhoneUiExpectation {
                    application: Some("设置".to_string()),
                    present: vec![],
                    absent: vec![],
                },
                timeout_ms: 10_000,
                poll_ms: 100,
            })
            .collect();
        let error = phone_steps_request(steps).unwrap_err();
        assert!(error.contains("batch maximum of 60000ms"));
        assert!(error.contains("no action was sent"));
    }
}
