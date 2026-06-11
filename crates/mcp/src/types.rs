//! Shared wire types used by both the daemon HTTP client and the MCP server.
//!
//! These are kept in their own module so the unit tests can import them without
//! pulling in any I/O.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Daemon → MCP types
// ---------------------------------------------------------------------------

/// Response body from `GET /agent/status`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatusResponse {
    pub ok: bool,
    pub phone_target: bool,
    /// L2 element layer (WebDriverAgent) live right now. `None` when talking
    /// to an older daemon that predates the field.
    #[serde(default)]
    pub wda: Option<bool>,
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
    /// Scroll-wheel gesture.  `dy < 0` scrolls content up (iOS "natural").
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
    /// Send text. With the L2 element layer live (`wda:true` in status) the
    /// daemon routes this through WebDriverAgent and **any Unicode (incl. CJK)
    /// lands cleanly**; otherwise it falls back to per-character keycodes,
    /// which only handle printable US-ASCII reliably.
    Text { text: String },
    /// Named key press: `return | escape | space | tab | delete | up | down |
    /// left | right`.
    Key { name: String },
    /// iOS system shortcut: `home | spotlight | switcher`.
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
        let msg = InputMsg::Scroll { x: 0.5, y: 0.5, dx: 0.0, dy: -12.0 };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "scroll");
        assert_eq!(v["dy"], -12.0);
    }

    #[test]
    fn text_wire_format() {
        let msg = InputMsg::Text { text: "hello".to_string() };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn key_wire_format() {
        let msg = InputMsg::Key { name: "return".to_string() };
        let v: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
        assert_eq!(v["type"], "key");
        assert_eq!(v["name"], "return");
    }

    #[test]
    fn shortcut_wire_format() {
        let msg = InputMsg::Shortcut { name: "home".to_string() };
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
        let orig = InputMsg::Scroll { x: 0.5, y: 0.5, dx: 3.0, dy: -20.0 };
        assert_eq!(roundtrip(&orig), orig);
    }

    #[test]
    fn roundtrip_text() {
        let orig = InputMsg::Text { text: "hello world".into() };
        assert_eq!(roundtrip(&orig), orig);
    }

    #[test]
    fn roundtrip_key_variants() {
        for name in &["return", "escape", "space", "tab", "delete", "up", "down", "left", "right"] {
            let orig = InputMsg::Key { name: name.to_string() };
            assert_eq!(roundtrip(&orig), orig, "key={name}");
        }
    }

    #[test]
    fn roundtrip_shortcut_variants() {
        for name in &["home", "spotlight", "switcher"] {
            let orig = InputMsg::Shortcut { name: name.to_string() };
            assert_eq!(roundtrip(&orig), orig, "shortcut={name}");
        }
    }

    #[test]
    fn daemon_control_msg_compat() {
        // Verify that our serialization exactly matches the wire shapes the
        // daemon's input_bridge::decode_control expects (same serde tag).
        let cases: &[(&str, serde_json::Value)] = &[
            ("tap",    json!({"type":"tap","x":0.5,"y":0.5})),
            ("scroll", json!({"type":"scroll","x":0.5,"y":0.5,"dx":0.0,"dy":-12.0})),
            ("text",   json!({"type":"text","text":"hi"})),
            ("key",    json!({"type":"key","name":"return"})),
            ("shortcut", json!({"type":"shortcut","name":"home"})),
        ];
        let msgs: &[InputMsg] = &[
            InputMsg::Tap { x: 0.5, y: 0.5 },
            InputMsg::Scroll { x: 0.5, y: 0.5, dx: 0.0, dy: -12.0 },
            InputMsg::Text { text: "hi".into() },
            InputMsg::Key { name: "return".into() },
            InputMsg::Shortcut { name: "home".into() },
        ];
        for ((label, expected), msg) in cases.iter().zip(msgs.iter()) {
            let got: serde_json::Value = serde_json::from_str(&msg.to_json()).unwrap();
            assert_eq!(&got, expected, "mismatch for {label}");
        }
    }
}
