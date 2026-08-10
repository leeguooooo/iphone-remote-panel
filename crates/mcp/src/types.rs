//! Shared wire types used by both the daemon HTTP client and the MCP server.
//!
//! These are kept in their own module so the unit tests can import them without
//! pulling in any I/O.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Daemon → MCP types
// ---------------------------------------------------------------------------

/// Response body from `GET /agent/status`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatusResponse {
    pub ok: bool,
    /// Configured device backend. Current daemons report `direct` or `mirror`.
    /// Optional for compatibility with older daemon releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default)]
    pub phone_target: bool,
    /// WebDriverAgent's HTTP service is reachable. This alone does not mean it
    /// can perform actions; use `wda_actionable` / `drivable` for that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wda: Option<bool>,
    /// A real device-side action probe succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wda_actionable: Option<bool>,
    /// Device lock state. The daemon currently emits this as `wda_locked`; the
    /// MCP surface exposes the shorter `locked` name while accepting either.
    #[serde(default, alias = "wda_locked", skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// Whether an input command can be delivered right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drivable: Option<bool>,
    /// Stable lifecycle state such as `ready`, `locked`, `blocked`, `offline`,
    /// or `released`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_state: Option<String>,
    /// Device-screen transport state, independent from control readiness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_state: Option<String>,
    /// Compatibility mode reported by the daemon (`agent`, `mirror`, `offline`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Whether setup persisted one canonical iPhone UDID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_configured: Option<bool>,
    /// Whether the daemon owns the local WDA supervisor/relay lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_wda: Option<bool>,
    /// First-run local management intent is waiting for a canonical target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_wda_pending: Option<bool>,
    /// Component responsible for recovery (`daemon`, `unconfigured`,
    /// `external`, or `mirror`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_owner: Option<String>,
    /// True when the daemon intentionally stopped WDA after an idle period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released: Option<bool>,
    /// True while the daemon is actively stopping WDA for idle release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub releasing: Option<bool>,
    /// True while a managed WDA supervisor is being bootstrapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnecting: Option<bool>,
    /// Human-readable recovery guidance from the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Current WDA setup blocker (`warp`, `usb`, `trust`, `ddi`, or empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_blocked_on: Option<String>,
    /// Current managed-WDA setup phase, such as `prereq`, `building`, or
    /// `ready`. Empty when no fresh helper progress is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_phase: Option<String>,
    /// Bounded human-readable detail for the current setup phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_message: Option<String>,
    /// Preserve additional daemon fields (version/update metadata, legacy
    /// mirror diagnostics, viewer count, and fields added by future releases)
    /// when MCP parses and re-serializes the status response.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Input control messages (MCP → daemon POST /agent/input)
// ---------------------------------------------------------------------------

/// Every JSON shape the daemon's `POST /agent/input` endpoint accepts.
///
/// The `#[serde(tag = "type", rename_all = "snake_case")]` mirrors the wire
/// format already used by `crates/server/src/input_bridge.rs` so the daemon
/// can deserialize them without modification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputMsg {
    /// Tap at normalized coordinates (0–1).
    Tap { x: f64, y: f64 },
    /// Scroll gesture. Positive `dy` reveals content farther down; negative
    /// `dy` reveals content above. Positive `dx` reveals content to the right.
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
    /// Send Unicode text through the device-side input service.
    Text { text: String },
    /// Device-native named key press.
    Key { name: String },
    /// Supported iOS system shortcut: `home | spotlight`. App Switcher is not
    /// available through WDA and must not be advertised as supported.
    Shortcut { name: String },
    /// Begin a long-press at normalized coordinates.  Pair with an `Up` event
    /// to release.
    Longpress { x: f64, y: f64 },
    /// Mouse/touch down at normalized coordinates.
    Down { x: f64, y: f64 },
    /// Mouse/touch up at normalized coordinates.
    Up { x: f64, y: f64 },
}

impl InputMsg {
    /// Serialize to the JSON wire format expected by the daemon.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("InputMsg serialization is infallible")
    }
}

// ---------------------------------------------------------------------------
// Unit tests (pure — no I/O, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn direct_status_preserves_recovery_fields() {
        let status: StatusResponse = serde_json::from_value(json!({
            "ok": true,
            "backend": "direct",
            "phone_target": false,
            "wda": true,
            "wda_actionable": false,
            "wda_locked": true,
            "drivable": false,
            "device_state": "locked",
            "screen_state": "waiting",
            "mode": "agent",
            "target_configured": true,
            "managed_wda": true,
            "managed_wda_pending": false,
            "recovery_owner": "daemon",
            "releasing": false,
            "reconnecting": false,
            "released": false,
            "hint": "unlock the phone",
            "setup_blocked_on": "trust",
            "setup_phase": "building",
            "setup_message": "phone is locked — unlock it and keep it awake",
            "viewer_count": 2
        }))
        .unwrap();

        assert_eq!(status.backend.as_deref(), Some("direct"));
        assert_eq!(status.wda_actionable, Some(false));
        assert_eq!(status.locked, Some(true));
        assert_eq!(status.drivable, Some(false));
        assert_eq!(status.device_state.as_deref(), Some("locked"));
        assert_eq!(status.screen_state.as_deref(), Some("waiting"));
        assert_eq!(status.target_configured, Some(true));
        assert_eq!(status.managed_wda, Some(true));
        assert_eq!(status.managed_wda_pending, Some(false));
        assert_eq!(status.recovery_owner.as_deref(), Some("daemon"));
        assert_eq!(status.released, Some(false));
        assert_eq!(status.releasing, Some(false));
        assert_eq!(status.reconnecting, Some(false));
        assert_eq!(status.hint.as_deref(), Some("unlock the phone"));
        assert_eq!(status.setup_blocked_on.as_deref(), Some("trust"));
        assert_eq!(status.setup_phase.as_deref(), Some("building"));
        assert_eq!(
            status.setup_message.as_deref(),
            Some("phone is locked — unlock it and keep it awake")
        );

        // MCP callers get the normalized field name instead of the daemon's
        // historical `wda_locked` spelling.
        let output = serde_json::to_value(&status).unwrap();
        assert_eq!(output["locked"], true);
        assert_eq!(output["viewer_count"], 2);
        assert!(output.get("wda_locked").is_none());
    }

    #[test]
    fn legacy_status_without_direct_fields_still_parses() {
        let status: StatusResponse = serde_json::from_value(json!({
            "ok": true,
            "phone_target": true,
            "wda": false
        }))
        .unwrap();

        assert_eq!(status.backend, None);
        assert_eq!(status.device_state, None);
        assert_eq!(status.screen_state, None);
        assert_eq!(status.wda_actionable, None);
        assert_eq!(status.locked, None);
        assert_eq!(status.drivable, None);
        assert_eq!(status.released, None);
        assert_eq!(status.releasing, None);
        assert_eq!(status.reconnecting, None);
        assert_eq!(status.managed_wda, None);
        assert_eq!(status.recovery_owner, None);
        assert_eq!(status.hint, None);
        assert_eq!(status.setup_blocked_on, None);
        assert_eq!(status.setup_phase, None);
        assert_eq!(status.setup_message, None);
        assert!(status.extra.is_empty());
    }

    /// Helper: round-trip through JSON and back.
    fn roundtrip(msg: &InputMsg) -> InputMsg {
        let s = msg.to_json();
        serde_json::from_str(&s).expect("round-trip deserialize")
    }

    #[test]
    fn tap_wire_format() {
        let msg = InputMsg::Tap { x: 0.5, y: 0.25 };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "tap");
        assert_eq!(v["x"], 0.5);
        assert_eq!(v["y"], 0.25);
    }

    #[test]
    fn scroll_wire_format() {
        let msg = InputMsg::Scroll {
            x: 0.5,
            y: 0.5,
            dx: 0.0,
            dy: -12.0,
        };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "scroll");
        assert_eq!(v["dy"], -12.0);
    }

    #[test]
    fn text_wire_format() {
        let msg = InputMsg::Text {
            text: "hello".to_string(),
        };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn key_wire_format() {
        let msg = InputMsg::Key {
            name: "return".to_string(),
        };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "key");
        assert_eq!(v["name"], "return");
    }

    #[test]
    fn shortcut_wire_format() {
        let msg = InputMsg::Shortcut {
            name: "home".to_string(),
        };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "shortcut");
        assert_eq!(v["name"], "home");
    }

    #[test]
    fn longpress_wire_format() {
        let msg = InputMsg::Longpress { x: 0.3, y: 0.7 };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "longpress");
    }

    #[test]
    fn down_up_wire_format() {
        let down = InputMsg::Down { x: 0.1, y: 0.2 };
        let up = InputMsg::Up { x: 0.1, y: 0.2 };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&down.to_json()).unwrap()["type"],
            "down"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&up.to_json()).unwrap()["type"],
            "up"
        );
    }

    #[test]
    fn roundtrip_tap() {
        let orig = InputMsg::Tap { x: 0.123, y: 0.456 };
        assert_eq!(roundtrip(&orig), orig);
    }

    #[test]
    fn roundtrip_scroll() {
        let orig = InputMsg::Scroll {
            x: 0.5,
            y: 0.5,
            dx: 3.0,
            dy: -20.0,
        };
        assert_eq!(roundtrip(&orig), orig);
    }

    #[test]
    fn roundtrip_text() {
        let orig = InputMsg::Text {
            text: "hello world".into(),
        };
        assert_eq!(roundtrip(&orig), orig);
    }

    #[test]
    fn roundtrip_key_variants() {
        for name in &[
            "return", "escape", "space", "tab", "delete", "up", "down", "left", "right",
        ] {
            let orig = InputMsg::Key {
                name: name.to_string(),
            };
            assert_eq!(roundtrip(&orig), orig, "key={name}");
        }
    }

    #[test]
    fn roundtrip_shortcut_variants() {
        for name in &["home", "spotlight", "switcher"] {
            let orig = InputMsg::Shortcut {
                name: name.to_string(),
            };
            assert_eq!(roundtrip(&orig), orig, "shortcut={name}");
        }
    }

    #[test]
    fn daemon_control_msg_compat() {
        // Verify that our serialization exactly matches the wire shapes the
        // daemon's input_bridge::decode_control expects (same serde tag).
        let cases: &[(&str, serde_json::Value)] = &[
            ("tap", json!({"type":"tap","x":0.5,"y":0.5})),
            (
                "scroll",
                json!({"type":"scroll","x":0.5,"y":0.5,"dx":0.0,"dy":-12.0}),
            ),
            ("text", json!({"type":"text","text":"hi"})),
            ("key", json!({"type":"key","name":"return"})),
            ("shortcut", json!({"type":"shortcut","name":"home"})),
        ];
        let msgs: &[InputMsg] = &[
            InputMsg::Tap { x: 0.5, y: 0.5 },
            InputMsg::Scroll {
                x: 0.5,
                y: 0.5,
                dx: 0.0,
                dy: -12.0,
            },
            InputMsg::Text { text: "hi".into() },
            InputMsg::Key {
                name: "return".into(),
            },
            InputMsg::Shortcut {
                name: "home".into(),
            },
        ];
        for ((label, expected), msg) in cases.iter().zip(msgs.iter()) {
            let got: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
            assert_eq!(&got, expected, "mismatch for {label}");
        }
    }
}
