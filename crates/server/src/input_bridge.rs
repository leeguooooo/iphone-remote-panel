//! Bridge from web-client wire events to `core::input` injection.
//!
//! Two pure decoders (unit-testable on every platform) plus a background
//! injector thread that owns the real [`core::input::CgEventSink`] (macOS only).
//!
//! ## Wire formats
//!
//! * **control** data channel — JSON objects:
//!   - `{type:"down", x, y}` → [`InputEvent::Down`]
//!   - `{type:"up", x, y}` → [`InputEvent::Up`]
//!   - `{type:"tap", x, y}` → [`InputEvent::Tap`]
//!   - `{type:"longpress", x, y}` → [`InputEvent::Down`] (held; the iOS side
//!     times the long-press — we map it to a Down so the press registers, and the
//!     following `up` releases it)
//!   - `{type:"shortcut", name}` → [`InputEvent::Shortcut`]
//!   - `{type:"text", text}` → [`InputEvent::Text`]
//!   - `{type:"key", name}` → [`InputEvent::Key`]
//!
//! * **move** data channel — binary, big-endian, two shapes distinguished by length:
//!   - 5 bytes `[u16 nx*65535][u16 ny*65535][u8 flags]` (bit0 = button down) →
//!     [`InputEvent::Move`] (a mouse-drag, for long-press-then-drag gestures).
//!   - 7 bytes `[u16 nx*65535][u16 ny*65535][i8 dx][i8 dy][u8 flags]` (bit1 set) →
//!     [`InputEvent::Scroll`] (a swipe → scroll-wheel; Mirroring won't scroll on
//!     a mouse-drag, only on real scroll-wheel events). `dx`/`dy` are CSS-pixel
//!     deltas; `nx`/`ny` anchor the cursor.

use core::input::InputEvent;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// control-channel JSON decode
// ---------------------------------------------------------------------------

/// The control-channel message shapes the web client emits.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    Down { x: f64, y: f64 },
    Up { x: f64, y: f64 },
    Tap { x: f64, y: f64 },
    Longpress { x: f64, y: f64 },
    Shortcut { name: String },
    Text { text: String },
    Key { name: String },
}

/// Decode a control-channel JSON string into an [`InputEvent`].
///
/// Returns `None` if the JSON does not match any known control message (the
/// caller silently drops unknown messages so a future client extension can't
/// crash the daemon).
pub fn decode_control(json: &str) -> Option<InputEvent> {
    let msg: ControlMsg = serde_json::from_str(json).ok()?;
    Some(control_to_event(msg))
}

/// Pure mapping `ControlMsg` → `InputEvent` (no IO).
pub fn control_to_event(msg: ControlMsg) -> InputEvent {
    match msg {
        ControlMsg::Down { x, y } => InputEvent::Down { x, y },
        ControlMsg::Up { x, y } => InputEvent::Up { x, y },
        ControlMsg::Tap { x, y } => InputEvent::Tap { x, y },
        // A long-press is a press that the client times on its side; we register
        // it as a Down (the matching `up` releases it).
        ControlMsg::Longpress { x, y } => InputEvent::Down { x, y },
        ControlMsg::Shortcut { name } => InputEvent::Shortcut(name),
        ControlMsg::Text { text } => InputEvent::Text(text),
        ControlMsg::Key { name } => InputEvent::Key(name),
    }
}

// ---------------------------------------------------------------------------
// move-channel binary decode
// ---------------------------------------------------------------------------

/// Decode a binary move-channel packet into an [`InputEvent`].
///
/// Two shapes, distinguished by length:
/// * **5 bytes** `[u16 nx][u16 ny][u8 flags]` → [`InputEvent::Move`] (mouse-drag,
///   for long-press-then-drag). `nx/ny` are normalized via `/65535`.
/// * **7 bytes** `[u16 nx][u16 ny][i8 dx][i8 dy][u8 flags]` with the scroll bit
///   (`0x02`) set → [`InputEvent::Scroll`]. `nx/ny` anchor the cursor; `dx/dy`
///   are signed CSS-pixel deltas.
///
/// Returns `None` for any other length / unset scroll bit.
pub fn decode_move(bytes: &[u8]) -> Option<InputEvent> {
    match bytes.len() {
        5 => {
            let nx = u16::from_be_bytes([bytes[0], bytes[1]]) as f64 / 65535.0;
            let ny = u16::from_be_bytes([bytes[2], bytes[3]]) as f64 / 65535.0;
            Some(InputEvent::Move { x: nx, y: ny })
        }
        7 if bytes[6] & 0x02 != 0 => {
            let nx = u16::from_be_bytes([bytes[0], bytes[1]]) as f64 / 65535.0;
            let ny = u16::from_be_bytes([bytes[2], bytes[3]]) as f64 / 65535.0;
            let dx = (bytes[4] as i8) as f64;
            let dy = (bytes[5] as i8) as f64;
            Some(InputEvent::Scroll { x: nx, y: ny, dx, dy })
        }
        _ => None,
    }
}

/// Returns `true` if the move packet's flags byte has the button-down bit set.
pub fn move_button_down(bytes: &[u8]) -> bool {
    bytes.len() == 5 && (bytes[4] & 0x01) != 0
}

// ---------------------------------------------------------------------------
// Background injector (macOS owns a CgEventSink on a dedicated thread)
// ---------------------------------------------------------------------------

/// A handle the WebRTC layer uses to enqueue decoded [`InputEvent`]s for
/// injection on the dedicated injector thread.
///
/// Cloneable; dropping all clones lets the injector thread exit.
#[derive(Clone)]
pub struct InputInjector {
    tx: std::sync::mpsc::Sender<InputEvent>,
}

impl InputInjector {
    /// Enqueue an event for injection. Non-blocking; drops the event if the
    /// injector thread has gone away.
    pub fn send(&self, ev: InputEvent) {
        let _ = self.tx.send(ev);
    }
}

/// Spawn the injector thread.
///
/// On macOS it owns a [`core::input::CgEventSink`] and the [`SessionGeometry`],
/// brings the Mirroring window frontmost before the first injection, and calls
/// [`core::input::inject`] for every enqueued event. The gate closure decides,
/// per event, whether injection is currently allowed (the control lease check) —
/// it is evaluated on the injector thread so the lease `Mutex` is only touched
/// from one place.
///
/// On non-macOS it drains and discards events (no OS sink available).
/// `target` = `(cua_driver_path, pid, window_id)` of the Mirroring window, used
/// for key/text/shortcut injection via cua-driver. `None` → pointer-only.
pub fn spawn_injector<F>(
    geo: core::coords::SessionGeometry,
    target: Option<(String, i32, u32)>,
    is_allowed: F,
) -> InputInjector
where
    F: Fn() -> bool + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input-injector".into())
        .spawn(move || injector_loop(geo, target, rx, is_allowed))
        .expect("spawn input-injector thread");
    InputInjector { tx }
}

#[cfg(target_os = "macos")]
fn injector_loop<F>(
    geo: core::coords::SessionGeometry,
    target: Option<(String, i32, u32)>,
    rx: std::sync::mpsc::Receiver<InputEvent>,
    is_allowed: F,
) where
    F: Fn() -> bool,
{
    use core::input::{inject, CgEventSink};
    let mut sink = match target {
        Some((cua, pid, wid)) => CgEventSink::with_cua(cua, pid, wid),
        None => CgEventSink::new(),
    };
    // HID-tap / scroll-wheel events land only on the *frontmost* app. Re-assert
    // frontmost when (and only when) focus was stolen — the check is a cheap
    // in-process query, while the `open -a` re-activation is throttled so a brief
    // not-yet-frontmost window after activation doesn't spawn a burst of them.
    let mut last_front: Option<std::time::Instant> = None;
    const FRONT_THROTTLE: std::time::Duration = std::time::Duration::from_millis(400);
    while let Ok(ev) = rx.recv() {
        if !is_allowed() {
            continue;
        }
        if !crate::macos::mirroring_is_frontmost() {
            let now = std::time::Instant::now();
            let throttled = last_front.is_some_and(|t| now.duration_since(t) < FRONT_THROTTLE);
            if !throttled {
                crate::macos::bring_mirroring_frontmost();
                last_front = Some(now);
            }
        }
        if let Err(e) = inject(&ev, &geo, &mut sink) {
            tracing::debug!("inject dropped {ev:?}: {e}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn injector_loop<F>(
    _geo: core::coords::SessionGeometry,
    _target: Option<(String, i32, u32)>,
    rx: std::sync::mpsc::Receiver<InputEvent>,
    _is_allowed: F,
) where
    F: Fn() -> bool,
{
    // No OS sink off-macOS; drain so senders don't block.
    while rx.recv().is_ok() {}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_control_down() {
        let ev = decode_control(r#"{"type":"down","x":0.25,"y":0.75}"#).unwrap();
        assert_eq!(ev, InputEvent::Down { x: 0.25, y: 0.75 });
    }

    #[test]
    fn decode_control_up() {
        let ev = decode_control(r#"{"type":"up","x":0.1,"y":0.2}"#).unwrap();
        assert_eq!(ev, InputEvent::Up { x: 0.1, y: 0.2 });
    }

    #[test]
    fn decode_control_tap() {
        let ev = decode_control(r#"{"type":"tap","x":0.5,"y":0.5}"#).unwrap();
        assert_eq!(ev, InputEvent::Tap { x: 0.5, y: 0.5 });
    }

    #[test]
    fn decode_control_longpress_maps_to_down() {
        let ev = decode_control(r#"{"type":"longpress","x":0.3,"y":0.4}"#).unwrap();
        assert_eq!(ev, InputEvent::Down { x: 0.3, y: 0.4 });
    }

    #[test]
    fn decode_control_shortcut() {
        let ev = decode_control(r#"{"type":"shortcut","name":"home"}"#).unwrap();
        assert_eq!(ev, InputEvent::Shortcut("home".to_string()));
    }

    #[test]
    fn decode_control_text() {
        let ev = decode_control(r#"{"type":"text","text":"hello 世界"}"#).unwrap();
        assert_eq!(ev, InputEvent::Text("hello 世界".to_string()));
    }

    #[test]
    fn decode_control_key() {
        let ev = decode_control(r#"{"type":"key","name":"return"}"#).unwrap();
        assert_eq!(ev, InputEvent::Key("return".to_string()));
    }

    #[test]
    fn decode_control_unknown_is_none() {
        assert!(decode_control(r#"{"type":"wat","x":1}"#).is_none());
        assert!(decode_control("not json").is_none());
        assert!(decode_control(r#"{"type":"toast","text":"x"}"#).is_none());
    }

    #[test]
    fn decode_move_center() {
        // 0x7FFF / 65535 ≈ 0.49999 (center-ish)
        let pkt = [0x7F, 0xFF, 0x7F, 0xFF, 0x01];
        let ev = decode_move(&pkt).unwrap();
        match ev {
            InputEvent::Move { x, y } => {
                assert!((x - 0.49999).abs() < 1e-3, "x={x}");
                assert!((y - 0.49999).abs() < 1e-3, "y={y}");
            }
            other => panic!("expected Move, got {other:?}"),
        }
    }

    #[test]
    fn decode_move_origin_and_far_corner() {
        let origin = decode_move(&[0x00, 0x00, 0x00, 0x00, 0x00]).unwrap();
        assert_eq!(origin, InputEvent::Move { x: 0.0, y: 0.0 });
        let corner = decode_move(&[0xFF, 0xFF, 0xFF, 0xFF, 0x01]).unwrap();
        assert_eq!(corner, InputEvent::Move { x: 1.0, y: 1.0 });
    }

    #[test]
    fn decode_move_big_endian_axes_distinct() {
        // nx = 0x0001 (tiny), ny = 0x8000 (~0.5) — verifies BE byte order + axis order.
        let ev = decode_move(&[0x00, 0x01, 0x80, 0x00, 0x01]).unwrap();
        match ev {
            InputEvent::Move { x, y } => {
                assert!(x < 0.001, "x small, got {x}");
                assert!((y - 0.5).abs() < 0.01, "y ~0.5, got {y}");
            }
            other => panic!("expected Move, got {other:?}"),
        }
    }

    #[test]
    fn decode_move_wrong_length_is_none() {
        assert!(decode_move(&[]).is_none());
        assert!(decode_move(&[0, 0, 0, 0]).is_none());
        assert!(decode_move(&[0, 0, 0, 0, 0, 0]).is_none()); // 6 bytes: no shape
    }

    #[test]
    fn decode_scroll_7byte_deltas_and_anchor() {
        // nx=0x8000 (~0.5), ny=0x4000 (~0.25), dx=+5, dy=-12, flags=0x02 (scroll)
        let pkt = [0x80, 0x00, 0x40, 0x00, 0x05, 0xF4, 0x02];
        match decode_move(&pkt).unwrap() {
            InputEvent::Scroll { x, y, dx, dy } => {
                assert!((x - 0.5).abs() < 0.01, "x ~0.5, got {x}");
                assert!((y - 0.25).abs() < 0.01, "y ~0.25, got {y}");
                assert_eq!(dx, 5.0);
                assert_eq!(dy, -12.0); // 0xF4 as i8 = -12
            }
            other => panic!("expected Scroll, got {other:?}"),
        }
    }

    #[test]
    fn decode_scroll_without_flag_bit_is_none() {
        // 7 bytes but scroll bit (0x02) not set → not a known shape.
        assert!(decode_move(&[0x80, 0x00, 0x40, 0x00, 0x05, 0xF4, 0x01]).is_none());
    }

    #[test]
    fn move_button_down_flag() {
        assert!(move_button_down(&[0, 0, 0, 0, 0x01]));
        assert!(!move_button_down(&[0, 0, 0, 0, 0x00]));
        assert!(move_button_down(&[0, 0, 0, 0, 0xFF]));
        assert!(!move_button_down(&[0, 0, 0, 0])); // wrong length
    }
}
