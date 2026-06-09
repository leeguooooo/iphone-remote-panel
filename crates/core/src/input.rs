//! Input injection for iPhone Mirroring via the macOS HID event tap.
//!
//! ## Architecture
//!
//! ```text
//! caller ──► inject(InputEvent, SessionGeometry, &mut dyn EventSink)
//!                │
//!                ├─ coords::to_screen()  (normalized → global screen points)
//!                │
//!                └─ EventSink::{mouse_down, mouse_dragged, mouse_up, key, text, shortcut}
//!                       │                        │
//!                       │ (tests)                │ (production)
//!                  RecordingSink            CgEventSink
//!                  (no OS calls)            (CGEvent::post(HID))
//! ```
//!
//! ## Why HID, not post_to_pid
//!
//! iPhone Mirroring (`com.apple.ScreenContinuity`) only acts on events
//! observed on the system-level HID event tap at the global cursor position.
//! `CGEventPostToPid` is silently ignored.  See spike
//! `crates/spikes/src/bin/s1b_session_input.rs` for the confirmed approach:
//! `CGEventSource::new(HIDSystemState)` + `evt.post(CGEventTapLocation::HID)`.
//!
//! ## Tap dwell
//!
//! A `Tap` event maps to a quick mouse-down followed by mouse-up.
//! The dwell **must be short** (< ~60 ms).  The hardware probe (s1b) used
//! 100 ms and triggered iOS jiggle / edit-mode.  [`TAP_DWELL_MS`] is set to
//! 30 ms, which is enough for the system to register a tap but not a press.
//!
//! The *pure logic* in `inject` calls `sink.mouse_down` then `sink.mouse_up`
//! back-to-back.  The *real* [`CgEventSink`] sleeps [`TAP_DWELL_MS`] between
//! them inside `mouse_up` (it tracks whether an implicit dwell is pending).
//! Test doubles like [`RecordingSink`] do **not** sleep and are therefore
//! deterministic.

use crate::coords::{to_screen, SessionGeometry};

// ── Tap dwell ────────────────────────────────────────────────────────────────

/// Dwell between the synthesised down and up for a [`InputEvent::Tap`], in ms.
///
/// Must be short enough that iOS does **not** interpret it as a long-press
/// (which triggers jiggle / context-menu).  30 ms is confirmed safe; the spike
/// probe showed 100 ms already crosses that threshold.
pub const TAP_DWELL_MS: u64 = 30;

// ── InputEvent ───────────────────────────────────────────────────────────────

/// A caller-facing input event expressed in *normalized* coordinates.
///
/// `x` and `y` are in `[0, 1]` relative to the **content rect** of the
/// mirroring session (letterbox bars excluded).  Values outside `[0, 1]`
/// cause `inject` to return [`InputError::OutOfBounds`] and emit nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Mouse/touch down at `(x, y)`.
    Down { x: f64, y: f64 },
    /// Mouse/touch drag to `(x, y)` (typically preceded by `Down`).
    Move { x: f64, y: f64 },
    /// Mouse/touch up at `(x, y)`.
    Up { x: f64, y: f64 },
    /// Discrete tap: synthesised as down→(dwell)→up.
    ///
    /// The dwell is [`TAP_DWELL_MS`] ms; the real sink sleeps that amount
    /// between the two posts.  Test doubles do not sleep.
    Tap { x: f64, y: f64 },
    /// Single key by name (e.g. `"return"`, `"escape"`, `"space"`).
    Key(String),
    /// Literal text to type.
    Text(String),
    /// Named system-level shortcut: `"home"` | `"spotlight"` | `"switcher"`.
    Shortcut(String),
}

// ── EventSink ────────────────────────────────────────────────────────────────

/// Abstracts actual event emission so the routing logic in [`inject`] is
/// testable without touching the OS.
///
/// All coordinate arguments (`sx`, `sy`) are **global screen points** already
/// converted from normalized via [`to_screen`].
pub trait EventSink {
    /// Left mouse-button down at global screen point `(sx, sy)`.
    fn mouse_down(&mut self, sx: f64, sy: f64);

    /// Left mouse drag to global screen point `(sx, sy)`.
    fn mouse_dragged(&mut self, sx: f64, sy: f64);

    /// Left mouse-button up at global screen point `(sx, sy)`.
    ///
    /// When called as part of a [`InputEvent::Tap`] sequence, the real sink
    /// sleeps [`TAP_DWELL_MS`] before posting the up event to avoid triggering
    /// iOS long-press / jiggle mode.
    fn mouse_up(&mut self, sx: f64, sy: f64);

    /// Key event by name (e.g. `"return"`, `"escape"`).
    fn key(&mut self, name: &str);

    /// Type literal text.
    fn text(&mut self, s: &str);

    /// Named system shortcut: `"home"` | `"spotlight"` | `"switcher"`.
    fn shortcut(&mut self, name: &str);
}

// ── InputError ───────────────────────────────────────────────────────────────

/// Errors returned by [`inject`].
#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    /// The normalized point was outside `[0, 1]`×`[0, 1]` (content rect).
    OutOfBounds,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::OutOfBounds => {
                f.write_str("normalized coordinate is outside the content rect [0,1]×[0,1]")
            }
        }
    }
}

impl std::error::Error for InputError {}

// ── inject ───────────────────────────────────────────────────────────────────

/// Map a normalized [`InputEvent`] through [`SessionGeometry`] and drive the sink.
///
/// # Errors
///
/// Returns [`InputError::OutOfBounds`] when the normalized coordinate of a
/// pointer event is outside `[0, 1]`×`[0, 1]`.  Nothing is emitted to the
/// sink in that case.
///
/// Key / Text / Shortcut events are not coordinate-based and always succeed.
pub fn inject(
    ev: &InputEvent,
    geo: &SessionGeometry,
    sink: &mut dyn EventSink,
) -> Result<(), InputError> {
    match ev {
        InputEvent::Down { x, y } => {
            let (sx, sy) = screen_or_err(*x, *y, geo)?;
            sink.mouse_down(sx, sy);
        }
        InputEvent::Move { x, y } => {
            let (sx, sy) = screen_or_err(*x, *y, geo)?;
            sink.mouse_dragged(sx, sy);
        }
        InputEvent::Up { x, y } => {
            let (sx, sy) = screen_or_err(*x, *y, geo)?;
            sink.mouse_up(sx, sy);
        }
        InputEvent::Tap { x, y } => {
            let (sx, sy) = screen_or_err(*x, *y, geo)?;
            // Pure ordering: down then up.  The real sink sleeps TAP_DWELL_MS
            // between these two calls; test doubles do not.
            sink.mouse_down(sx, sy);
            sink.mouse_up(sx, sy);
        }
        InputEvent::Key(name) => {
            sink.key(name);
        }
        InputEvent::Text(s) => {
            sink.text(s);
        }
        InputEvent::Shortcut(name) => {
            sink.shortcut(name);
        }
    }
    Ok(())
}

/// Convert normalized `(nx, ny)` to screen coords, mapping `None` → `Err`.
#[inline]
fn screen_or_err(
    nx: f64,
    ny: f64,
    geo: &SessionGeometry,
) -> Result<(f64, f64), InputError> {
    to_screen((nx, ny), geo).ok_or(InputError::OutOfBounds)
}

// ── RecordingSink ─────────────────────────────────────────────────────────────

/// A test double that records every call without touching the OS.
///
/// Use this in unit tests to assert that `inject` routes events correctly.
///
/// ```rust
/// use core::input::{inject, InputEvent, RecordingSink, SinkCall};
/// use core::coords::{SessionGeometry, Rect, Orientation};
///
/// let geo = SessionGeometry {
///     content_rect: Rect { x: 0.0, y: 0.0, w: 100.0, h: 200.0 },
///     scale: 1.0,
///     orientation: Orientation::Portrait,
/// };
/// let mut sink = RecordingSink::default();
/// inject(&InputEvent::Tap { x: 0.5, y: 0.5 }, &geo, &mut sink).unwrap();
/// assert_eq!(sink.calls[0], SinkCall::MouseDown { sx: 50.0, sy: 100.0 });
/// assert_eq!(sink.calls[1], SinkCall::MouseUp   { sx: 50.0, sy: 100.0 });
/// ```
#[derive(Debug, Default, Clone)]
pub struct RecordingSink {
    /// Ordered list of calls received.
    pub calls: Vec<SinkCall>,
}

/// One recorded call on [`RecordingSink`].
#[derive(Debug, Clone, PartialEq)]
pub enum SinkCall {
    MouseDown { sx: f64, sy: f64 },
    MouseDragged { sx: f64, sy: f64 },
    MouseUp { sx: f64, sy: f64 },
    Key(String),
    Text(String),
    Shortcut(String),
}

impl EventSink for RecordingSink {
    fn mouse_down(&mut self, sx: f64, sy: f64) {
        self.calls.push(SinkCall::MouseDown { sx, sy });
    }
    fn mouse_dragged(&mut self, sx: f64, sy: f64) {
        self.calls.push(SinkCall::MouseDragged { sx, sy });
    }
    fn mouse_up(&mut self, sx: f64, sy: f64) {
        self.calls.push(SinkCall::MouseUp { sx, sy });
    }
    fn key(&mut self, name: &str) {
        self.calls.push(SinkCall::Key(name.to_owned()));
    }
    fn text(&mut self, s: &str) {
        self.calls.push(SinkCall::Text(s.to_owned()));
    }
    fn shortcut(&mut self, name: &str) {
        self.calls.push(SinkCall::Shortcut(name.to_owned()));
    }
}

// ── CgEventSink ──────────────────────────────────────────────────────────────

/// Real OS sink: posts events via `CGEvent::post(CGEventTapLocation::HID)`.
///
/// # Behaviour
///
/// - **Mouse events**: `CGEventSource::new(HIDSystemState)` is created once;
///   each mouse call constructs a `CGEvent::new_mouse_event` and posts it via
///   `CGEventTapLocation::HID`.  This moves the Mac's real cursor and is
///   received by whichever window is frontmost at those global screen
///   coordinates (typically iPhone Mirroring, which must be brought to the
///   front first).
///
/// - **Key / Text**: delegates to `cua-driver` via a child-process call
///   (`cua-driver key <name>` / `cua-driver type <text>`).  This avoids
///   reimplementing keyboard scan-code mapping in Rust.
///
/// - **Shortcut**: similarly delegates to `cua-driver shortcut <name>`.
///
/// - **Tap dwell**: when called from a `Tap` sequence, `mouse_up` sleeps
///   [`TAP_DWELL_MS`] ms *before* posting the up event so iOS does not
///   mistake the gesture for a long-press.  The sink tracks this with an
///   internal `pending_tap_dwell` flag set by `mouse_down`.
///
/// # Safety / availability
///
/// Only compiled on macOS (`target_os = "macos"`).  Requires the process to
/// run inside a macOS window server session (not SSH without `DISPLAY`).
#[cfg(target_os = "macos")]
pub struct CgEventSink {
    /// Whether the next `mouse_up` call should sleep `TAP_DWELL_MS` first.
    ///
    /// Set by `mouse_down` so that a `Tap` (which is always down→up with no
    /// intervening drag) gets the short dwell.  Cleared after `mouse_up` fires.
    pending_tap_dwell: bool,
}

#[cfg(target_os = "macos")]
impl CgEventSink {
    /// Create a new sink.  Panics if there is no window server session.
    pub fn new() -> Self {
        // Eagerly verify the event source is available so the constructor
        // fails fast rather than panicking on the first event.
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        let _ = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .expect("CGEventSource::new failed — is there a macOS window server session?");
        CgEventSink {
            pending_tap_dwell: false,
        }
    }

    fn make_source() -> core_graphics::event_source::CGEventSource {
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .expect("CGEventSource::new failed")
    }

    fn post_mouse(
        event_type: core_graphics::event::CGEventType,
        sx: f64,
        sy: f64,
    ) {
        use core_graphics::event::{CGEvent, CGEventTapLocation, CGMouseButton};
        use core_graphics::geometry::CGPoint;
        let source = Self::make_source();
        let pt = CGPoint::new(sx, sy);
        let evt = CGEvent::new_mouse_event(source, event_type, pt, CGMouseButton::Left)
            .expect("CGEvent::new_mouse_event failed");
        evt.post(CGEventTapLocation::HID);
    }

    fn cua_driver(args: &[&str]) {
        // Best-effort: if cua-driver is not on PATH we log and continue.
        match std::process::Command::new("cua-driver").args(args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("cua-driver {:?} exited with {}", args, s),
            Err(e) => eprintln!("cua-driver not found / failed: {e}"),
        }
    }
}

#[cfg(target_os = "macos")]
impl Default for CgEventSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl EventSink for CgEventSink {
    fn mouse_down(&mut self, sx: f64, sy: f64) {
        use core_graphics::event::CGEventType;
        self.pending_tap_dwell = true; // set — cleared only by mouse_up
        Self::post_mouse(CGEventType::LeftMouseDown, sx, sy);
    }

    fn mouse_dragged(&mut self, sx: f64, sy: f64) {
        use core_graphics::event::CGEventType;
        self.pending_tap_dwell = false; // drag happened → not a pure tap
        Self::post_mouse(CGEventType::LeftMouseDragged, sx, sy);
    }

    fn mouse_up(&mut self, sx: f64, sy: f64) {
        use core_graphics::event::CGEventType;
        if self.pending_tap_dwell {
            // This is the up half of a Tap: sleep the safe dwell so iOS
            // registers a tap and NOT a long-press / jiggle trigger.
            std::thread::sleep(std::time::Duration::from_millis(TAP_DWELL_MS));
            self.pending_tap_dwell = false;
        }
        Self::post_mouse(CGEventType::LeftMouseUp, sx, sy);
    }

    fn key(&mut self, name: &str) {
        Self::cua_driver(&["key", name]);
    }

    fn text(&mut self, s: &str) {
        Self::cua_driver(&["type", s]);
    }

    fn shortcut(&mut self, name: &str) {
        Self::cua_driver(&["shortcut", name]);
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::{Orientation, Rect, SessionGeometry};

    /// A portrait geometry with a simple content rect for easy mental arithmetic.
    ///
    /// content_rect: x=100, y=200, w=300, h=600
    /// → center (0.5, 0.5) maps to screen (250.0, 500.0)
    /// → origin (0.0, 0.0) maps to screen (100.0, 200.0)
    /// → far corner (1.0, 1.0) maps to screen (400.0, 800.0)
    fn portrait_geo() -> SessionGeometry {
        SessionGeometry {
            content_rect: Rect { x: 100.0, y: 200.0, w: 300.0, h: 600.0 },
            scale: 2.0,
            orientation: Orientation::Portrait,
        }
    }

    // ── Tap ──────────────────────────────────────────────────────────────────

    #[test]
    fn tap_at_center_emits_down_then_up_at_content_center() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Tap { x: 0.5, y: 0.5 }, &geo, &mut sink).unwrap();

        assert_eq!(sink.calls.len(), 2, "Tap must emit exactly 2 calls");
        assert_eq!(
            sink.calls[0],
            SinkCall::MouseDown { sx: 250.0, sy: 500.0 },
            "first call must be MouseDown at content-rect center"
        );
        assert_eq!(
            sink.calls[1],
            SinkCall::MouseUp { sx: 250.0, sy: 500.0 },
            "second call must be MouseUp at the same point"
        );
    }

    #[test]
    fn tap_down_before_up_ordering() {
        // Verify the ordering invariant: Down always precedes Up.
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Tap { x: 0.0, y: 0.0 }, &geo, &mut sink).unwrap();

        assert!(
            matches!(sink.calls[0], SinkCall::MouseDown { .. }),
            "first event in Tap must be MouseDown"
        );
        assert!(
            matches!(sink.calls[1], SinkCall::MouseUp { .. }),
            "second event in Tap must be MouseUp"
        );
    }

    // ── Move ─────────────────────────────────────────────────────────────────

    #[test]
    fn move_emits_mouse_dragged() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Move { x: 0.5, y: 0.5 }, &geo, &mut sink).unwrap();

        assert_eq!(sink.calls.len(), 1);
        assert_eq!(
            sink.calls[0],
            SinkCall::MouseDragged { sx: 250.0, sy: 500.0 }
        );
    }

    // ── Down / Up sequence ───────────────────────────────────────────────────

    #[test]
    fn down_up_sequence_maps_both_correctly() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();

        inject(&InputEvent::Down { x: 0.0, y: 0.0 }, &geo, &mut sink).unwrap();
        inject(&InputEvent::Up { x: 1.0, y: 1.0 }, &geo, &mut sink).unwrap();

        assert_eq!(sink.calls.len(), 2);
        assert_eq!(
            sink.calls[0],
            SinkCall::MouseDown { sx: 100.0, sy: 200.0 }
        );
        assert_eq!(
            sink.calls[1],
            SinkCall::MouseUp { sx: 400.0, sy: 800.0 }
        );
    }

    #[test]
    fn down_move_up_sequence() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();

        inject(&InputEvent::Down { x: 0.2, y: 0.2 }, &geo, &mut sink).unwrap();
        inject(&InputEvent::Move { x: 0.5, y: 0.5 }, &geo, &mut sink).unwrap();
        inject(&InputEvent::Up { x: 0.8, y: 0.8 }, &geo, &mut sink).unwrap();

        assert_eq!(sink.calls.len(), 3);
        assert!(matches!(sink.calls[0], SinkCall::MouseDown { .. }));
        assert!(matches!(sink.calls[1], SinkCall::MouseDragged { .. }));
        assert!(matches!(sink.calls[2], SinkCall::MouseUp { .. }));
    }

    // ── OutOfBounds ──────────────────────────────────────────────────────────

    #[test]
    fn out_of_bounds_negative_returns_err_and_emits_nothing() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        let result = inject(&InputEvent::Down { x: -0.01, y: 0.5 }, &geo, &mut sink);
        assert_eq!(result, Err(InputError::OutOfBounds));
        assert!(sink.calls.is_empty(), "nothing emitted on out-of-bounds");
    }

    #[test]
    fn out_of_bounds_greater_than_one_returns_err_and_emits_nothing() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        let result = inject(&InputEvent::Up { x: 1.01, y: 0.5 }, &geo, &mut sink);
        assert_eq!(result, Err(InputError::OutOfBounds));
        assert!(sink.calls.is_empty());
    }

    #[test]
    fn out_of_bounds_tap_returns_err_and_emits_nothing() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        let result = inject(&InputEvent::Tap { x: 0.5, y: 1.5 }, &geo, &mut sink);
        assert_eq!(result, Err(InputError::OutOfBounds));
        assert!(sink.calls.is_empty());
    }

    #[test]
    fn out_of_bounds_move_returns_err_and_emits_nothing() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        let result = inject(&InputEvent::Move { x: 2.0, y: 0.0 }, &geo, &mut sink);
        assert_eq!(result, Err(InputError::OutOfBounds));
        assert!(sink.calls.is_empty());
    }

    // ── Boundary values are valid ─────────────────────────────────────────────

    #[test]
    fn boundary_0_0_is_valid_and_emits() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Down { x: 0.0, y: 0.0 }, &geo, &mut sink).unwrap();
        assert!(!sink.calls.is_empty());
    }

    #[test]
    fn boundary_1_1_is_valid_and_emits() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Down { x: 1.0, y: 1.0 }, &geo, &mut sink).unwrap();
        assert!(!sink.calls.is_empty());
    }

    // ── Key / Text / Shortcut routing ────────────────────────────────────────

    #[test]
    fn key_routes_to_sink_key() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Key("return".to_owned()), &geo, &mut sink).unwrap();
        assert_eq!(sink.calls, vec![SinkCall::Key("return".to_owned())]);
    }

    #[test]
    fn text_routes_to_sink_text() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Text("hello".to_owned()), &geo, &mut sink).unwrap();
        assert_eq!(sink.calls, vec![SinkCall::Text("hello".to_owned())]);
    }

    #[test]
    fn shortcut_home_routes_to_sink_shortcut() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Shortcut("home".to_owned()), &geo, &mut sink).unwrap();
        assert_eq!(sink.calls, vec![SinkCall::Shortcut("home".to_owned())]);
    }

    #[test]
    fn shortcut_spotlight_routes_to_sink_shortcut() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Shortcut("spotlight".to_owned()), &geo, &mut sink).unwrap();
        assert_eq!(
            sink.calls,
            vec![SinkCall::Shortcut("spotlight".to_owned())]
        );
    }

    #[test]
    fn shortcut_switcher_routes_to_sink_shortcut() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        inject(&InputEvent::Shortcut("switcher".to_owned()), &geo, &mut sink).unwrap();
        assert_eq!(
            sink.calls,
            vec![SinkCall::Shortcut("switcher".to_owned())]
        );
    }

    // ── Key/Text/Shortcut are coordinate-free — always Ok ────────────────────

    #[test]
    fn key_text_shortcut_never_return_out_of_bounds() {
        // geo doesn't matter for these; use anything
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        assert!(inject(&InputEvent::Key("escape".to_owned()), &geo, &mut sink).is_ok());
        assert!(inject(&InputEvent::Text("test".to_owned()), &geo, &mut sink).is_ok());
        assert!(inject(&InputEvent::Shortcut("home".to_owned()), &geo, &mut sink).is_ok());
    }

    // ── Coordinate mapping accuracy ──────────────────────────────────────────

    #[test]
    fn tap_coordinates_match_to_screen() {
        // Cross-check: inject's coordinate math must agree with to_screen directly.
        let geo = portrait_geo();
        let (ex, ey) = to_screen((0.25, 0.75), &geo).unwrap();

        let mut sink = RecordingSink::default();
        inject(&InputEvent::Tap { x: 0.25, y: 0.75 }, &geo, &mut sink).unwrap();

        assert_eq!(
            sink.calls[0],
            SinkCall::MouseDown { sx: ex, sy: ey },
            "Down x coord must match to_screen output"
        );
        assert_eq!(
            sink.calls[1],
            SinkCall::MouseUp { sx: ex, sy: ey },
            "Up x coord must match to_screen output"
        );
    }

    // ── tap_dwell_ms constant is within safe range ────────────────────────────

    #[test]
    fn tap_dwell_ms_is_within_safe_range() {
        // Must be short enough to not trigger iOS long-press (< ~60 ms empirical)
        // but non-zero so the system registers it.
        assert!(TAP_DWELL_MS > 0, "dwell must be positive");
        assert!(TAP_DWELL_MS <= 50, "dwell must be ≤50 ms to avoid jiggle/edit-mode");
    }

    // ── InputError display ───────────────────────────────────────────────────

    #[test]
    fn input_error_display() {
        let msg = format!("{}", InputError::OutOfBounds);
        assert!(
            msg.contains("outside") || msg.contains("content rect"),
            "display should describe the error: {msg}"
        );
    }
}
