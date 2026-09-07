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
    /// Ask the daemon to observe the screen after the action and return what
    /// settled (`settle`, `snapshot`, `delta`). Costs extra latency, so it is
    /// off unless you need to know what the tap produced.
    #[serde(default)]
    pub observe: Option<bool>,
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
    /// Ask the daemon to observe the screen after the action and return what
    /// settled (`settle`, `snapshot`, `delta`). Costs extra latency, so it is
    /// off unless you need to know what the action produced.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_type`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeParams {
    /// Unicode text to send through the device-side input service. Focus the
    /// intended field and verify it before typing.
    pub text: String,
    /// Ask the daemon to observe the screen after the action and return what
    /// settled (`settle`, `snapshot`, `delta`). Costs extra latency, so it is
    /// off unless you need to know what the action produced.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_tap_label`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TapLabelParams {
    /// The element's visible accessibility label, exactly as shown by
    /// `phone_elements` (e.g. "新备忘录", "Connect").
    pub label: String,
    /// Ask the daemon to observe the screen after the action and return what
    /// settled (`settle`, `snapshot`, `delta`). Costs extra latency, so it is
    /// off unless you need to know what the action produced.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_tap_element`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TapElementParams {
    /// Zero-based element index from `phone_elements`.
    pub element: usize,
    /// Snapshot token from the same `phone_elements` response.
    pub snapshot: String,
    /// Ask the daemon to observe the screen after the action and return what
    /// settled (`settle`, `snapshot`, `delta`). Costs extra latency, so it is
    /// off unless you need to know what the action produced.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_key`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyParams {
    /// Supported names: `return`/`enter`, `escape`, `space`, `tab`,
    /// `delete`/`backspace`, `up`, `down`, `left`, `right`.
    pub name: String,
    /// Ask the daemon to observe the screen after the action and return what
    /// settled (`settle`, `snapshot`, `delta`). Costs extra latency, so it is
    /// off unless you need to know what the action produced.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_shortcut`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShortcutParams {
    /// Supported names: `home` (Home Screen) and `spotlight` (search).
    /// App Switcher is unsupported by the Direct/WDA backend.
    pub name: String,
    /// Ask the daemon to observe the screen after the action and return what
    /// settled (`settle`, `snapshot`, `delta`). Costs extra latency, so it is
    /// off unless you need to know what the action produced.
    #[serde(default)]
    pub observe: Option<bool>,
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
    /// Press a button on a system alert (UIAlertController) through WDA's
    /// native alert route — the only path that actually acts on one; taps on
    /// alert buttons are acknowledged without effect. Give the exact button
    /// text, or `action: accept|dismiss` for the default/cancel button. Fails
    /// closed when no alert is showing.
    Alert {
        #[serde(default)]
        button: Option<String>,
        #[serde(default)]
        action: Option<String>,
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
    15_000
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
    /// Run even when compat is `broken` or `incompatible` for this phone.
    #[serde(default)]
    pub force: bool,
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

/// Parameters for [`PhoneHandler::phone_hold`].
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct HoldParams {
    /// Seconds to keep the phone (0 clears the hold; at most 14400).
    pub secs: u64,
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
    // phone_capabilities
    // -----------------------------------------------------------------------

    #[tool(
        description = "What this iphone-use build supports, and whether the phone can be \
        driven right now. Read-only: it opens no device connection, wakes nothing, and \
        takes no owner lease, so it is safe to call before deciding what to do. \
        `supported` is static for the configured backend (single-step and batch action \
        vocabularies, the closed `perform` set, whether the element tree and post-action \
        observation exist, and which `mode` values would be accepted). `available` is a \
        cached snapshot with `blocked_by` naming what stands in the way \
        (locked, released, reconnecting, human_handoff, owned_by_other, offline); \
        `ok:null` means the daemon has no evidence either way, not that it is fine. \
        Scope is UI control and observation only — it says nothing about app \
        install/uninstall or other management surfaces."
    )]
    async fn phone_capabilities(&self) -> CallToolResult {
        match self.daemon.capabilities().await {
            Ok(response) => daemon_read_result(&response),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "capabilities failed: {e:#}"
            ))]),
        }
    }

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
        Parameters(TapParams { x, y, observe }): Parameters<TapParams>,
    ) -> CallToolResult {
        send_input_observed(&self.daemon, &InputMsg::Tap { x, y }, observe).await
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
        Parameters(ScrollParams { x, y, dx, dy, observe }): Parameters<ScrollParams>,
    ) -> CallToolResult {
        send_input_observed(&self.daemon, &InputMsg::Scroll { x, y, dx, dy }, observe).await
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
        Parameters(TypeParams { text, observe }): Parameters<TypeParams>,
    ) -> CallToolResult {
        send_input_observed(&self.daemon, &InputMsg::Text { text }, observe).await
    }

    // -----------------------------------------------------------------------
    // phone_key
    // -----------------------------------------------------------------------

    #[tool(description = "Send a device-native named key. Supported names: \
        return/enter, escape, space, tab, delete/backspace, up, down, left, right. \
        Other names return an explicit unsupported error.")]
    async fn phone_key(
        &self,
        Parameters(KeyParams { name, observe }): Parameters<KeyParams>,
    ) -> CallToolResult {
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "return" | "enter" | "escape" | "space" | "tab" | "delete" | "backspace" | "up"
            | "down" | "left" | "right" => {
                send_input_observed(&self.daemon, &InputMsg::Key { name }, observe).await
            }
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
        Parameters(ShortcutParams { name, observe }): Parameters<ShortcutParams>,
    ) -> CallToolResult {
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "home" | "spotlight" => {
                send_input_observed(&self.daemon, &InputMsg::Shortcut { name }, observe).await
            }
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
        alert (native system-alert button; taps on alerts do not act), wait_for, and pause. The daemon validates \
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
        // The batch entry point answers with the same structured result the
        // CLI and phone_flow_run do. A failing batch carries the evidence a
        // caller needs — `failed_step`, `applied_actions`, `retry_safe`, and
        // each step's `observation` — and flattening that into error prose
        // (bounded to a couple of kilobytes, at that) threw away exactly the
        // part that says whether anyone could see the screen.
        let response = match self.daemon.actions_outcome(&request).await {
            Ok(response) => response,
            // The request left this process; what the phone did is unknown.
            Err(error) => {
                return unknown_action_result("transport_error", format!("{error:#}"));
            }
        };

        // A legacy plain-text `ok` is a Mirror single-action acknowledgement.
        // It is not a batch result: it carries no per-step outcome, so it can
        // never stand in for one.
        if response.confirms_action() {
            let body = response.body().to_string();
            let hint = (step_count >= 3).then(|| {
                serde_json::json!({
                    "hint": format!(
                        "{step_count} steps ran deterministically. If this is a task someone will repeat, \
                         save it as a flow: write these steps to a v1 JSON file (typed text → named input), \
                         `flow validate`, then phone_flow_publish (with the user's go-ahead) so the next run \
                         is one phone_flow_run call. Check phone_flow_list first: it may already exist."
                    )
                })
            });
            return with_structure(
                CallToolResult::success(vec![Content::text(crate::registry::attach_hint(
                    body, "registry", hint,
                ))]),
                &response,
            );
        }

        if response.explicit_refusal() {
            // The daemon's own result, whole: status, failed_step, per-step
            // observations, and its own `retry_safe` judgement.
            return with_structure(
                CallToolResult::error(vec![Content::text(response.body().to_string())]),
                &response,
            );
        }

        // Neither a confirmation nor a refusal: the request was answered, but
        // nothing in the answer says what happened to the phone.
        unknown_action_result(
            if response.too_large {
                "response_too_large"
            } else if response.json.is_none() {
                "unparseable_response"
            } else {
                "no_verdict_in_response"
            },
            format!("HTTP {}: {}", response.status.as_u16(), response.preview()),
        )
    }

    // -----------------------------------------------------------------------
    // phone_status
    // -----------------------------------------------------------------------

    #[tool(
        description = "Read Direct/WDA status without taking control. `owner` names the \
        session currently driving the phone (this session presents PHONE_REMOTE_OWNER, \
        else mcp-<pid>); if it is someone else, do not drive the phone — control calls \
        will be refused with phone_owned. The JSON preserves \
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
                let installed = crate::compat::installed_apps(&self.daemon).await;
                let hint = crate::registry::elements_hint(&json, installed.as_ref());
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
        Parameters(TapElementParams {
            element,
            snapshot,
            observe,
        }): Parameters<TapElementParams>,
    ) -> CallToolResult {
        let observe = observe.unwrap_or(false);
        // Refused here, before anything is sent: this is the one case where a
        // retry is provably safe, so it is reported as such rather than as an
        // unknown outcome.
        if snapshot.trim().is_empty() {
            return not_sent_result(
                "missing_snapshot",
                "no request was sent: tap_element needs the snapshot token from the \
                 phone_elements response the index came from."
                    .to_string(),
            );
        }
        // The snapshot is the caller's. This never substitutes one of its own,
        // so the daemon's staleness check runs against the tree the caller
        // actually read.
        match self
            .daemon
            .tap_element_observed(element, &snapshot, observe)
            .await
        {
            Ok(response) => daemon_action_result(
                &response,
                observe,
                &format!("tapped element #{element} from the supplied snapshot"),
            ),
            Err(e) => unknown_action_result(
                "transport_error",
                format!("tap_element #{element} failed: {e:#}."),
            ),
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
        Parameters(TapLabelParams { label, observe }): Parameters<TapLabelParams>,
    ) -> CallToolResult {
        let observe = observe.unwrap_or(false);
        // The snapshot comes from the element read this call performs — never
        // a cached or borrowed baseline.
        match self.daemon.tap_label_observed(&label, observe).await {
            Ok(response) => {
                daemon_action_result(&response, observe, &format!("tapped element: {label}"))
            }
            // `tap_label` reads the element tree before it taps. A failure in
            // that read means no tap was sent; a failure after it is unknown.
            // The two are not distinguishable from here, so this reports the
            // conservative answer rather than guessing from the message text.
            Err(e) => unknown_action_result(
                "transport_error",
                format!("tap_label '{label}' failed: {e:#}."),
            ),
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

    #[tool(
        description = "Keep the phone for a bounded human-in-the-loop pause inside the \
        current user-requested task (the operator types a PIN, approves a prompt, \
        fetches a code): the idle watchdog will not release it for `secs` seconds \
        (0 clears the hold; max 14400). Not for initialization, health checks, or \
        keeping the phone ready; clear it when the step is done. Fails with \
        device_release_in_progress if the daemon is already releasing, and with \
        phone_owned if another session holds the phone lease."
    )]
    async fn phone_hold(&self, Parameters(params): Parameters<HoldParams>) -> CallToolResult {
        match self.daemon.hold(params.secs).await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("hold failed: {e:#}"))]),
        }
    }

    #[tool(
        description = "Hand the phone lease back when this session's phone task is \
        finished, so another session may drive it without waiting for the lease \
        to lapse. Only the current owner can release."
    )]
    async fn phone_release_owner(&self) -> CallToolResult {
        match self.daemon.release_owner().await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("release failed: {e:#}"))]),
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
        verified (has a recorded hardware run), compat against the app version installed on THIS phone \
        (verified|untested-newer|incompatible|broken|needs-verification|draft|unknown), inputs, app, and category. Empty store: \
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
        let installed = crate::compat::installed_apps(&self.daemon).await;
        match crate::registry::list(&filter) {
            Ok((entries, index)) => CallToolResult::success(vec![Content::text(
                crate::registry::list_json(&entries, &index, installed.as_ref()).to_string(),
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
        declared risk=side_effect is refused unless confirm=true; a flow whose compat is broken or \
        incompatible for the installed app version is refused unless force=true. The result reports \
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
            force,
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
        let (compat, _installed) = match crate::flow::compat_gate(&flow, &self.daemon, force).await
        {
            Ok(gate) => gate,
            Err(e) => return CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
        };
        // One shared execution path with the CLI: the daemon's failure body is
        // kept whole and diagnosed, never reverse-parsed out of an error
        // string. An agent driving through MCP sees exactly what a human at
        // the CLI sees.
        let run = match crate::flow::execute_and_diagnose(&flow, &inputs, &self.daemon, confirm)
            .await
        {
            Ok(run) => run,
            // Nothing came back at all — a transport failure, or the flow was
            // refused before dispatch. There is no result to diagnose.
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "flow {id} did not run: {e:#}. Nothing was recorded; check phone_status \
                     and do NOT replay blindly."
                ))])
            }
        };
        let mut summary = serde_json::json!({
            "flow": id,
            "verified": flow.meta.verified(),
            "risk": flow.meta.risk_label(),
            "compat": compat,
            "result": run.value,
        });
        if run.succeeded {
            if compat.compat == crate::compat::Compat::UntestedNewer {
                summary["hint"] = serde_json::json!(
                    "the installed app is newer than this flow's last verification — if the \
                     phone ended where the flow promised, publish an updated verified_on \
                     (phone_flow_publish) so others get compat=verified"
                );
            } else if !flow.meta.verified() {
                summary["hint"] = serde_json::json!(
                    "this flow had no hardware verification yet — if the phone is now where \
                     the flow promised, tell the user and offer to add verified_on via \
                     phone_flow_publish"
                );
            }
            return CallToolResult::success(vec![Content::text(summary.to_string())]);
        }

        // The pre-flight status is already in hand. Asking again costs a
        // round trip on a path that has just failed, and would describe the
        // phone AFTER the failure rather than the run we are reporting.
        let status = Some(run.preflight_status.clone());
        self.remember_flow_failure(crate::contrib::ReportContext {
            id: id.clone(),
            result: Some(summary["result"].clone()),
            status,
            application: None,
            note: None,
        });
        summary["hint"] = serde_json::json!(format!(
            "flow {id} did not complete ({}). `result.diagnosis` says whether the screen could \
             be read and which candidates resemble the locator that failed; candidates are for \
             review only — nothing was retried and the flow was not edited. If the flow itself is \
             wrong (label changed, app updated), call phone_flow_report(id=\"{id}\", confirm=true) \
             with the user's go-ahead. Do NOT replay the flow blindly.",
            match run.status {
                Some(code) => format!("HTTP {code}"),
                None => "no answer from the daemon; the outcome is UNKNOWN, not \"never ran\""
                    .to_string(),
            }
        ));
        CallToolResult::error(vec![Content::text(summary.to_string())])
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
    const MAX_WAIT_MS: u64 = 30_000;
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
            PhoneStep::Alert {
                button,
                action,
                after_ms,
            } => {
                validate_after(after_ms)?;
                let button = button.filter(|b| !b.trim().is_empty());
                let action = action.filter(|a| !a.trim().is_empty());
                match (&button, &action) {
                    (Some(b), None) if b.chars().count() <= 200 => action_step(
                        serde_json::json!({"type":"alert","button":b}),
                        after_ms,
                    ),
                    (None, Some(a)) if a == "accept" || a == "dismiss" => action_step(
                        serde_json::json!({"type":"alert","action":a}),
                        after_ms,
                    ),
                    _ => {
                        return Err(format!(
                            "steps[{index}] alert needs exactly one of button (exact text, ≤200 chars) or action accept|dismiss; no action was sent"
                        ))
                    }
                }
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
/// Attach the parsed JSON as structured content, keeping the text short.
///
/// The JSON goes in whole; the text is only a preview. Putting a large body
/// in the text block is what truncates it — an element tree or a post-action
/// delta is routinely past any display-sized cap, and a client that reads the
/// text would then get unparseable, tail-truncated output.
fn with_structure(
    mut result: CallToolResult,
    response: &crate::client::DaemonResponse,
) -> CallToolResult {
    // Objects only: MCP structured content is an object, so an array or a
    // scalar must not be attached even when it parsed.
    if let Some(json) = response
        .json
        .as_ref()
        .filter(|value| value.is_object())
        .cloned()
    {
        result.structured_content = Some(json);
    }
    result
}

/// A mutation whose result is not known, in the one form a caller can branch
/// on. Never says "not sent": that is a claim about the phone we cannot make
/// once the request left this process.
fn unknown_action_result(reason: &str, detail: String) -> CallToolResult {
    let mut result = CallToolResult::error(vec![Content::text(format!(
        "outcome unknown: {detail} Do NOT resend automatically — check phone_elements or \
         phone_screenshot to see whether it took effect."
    ))]);
    result.structured_content = Some(serde_json::json!({
        "ok": false,
        "error": "outcome_unknown",
        "outcome": "unknown",
        "retry_safe": false,
        "reason": reason,
    }));
    result
}

/// A mutation this process refused before anything left it. Here — and only
/// here — re-sending is known to be safe, because nothing was sent.
fn not_sent_result(reason: &str, detail: String) -> CallToolResult {
    let mut result = CallToolResult::error(vec![Content::text(detail)]);
    result.structured_content = Some(serde_json::json!({
        "ok": false,
        "error": reason,
        "outcome": "not_sent",
        "retry_safe": true,
        "reason": reason,
    }));
    result
}

/// The pre-JSON acknowledgement: `200` with the literal body `ok`.
///
/// The Mirror backend still answers this way after injecting a CGEvent, so
/// treating every non-JSON 2xx as unknown would report every legitimate
/// Mirror action as an uncertain outcome. Recognised EXACTLY — a 200 whose
/// body is nothing but `ok` — so an HTML error page or a truncated body stays
/// unknown.
///
/// It acknowledges that the daemon accepted and dispatched the event; it is
/// NOT evidence that anything happened on screen, because that backend
/// injects into a Mirroring window and nothing reports back.
/// [`crate::client::DaemonResponse::confirms_action`] stays strict about
/// JSON, and this compatibility branch lives only in these seven tools — the
/// shared adapter and the flow verdicts must not learn it.
fn is_legacy_ok_ack(response: &crate::client::DaemonResponse) -> bool {
    response.status == reqwest::StatusCode::OK
        && response.json.is_none()
        && !response.too_large
        && response.body().trim() == "ok"
}

/// Render one MUTATION's daemon response.
///
/// Success requires `confirms_action()`, not `ok()`: a 2xx whose body could
/// not be read (unparseable, or past the read limit) proves the request was
/// accepted, never that the phone did anything. Reporting that as a tool
/// success is how an agent concludes a tap landed when nobody knows.
///
/// When there is no usable evidence the result is an error carrying
/// `retry_safe: false` — "we cannot tell" must not read as "nothing was sent",
/// because a blind resend is the one outcome that turns an uncertainty into a
/// duplicate action on a real phone.
fn daemon_action_result(
    response: &crate::client::DaemonResponse,
    observed: bool,
    plain_success: &str,
) -> CallToolResult {
    if response.confirms_action() {
        let text = if observed {
            response.preview()
        } else {
            plain_success.to_string()
        };
        return with_structure(CallToolResult::success(vec![Content::text(text)]), response);
    }
    // Legacy plain-text acknowledgement (Mirror). Reported as success because
    // the event was dispatched, but labelled so nobody mistakes it for a
    // verified outcome.
    if is_legacy_ok_ack(response) {
        let mut result = if observed {
            // Do not invent an observation this backend cannot produce, and do
            // not turn "no observation" into a reason to send the action again.
            CallToolResult::success(vec![Content::text(format!(
                "{plain_success} (acknowledged, but this backend cannot observe the \
                 result — check phone_screenshot; do NOT resend)"
            ))])
        } else {
            CallToolResult::success(vec![Content::text(plain_success.to_string())])
        };
        result.structured_content = Some(serde_json::json!({
            "ok": true,
            "outcome": "acknowledged",
            "verified": false,
            "protocol": "legacy_text_ack",
            "observation": if observed { "unavailable" } else { "not_requested" },
            "note": "the daemon accepted and dispatched the event; this backend does not \
                     report what happened on screen",
        }));
        return result;
    }
    // A refusal the daemon spelled out keeps its own fields — it knows what
    // happened and said so. "Spelled out" means a body that explicitly says
    // `ok:false`. Merely parsing is not enough: `500 []`, `500 42` and
    // `500 {"unrelated":1}` all parse, none of them is the daemon telling us
    // the action did not run, and treating them as refusals dropped both the
    // structure (an array cannot be attached) and the unknown contract.
    // Shared with the batch entry point so both agree on what counts.
    if response.explicit_refusal() {
        return with_structure(
            CallToolResult::error(vec![Content::text(response.failure_summary())]),
            response,
        );
    }
    // Everything else — an unparseable 2xx, an oversized body, a 500 with an
    // HTML error page — leaves the fate of the action unknown. A non-2xx does
    // NOT imply the request never reached the phone: it can fail on the way
    // back just as easily as on the way out.
    // Same three reasons the batch entry point reports, so a caller can branch
    // on `reason` without knowing which tool produced it. A body that parsed
    // but carries no verdict is NOT "unparseable": we read it fine, it simply
    // did not say what happened.
    unknown_action_result(
        if response.too_large {
            "response_too_large"
        } else if response.json.is_none() {
            "unparseable_response"
        } else {
            "no_verdict_in_response"
        },
        format!(
            "the daemon answered {} and the result did not say what happened ({}).",
            response.status,
            response.preview()
        ),
    )
}

/// Render one READ-ONLY daemon response.
///
/// A read has no side effect to be uncertain about, so `ok()` is the right
/// question here — but a body that could not be parsed is still not data, so
/// it is reported as a failure rather than handed back as if it were.
fn daemon_read_result(response: &crate::client::DaemonResponse) -> CallToolResult {
    if !response.ok() {
        return with_structure(
            CallToolResult::error(vec![Content::text(response.failure_summary())]),
            response,
        );
    }
    // MCP's structured content is an object. A body that parsed into an array
    // or a scalar is not this endpoint's shape, and attaching it would emit
    // something the protocol does not allow.
    if !response.json.as_ref().is_some_and(serde_json::Value::is_object) {
        return CallToolResult::error(vec![Content::text(format!(
            "the daemon answered {} but the body was not a JSON object: {}",
            response.status,
            response.preview()
        ))]);
    }
    with_structure(
        CallToolResult::success(vec![Content::text(response.preview())]),
        response,
    )
}

async fn send_input_observed(
    daemon: &DaemonClient,
    msg: &InputMsg,
    observe: Option<bool>,
) -> CallToolResult {
    let observe = observe.unwrap_or(false);
    match daemon.input_observed(msg, observe).await {
        Ok(response) => daemon_action_result(&response, observe, "ok"),
        // The request may well have reached the phone before the transport
        // broke, so this is unknown, not "not sent".
        Err(e) => unknown_action_result("transport_error", format!("the call failed: {e:#}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // Real tool calls against a scripted daemon.
    // ---------------------------------------------------------------------

    /// One-shot HTTP responder. Returns the URL and a join handle so a panic
    /// inside it surfaces in the test.
    fn scripted_daemon(status: &str, body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // Non-blocking with a deadline: a blocking `accept` that never gets a
        // connection — because the test under it failed early — turns a joined
        // handle into a hung suite.
        listener.set_nonblocking(true).unwrap();
        let status = status.to_string();
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        // The accept deadline does not cover the socket: a
                        // peer that connects and stays silent would park this
                        // thread in `read`.
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                            .ok();
                        stream
                            .set_write_timeout(Some(std::time::Duration::from_secs(10)))
                            .ok();
                        let mut request = [0_u8; 8_192];
                        let _ = stream.read(&mut request);
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                        return;
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        (format!("http://{address}"), task)
    }

    fn handler_for(url: &str) -> PhoneHandler {
        PhoneHandler::new(DaemonClient::new(url.to_string(), None))
    }

    fn block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point of `observe`: a real observation is far past any
    /// display-sized cap, and its TAIL is where `settle` and `delta` live. The
    /// structured content must carry it intact even though the text preview
    /// is trimmed.
    #[test]
    fn an_observed_tap_returns_the_whole_observation_not_a_truncated_preview() {
        let filler = "x".repeat(64 * 1024);
        let body = format!(
            r#"{{"ok":true,"transport":"wda","tree":"{filler}","snapshot":"snap-1","settle":{{"settled":true,"reason":"stable","captures":2}},"delta":{{"added":["搜索"]}}}}"#
        );
        assert!(body.len() > 64 * 1024);
        let (url, task) = scripted_daemon("200 OK", body.into_bytes());
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: Some(true),
        })));
        task.join().unwrap();

        assert_ne!(result.is_error, Some(true), "{:?}", text_of(&result));
        let structured = result
            .structured_content
            .as_ref()
            .expect("an observed action must return structured content");
        assert_eq!(structured["settle"]["reason"], "stable");
        assert_eq!(structured["settle"]["captures"], 2);
        assert_eq!(structured["delta"]["added"][0], "搜索");
        assert_eq!(structured["snapshot"], "snap-1");
    }

    /// Without `observe` the result stays the short string callers already
    /// parse, and the request must not have asked for a delta.
    #[test]
    fn an_unobserved_tap_keeps_its_plain_result() {
        let (url, task) = scripted_daemon(
            "200 OK",
            br#"{"ok":true,"transport":"wda"}"#.to_vec(),
        );
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: None,
        })));
        task.join().unwrap();

        assert_ne!(result.is_error, Some(true));
        assert_eq!(text_of(&result), "ok");
    }

    /// A refusal must arrive with the fields a caller decides on, not as prose.
    #[test]
    fn a_refused_action_keeps_outcome_and_retry_safety() {
        let (url, task) = scripted_daemon(
            "409 Conflict",
            br#"{"ok":false,"error":"phone_owned","outcome":"not_sent","retry_safe":true}"#.to_vec(),
        );
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: None,
        })));
        task.join().unwrap();

        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .as_ref()
            .expect("a structured refusal must survive");
        assert_eq!(structured["error"], "phone_owned");
        assert_eq!(structured["outcome"], "not_sent");
        assert_eq!(structured["retry_safe"], true);
        let text = text_of(&result);
        assert!(text.contains("outcome=not_sent"), "{text}");
    }

    /// A 400 the daemon spelled out is still a spelled-out refusal.
    ///
    /// `invalid_action` is newer than this renderer. Nothing here keeps a list
    /// of known error codes, and this test is what keeps it that way: an
    /// unfamiliar `ok:false` body must reach the caller with its own
    /// `outcome`/`retry_safe` intact, NOT be flattened into `unknown` — which
    /// would strip the very fact that makes it safe to fix and resend.
    #[test]
    fn an_unfamiliar_refusal_code_is_passed_through_not_downgraded() {
        let (url, task) = scripted_daemon(
            "400 Bad Request",
            br#"{"ok":false,"error":"invalid_action","detail":"unknown action type \"teleport\"","outcome":"not_sent","retry_safe":true}"#.to_vec(),
        );
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: None,
        })));
        task.join().unwrap();

        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .as_ref()
            .expect("a structured refusal must survive");
        assert_eq!(structured["error"], "invalid_action");
        assert_ne!(
            structured["outcome"], "unknown",
            "a spelled-out refusal must not be downgraded: {structured}"
        );
        assert_eq!(structured["outcome"], "not_sent");
        assert_eq!(structured["retry_safe"], true);
    }

    /// The dangerous case: HTTP said yes, the body says nothing. That is not a
    /// success, and above all it is not a licence to resend.
    #[test]
    fn an_unreadable_success_is_reported_unknown_and_never_retry_safe() {
        for body in [
            b"not json at all".to_vec(),
            vec![b'x'; 5 * 1024 * 1024], // past the read limit
        ] {
            let (url, task) = scripted_daemon("200 OK", body);
            let handler = handler_for(&url);

            let result = block(handler.phone_tap(Parameters(TapParams {
                x: 0.5,
                y: 0.5,
                observe: None,
            })));
            task.join().unwrap();

            assert_eq!(
                result.is_error,
                Some(true),
                "an unreadable body was reported as a successful tap: {}",
                text_of(&result)
            );
            let structured = result
                .structured_content
                .as_ref()
                .expect("an unknown outcome must still be structured");
            assert_eq!(structured["outcome"], "unknown");
            assert_eq!(
                structured["retry_safe"], false,
                "an unknown outcome authorised a resend"
            );
            let text = text_of(&result);
            assert!(text.contains("Do NOT resend"), "{text}");
        }
    }

    /// A read that could not be parsed is not data, and must not be handed
    /// back as though it were.
    #[test]
    fn capabilities_reports_an_unparseable_body_as_a_failure() {
        let (url, task) = scripted_daemon("200 OK", b"<html>proxy error</html>".to_vec());
        let handler = handler_for(&url);

        let result = block(handler.phone_capabilities());
        task.join().unwrap();

        assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
        assert!(result.structured_content.is_none());

        let (url, task) = scripted_daemon(
            "200 OK",
            br#"{"ok":true,"backend":"direct","supported":{"element_tree":true}}"#.to_vec(),
        );
        let handler = handler_for(&url);
        let result = block(handler.phone_capabilities());
        task.join().unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["supported"]["element_tree"],
            true
        );
    }

    /// Tool discovery, from the router the macro actually generates — the same
    /// list an MCP client receives. Asserting a documented number in prose
    /// would pass while the server exposed something else.
    #[test]
    fn tool_discovery_matches_the_documented_surface() {
        let router = PhoneHandler::tool_router();
        let tools = router.list_all();
        let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        names.sort_unstable();

        assert_eq!(
            names.len(),
            21,
            "tool count changed; update README, the skill, and the CI assertion: {names:?}"
        );
        for required in [
            "phone_capabilities",
            "phone_status",
            "phone_run_steps",
            "phone_hold",
            "phone_release_owner",
        ] {
            assert!(names.contains(&required), "{required} missing: {names:?}");
        }

        // Discovery must be free to call: no required arguments.
        let capabilities = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "phone_capabilities")
            .expect("phone_capabilities");
        let required = capabilities
            .input_schema
            .get("required")
            .and_then(|value| value.as_array());
        assert!(
            required.is_none_or(|list| list.is_empty()),
            "capability discovery asks for arguments: {:?}",
            capabilities.input_schema
        );

        // `observe` is opt-in on every single-step UI tool: in the schema,
        // never required, so calls written before it keep working.
        for name in [
            "phone_tap",
            "phone_scroll",
            "phone_type",
            "phone_key",
            "phone_shortcut",
            "phone_tap_element",
            "phone_tap_label",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} missing"));
            let schema = &tool.input_schema;
            let properties = schema
                .get("properties")
                .and_then(|value| value.as_object())
                .unwrap_or_else(|| panic!("{name} has no properties: {schema:?}"));
            assert!(
                properties.contains_key("observe"),
                "{name} does not accept observe: {schema:?}"
            );
            let required = schema
                .get("required")
                .and_then(|value| value.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|value| value.as_str())
                        .any(|field| field == "observe")
                })
                .unwrap_or(false);
            assert!(!required, "{name} made observe mandatory: {schema:?}");
        }
    }

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
    fn multi_step_alert_encodes_button_or_action_and_rejects_both_or_neither() {
        let ok = phone_steps_request(vec![
            PhoneStep::Alert {
                button: Some("导出".into()),
                action: None,
                after_ms: 100,
            },
            PhoneStep::Alert {
                button: None,
                action: Some("dismiss".into()),
                after_ms: 0,
            },
        ])
        .unwrap();
        assert_eq!(
            ok["steps"][0]["action"],
            serde_json::json!({"type":"alert","button":"导出"})
        );
        assert_eq!(
            ok["steps"][1]["action"],
            serde_json::json!({"type":"alert","action":"dismiss"})
        );
        for step in [
            PhoneStep::Alert {
                button: None,
                action: None,
                after_ms: 0,
            },
            PhoneStep::Alert {
                button: Some("OK".into()),
                action: Some("accept".into()),
                after_ms: 0,
            },
            PhoneStep::Alert {
                button: None,
                action: Some("yes".into()),
                after_ms: 0,
            },
        ] {
            assert!(phone_steps_request(vec![step])
                .unwrap_err()
                .contains("alert needs"));
        }
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
        // The ceiling is 30s — sized for an element read that can cost six or
        // seven seconds on real hardware. One millisecond over is still
        // refused, and refused before anything is sent.
        let invalid_timeout = phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: Some("设置".to_string()),
                present: vec![],
                absent: vec![],
            },
            timeout_ms: 30_001,
            poll_ms: 100,
        }])
        .unwrap_err();
        assert!(invalid_timeout.contains("timeout_ms"));
        assert!(invalid_timeout.contains("no action was sent"));

        // And the ceiling itself is accepted.
        phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: Some("设置".to_string()),
                present: vec![],
                absent: vec![],
            },
            timeout_ms: 30_000,
            poll_ms: 100,
        }])
        .expect("the ceiling is a valid wait");

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
