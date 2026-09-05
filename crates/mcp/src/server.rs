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

/// Parameters for [`phone_flow_list`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct FlowListParams {
    /// Only flows in this registry category (e.g. `health`, `system`, `finance`, `im`).
    #[serde(default)]
    pub category: Option<String>,
    /// Only flows for this app directory (e.g. `health`) or bundle id.
    #[serde(default)]
    pub app: Option<String>,
    /// Only flows with at least one recorded hardware verification.
    #[serde(default)]
    pub verified: bool,
}

/// Parameters for [`phone_flow_info`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlowInfoParams {
    /// Registry id such as `health/export-all`.
    pub id: String,
}

/// Parameters for [`phone_flow_run`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlowRunParams {
    /// Registry id such as `health/export-all` (see phone_flow_list).
    pub id: String,
    /// Runtime string inputs declared by the flow. Values are used for this
    /// run only and never persisted. Never pass passwords, codes, or private
    /// message content.
    #[serde(default)]
    pub inputs: std::collections::BTreeMap<String, String>,
    /// Required for flows declared `risk: side_effect`. Set true only after
    /// the user confirmed the target and inputs.
    #[serde(default)]
    pub confirm: bool,
}

/// Parameters for [`phone_flow_publish`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlowPublishParams {
    /// Flow file path, or an id installed with `flow add`.
    pub source: String,
    /// Registry id to publish as, `<app>/<flow>` lowercase slugs.
    pub id: String,
    /// Human app name, used only when the app is new to the registry.
    #[serde(default)]
    pub app_name: Option<String>,
    /// Foreground-app labels per language (e.g. ["Health","健康"]).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// What was verified, where, and anything a reviewer should know.
    #[serde(default)]
    pub note: Option<String>,
    /// Must be true; set only after the user agreed to open a public PR.
    #[serde(default)]
    pub confirm: bool,
}

/// Parameters for [`phone_flow_report`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlowReportParams {
    /// Registry id of the flow that failed.
    pub id: String,
    /// One or two sentences: what you expected, what the phone showed.
    #[serde(default)]
    pub note: Option<String>,
    /// Must be true; set only after the user agreed to open a public issue.
    #[serde(default)]
    pub confirm: bool,
}

// ---------------------------------------------------------------------------
// PhoneHandler
// ---------------------------------------------------------------------------

/// MCP server that forwards tool calls to the iphone-use daemon.
#[derive(Clone)]
pub struct PhoneHandler {
    daemon: DaemonClient,
    /// The most recent failed `phone_flow_run`, kept so `phone_flow_report`
    /// can file an issue with the real failure instead of a retelling.
    last_flow_failure: std::sync::Arc<std::sync::Mutex<Option<crate::contrib::ReportContext>>>,
}

impl PhoneHandler {
    pub fn new(daemon: DaemonClient) -> Self {
        Self {
            daemon,
            last_flow_failure: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn remember_flow_failure(&self, context: crate::contrib::ReportContext) {
        if let Ok(mut slot) = self.last_flow_failure.lock() {
            *slot = Some(context);
        }
    }

    fn take_flow_failure(&self, id: &str) -> Option<crate::contrib::ReportContext> {
        let slot = self.last_flow_failure.lock().ok()?;
        slot.as_ref().filter(|context| context.id == id).cloned()
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
        let step_count = request["steps"].as_array().map_or(0, Vec::len);
        match self.daemon.actions(&request).await {
            Ok(body) => {
                let hint = (step_count >= 3
                    && serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
                        == Some(true))
                .then(|| {
                    serde_json::json!({
                        "hint": format!(
                            "{step_count} steps ran deterministically. If this is a task someone will repeat, \
                             save it as a flow: write these steps to a v1 JSON file (typed text → named input), \
                             `flow validate`, then phone_flow_publish (with the user's go-ahead) so the next run \
                             is one phone_flow_run call. Check phone_flow_list first: it may already exist."
                        )
                    })
                });
                CallToolResult::success(vec![Content::text(crate::registry::attach_hint(
                    body, "registry", hint,
                ))])
            }
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
            Ok(json) => {
                // Bring the registry to the agent: which installed flows fit
                // the app that is on screen right now.
                let hint = crate::registry::elements_hint(&json);
                CallToolResult::success(vec![Content::text(crate::registry::attach_hint(
                    json, "registry", hint,
                ))])
            }
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

    // -----------------------------------------------------------------------
    // phone_flow_list / phone_flow_info / phone_flow_run / phone_flow_update
    // -----------------------------------------------------------------------

    #[tool(
        description = "List installed registry flows: reviewed, deterministic per-app \
        scripts (id like `health/export-all`) that replay a whole task with NO model \
        and NO screenshots. CHECK THIS FIRST before driving an app step by step: if a \
        flow matches the task, call phone_flow_run instead of exploring. Each entry \
        reports name, description, risk (read_only|navigation|side_effect|unknown), \
        verified (has a recorded hardware run), inputs, app, and category. Empty store: \
        call phone_flow_update once. Filters are optional."
    )]
    async fn phone_flow_list(
        &self,
        Parameters(FlowListParams {
            category,
            app,
            verified,
        }): Parameters<FlowListParams>,
    ) -> CallToolResult {
        let filter = crate::registry::ListFilter {
            category,
            app,
            verified_only: verified,
        };
        match crate::registry::list(&filter) {
            Ok((entries, index)) => CallToolResult::success(vec![Content::text(
                crate::registry::list_json(&entries, &index).to_string(),
            )]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("flow list failed: {e:#}"))])
            }
        }
    }

    #[tool(
        description = "Show one installed registry flow: metadata, declared inputs, and \
        its step templates (tap_label/tap_locator/wait_for/... with input placeholders, \
        never values). Use it to check preconditions (which app, which locale, verified \
        or not) and the exact side effects before phone_flow_run."
    )]
    async fn phone_flow_info(
        &self,
        Parameters(FlowInfoParams { id }): Parameters<FlowInfoParams>,
    ) -> CallToolResult {
        if !crate::registry::valid_flow_id(&id) {
            return CallToolResult::error(vec![Content::text(format!(
                "{id:?} is not a registry id; expected <app>/<flow> lowercase slugs"
            ))]);
        }
        match crate::registry::info(&id) {
            Ok(detail) => CallToolResult::success(vec![Content::text(detail.to_string())]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("flow info failed: {e:#}"))])
            }
        }
    }

    #[tool(
        description = "Run one installed registry flow exactly once through Direct/WDA: \
        the daemon validates the whole sequence, holds one control lock, and stops at \
        the first failed step. The happy path costs one tool call and zero screenshots. \
        Requires phone_status drivable=true. Pass the flow's declared inputs; a flow \
        declared risk=side_effect is refused unless confirm=true. The result reports \
        completed/applied counts and the failed step; retry_safe=false means DO NOT \
        replay — inspect phone_elements, repair the flow, do not guess. Unverified flows \
        (verified=false in phone_flow_list) may need a checkpoint screenshot afterwards."
    )]
    async fn phone_flow_run(
        &self,
        Parameters(FlowRunParams {
            id,
            inputs,
            confirm,
        }): Parameters<FlowRunParams>,
    ) -> CallToolResult {
        if !crate::registry::valid_flow_id(&id) {
            return CallToolResult::error(vec![Content::text(format!(
                "{id:?} is not a registry id; expected <app>/<flow> lowercase slugs"
            ))]);
        }
        let path = match crate::registry::resolve_target(&id) {
            Ok(path) => path,
            Err(e) => return CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
        };
        let flow = match crate::flow::load_flow(&path) {
            Ok(flow) => flow,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "flow {id} failed validation: {e:#}"
                ))])
            }
        };
        if let Err(e) = crate::flow::check_input_map(&inputs, &flow.inputs) {
            return CallToolResult::error(vec![Content::text(format!("{e:#}"))]);
        }
        match crate::flow::execute_flow(&flow, &inputs, &self.daemon, confirm).await {
            Ok(body) => {
                let result = serde_json::from_str::<serde_json::Value>(&body)
                    .unwrap_or(serde_json::Value::String(body));
                let mut summary = serde_json::json!({
                    "flow": id,
                    "verified": flow.meta.verified(),
                    "risk": flow.meta.risk_label(),
                    "result": result,
                });
                if !flow.meta.verified() {
                    summary["hint"] = serde_json::json!(
                        "this flow had no hardware verification yet — if the phone is now where the flow \
                         promised, tell the user and offer to add verified_on via phone_flow_publish"
                    );
                }
                CallToolResult::success(vec![Content::text(summary.to_string())])
            }
            Err(e) => {
                // The daemon's structured result is inside the error chain for
                // HTTP 4xx/5xx; keep whatever JSON we can find for the report.
                let text = format!("{e:#}");
                let result = text.find('{').and_then(|start| {
                    serde_json::from_str::<serde_json::Value>(&text[start..]).ok()
                });
                let status = self
                    .daemon
                    .status()
                    .await
                    .ok()
                    .and_then(|s| serde_json::to_value(s).ok());
                self.remember_flow_failure(crate::contrib::ReportContext {
                    id: id.clone(),
                    result,
                    status,
                    application: None,
                    note: None,
                });
                CallToolResult::error(vec![Content::text(format!(
                    "flow {id} did not complete: {text}. Next: read phone_elements to see where the phone \
                     stopped; if the flow itself is wrong (label changed, app updated), call \
                     phone_flow_report(id=\"{id}\", confirm=true) with the user's go-ahead — the failure \
                     details are already captured. Do NOT replay the flow blindly."
                ))])
            }
        }
    }

    #[tool(
        description = "Contribute a working flow to the official registry: fork if needed, add the \
        file (+ app.json for a new app), rebuild index.json, push a branch, and open a pull \
        request via the user's GitHub CLI login. OUTWARD-FACING: requires confirm=true, which \
        you may set only after the user agreed to publish. Use after a flow you compiled has \
        run successfully on the phone; put device/iOS/date in the file's verified_on first \
        (an unverified file opens as a draft PR). `source` is a flow file path or an id you \
        installed with `flow add`; `aliases` are the foreground-app labels (per language) that \
        should surface this app's flows from phone_elements."
    )]
    async fn phone_flow_publish(
        &self,
        Parameters(FlowPublishParams {
            source,
            id,
            app_name,
            aliases,
            note,
            confirm,
        }): Parameters<FlowPublishParams>,
    ) -> CallToolResult {
        if !confirm {
            return CallToolResult::error(vec![Content::text(
                "phone_flow_publish opens a public pull request; ask the user, then call again with confirm=true",
            )]);
        }
        let path = match crate::contrib::publish_source(&source) {
            Ok(path) => path,
            Err(e) => return CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
        };
        let options = crate::contrib::PublishOptions {
            id,
            app_name,
            aliases,
            note,
            draft: false,
        };
        match tokio::task::spawn_blocking(move || crate::contrib::publish(&path, &options)).await {
            Ok(Ok(report)) => CallToolResult::success(vec![Content::text(
                serde_json::to_string(&report).unwrap_or_default(),
            )]),
            Ok(Err(e)) => {
                CallToolResult::error(vec![Content::text(format!("publish failed: {e:#}"))])
            }
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("publish task failed: {e}"))])
            }
        }
    }

    #[tool(
        description = "File an issue on the official flow registry for an installed flow that \
        failed (label changed, app updated, wrong locale). Uses the failure captured by the last \
        phone_flow_run of that id — failed step, redacted daemon result, daemon version — so you \
        only add a short note. Screen labels, typed text, and element lists are stripped. \
        OUTWARD-FACING: requires confirm=true after the user agreed. Do not file for a phone \
        that was simply locked/offline, or for a flow you ran on the wrong app/locale."
    )]
    async fn phone_flow_report(
        &self,
        Parameters(FlowReportParams { id, note, confirm }): Parameters<FlowReportParams>,
    ) -> CallToolResult {
        if !confirm {
            return CallToolResult::error(vec![Content::text(
                "phone_flow_report opens a public issue; ask the user, then call again with confirm=true",
            )]);
        }
        if !crate::registry::valid_flow_id(&id) {
            return CallToolResult::error(vec![Content::text(format!(
                "{id:?} is not a registry id"
            ))]);
        }
        let mut context =
            self.take_flow_failure(&id)
                .unwrap_or_else(|| crate::contrib::ReportContext {
                    id: id.clone(),
                    ..Default::default()
                });
        if context.result.is_none() && note.as_deref().is_none_or(|n| n.trim().is_empty()) {
            return CallToolResult::error(vec![Content::text(
                "no captured failure for this flow in this session — run it first, or pass a note describing what went wrong",
            )]);
        }
        if note.is_some() {
            context.note = note;
        }
        if context.application.is_none() {
            if let Ok(body) = self.daemon.elements().await {
                context.application = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v["elements"].as_array().and_then(|rows| {
                            rows.iter()
                                .find(|r| r["kind"] == "Application")
                                .and_then(|r| r["label"].as_str().map(String::from))
                        })
                    });
            }
        }
        match tokio::task::spawn_blocking(move || crate::contrib::report(&context)).await {
            Ok(Ok(outcome)) => CallToolResult::success(vec![Content::text(
                serde_json::to_string(&outcome).unwrap_or_default(),
            )]),
            Ok(Err(e)) => {
                CallToolResult::error(vec![Content::text(format!("report failed: {e:#}"))])
            }
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("report task failed: {e}"))])
            }
        }
    }

    #[tool(description = "Mirror the official iphone-use flow registry \
        (github.com/leeguooooo/iphone-use-flows) into the local store. Every file is \
        checksum-verified and strictly validated before it is written; locally added \
        flows are kept. Call once when phone_flow_list reports an empty store, or to \
        pick up new flows. Network only; the phone is not touched.")]
    async fn phone_flow_update(&self) -> CallToolResult {
        match crate::registry::update().await {
            Ok(report) => CallToolResult::success(vec![Content::text(
                serde_json::to_string(&report).unwrap_or_default(),
            )]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("flow update failed: {e:#}"))])
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
