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

/// Delay between consecutive synthetic keyboard events posted by [`CgEventSink::text`].
///
/// iPhone Mirroring drops keycodes posted back-to-back too fast (an 8 ms gap lost
/// digits in hardware testing — `Hello123` arrived as `hello13`). Each transition
/// — Shift-down, key-down, key-up, Shift-up — is separated by this gap so Mirroring
/// forwards every one.
pub const KEY_GAP_MS: u64 = 14;

/// Extra spacing inserted *between characters* in [`CgEventSink::text`], on top of
/// [`KEY_GAP_MS`], so Mirroring does not coalesce or drop adjacent key presses.
pub const CHAR_GAP_MS: u64 = 22;

/// Multiplier applied to client-reported pixel deltas before they become
/// scroll-wheel pixel units in [`CgEventSink::scroll`].
///
/// iPhone Mirroring scrolls only on scroll-wheel events — a finger swipe was
/// reaching it as a mouse-drag (long-press / icon-reorder, never a scroll). The
/// client sends per-move CSS-pixel deltas; this scales them to a natural-feeling
/// scroll. Tune for feel against the hardware.
///
/// Hardware feel check (Hermes): 1.6 scrolled ~a full screen per swipe (slightly
/// much); 1.3 lands closer to finger-tracking.
pub const SCROLL_SCALE: f64 = 1.3;

/// Sign of the vertical scroll axis. Flip if the phone scrolls the wrong way:
/// finger-up should scroll the content up (reveal what's below).
pub const SCROLL_DIR_V: f64 = 1.0;

/// Sign of the horizontal scroll axis. Flip if the phone pans the wrong way.
pub const SCROLL_DIR_H: f64 = 1.0;

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
    /// Scroll-wheel gesture: position the cursor at `(x, y)` then scroll by the
    /// pixel deltas `(dx, dy)`.
    ///
    /// `x`/`y` are normalized `[0, 1]` (the gesture anchor); `dx`/`dy` are the
    /// per-move CSS-pixel deltas reported by the client. iPhone Mirroring only
    /// scrolls on real scroll-wheel events, so swipes route here instead of
    /// through `Move` (which is a mouse-drag and triggers long-press/reorder).
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
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

    /// Scroll the view under the cursor at global screen point `(sx, sy)` by the
    /// pixel deltas `(dx, dy)`. The real sink positions the hardware cursor at
    /// `(sx, sy)` first (the scroll-wheel event applies at the cursor location)
    /// then posts a scroll-wheel event.
    fn scroll(&mut self, sx: f64, sy: f64, dx: f64, dy: f64);

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
        InputEvent::Scroll { x, y, dx, dy } => {
            // Only the anchor (x, y) is bounds-checked / mapped through geometry;
            // the deltas are pixel amounts, passed through untouched.
            let (sx, sy) = screen_or_err(*x, *y, geo)?;
            sink.scroll(sx, sy, *dx, *dy);
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
    Scroll { sx: f64, sy: f64, dx: f64, dy: f64 },
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
    fn scroll(&mut self, sx: f64, sy: f64, dx: f64, dy: f64) {
        self.calls.push(SinkCall::Scroll { sx, sy, dx, dy });
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
/// - **Text**: synthesized as CGEvent keyboard events posted to the HID tap.
///   iPhone Mirroring forwards the virtual KEYCODE to the phone, not the
///   Unicode payload nor modifier flags.  Capitals/symbols need a real Shift
///   key (keycode 56) pressed around them — `CGEventFlagShift` alone is
///   dropped by Mirroring.  The phone field must already be focused.
///
/// - **Key**: maps the named key to a macOS US-ANSI virtual keycode and posts
///   native CGEvent keyboard down+up to the HID tap — no external tooling
///   required.  iPhone Mirroring forwards the keycode to the phone.
///   Frontmost window is guaranteed by the injector layer before `key` is
///   called.  Hardware validation pending (phone currently offline).
///
/// - **Shortcut**: posts a real Command key (keycode 55) down, then the digit
///   key (home=18/'1', switcher=19/'2', spotlight=20/'3') down+up with
///   `CGEventFlagCommand` set on the digit events, then Command up.  These are
///   the iPhone Mirroring Mac-app View-menu shortcuts (⌘1/⌘2/⌘3), handled on
///   the Mac side — no external tooling required.  Each transition is separated
///   by [`KEY_GAP_MS`] (same pattern as `text()` Shift handling).  Frontmost
///   window is guaranteed by the injector layer.  Hardware validation pending
///   (phone currently offline).
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

/// Map a named shortcut to its macOS virtual keycode for the digit key (with ⌘).
///
/// These are the iPhone Mirroring Mac-side View-menu shortcuts (⌘1/⌘2/⌘3/⌘4).
/// The returned keycode is for the digit key only; the caller also posts a
/// real Command key (keycode 55) around it.  Pure — unit-tested.
///
/// - `"home"`         → ⌘1 (keycode 18) — go to Home Screen
/// - `"switcher"`     → ⌘2 (keycode 19) — open App Switcher
/// - `"spotlight"`    → ⌘3 (keycode 20) — open Spotlight Search
/// - `"controlcenter"` / `"control_center"` → ⌘4 (keycode 21) — open Control Center
///   (macOS 27+ only; harmless no-op on macOS 15/26 which have no ⌘4 binding)
fn shortcut_keymap(name: &str) -> Option<u16> {
    match name {
        "home"                             => Some(18), // '1'
        "switcher"                         => Some(19), // '2'
        "spotlight"                        => Some(20), // '3'
        "controlcenter" | "control_center" => Some(21), // '4' — macOS 27+
        _ => None,
    }
}

/// Map a named key to its macOS US-ANSI virtual keycode.
///
/// iPhone Mirroring forwards VIRTUAL KEYCODES to the phone (not Unicode),
/// so named keys must post real keycodes.  Frontmost window is guaranteed
/// by the injector layer before any key is sent.
///
/// Hardware validation for key() is pending (phone currently offline).
fn named_key_keycode(name: &str) -> Option<u16> {
    match name {
        "return" | "enter"      => Some(36),
        "escape" | "esc"        => Some(53),
        "space"                 => Some(49),
        "tab"                   => Some(48),
        "delete" | "backspace"  => Some(51),
        "up"                    => Some(126),
        "down"                  => Some(125),
        "left"                  => Some(123),
        "right"                 => Some(124),
        _ => None,
    }
}

/// Returns `true` when `s` should be sent via clipboard-paste instead of
/// char-by-char keycode synthesis.
///
/// If ANY character in `s` has no US-ANSI keycode (i.e. [`char_to_keycode`]
/// returns `None` for it) the whole string goes through the clipboard path.
/// Mixing keycode-typed and clipboard-pasted segments mid-string would reorder
/// text, so the decision is all-or-nothing per string.
///
/// Typical case: CJK / Emoji / accented characters — none have US keycodes.
/// The clipboard-paste path (pbcopy + real Cmd+V via iPhone Mirroring) bypasses
/// the on-phone Pinyin IME entirely, resolving the digit-hijack described in
/// GitHub issue #10.
///
/// Empty string → `false` (nothing to send either way).
fn needs_clipboard_paste(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.chars().any(|c| char_to_keycode(c).is_none())
}

/// US-ANSI virtual keycode + whether Shift is held, for a printable ASCII char.
/// `None` for chars we can't map (non-ASCII / CJK). iPhone Mirroring forwards the
/// keycode (not the Unicode payload), so text input must send real keycodes.
fn char_to_keycode(c: char) -> Option<(u16, bool)> {
    Some(match c {
        'a' => (0, false), 'b' => (11, false), 'c' => (8, false), 'd' => (2, false),
        'e' => (14, false), 'f' => (3, false), 'g' => (5, false), 'h' => (4, false),
        'i' => (34, false), 'j' => (38, false), 'k' => (40, false), 'l' => (37, false),
        'm' => (46, false), 'n' => (45, false), 'o' => (31, false), 'p' => (35, false),
        'q' => (12, false), 'r' => (15, false), 's' => (1, false), 't' => (17, false),
        'u' => (32, false), 'v' => (9, false), 'w' => (13, false), 'x' => (7, false),
        'y' => (16, false), 'z' => (6, false),
        'A' => (0, true), 'B' => (11, true), 'C' => (8, true), 'D' => (2, true),
        'E' => (14, true), 'F' => (3, true), 'G' => (5, true), 'H' => (4, true),
        'I' => (34, true), 'J' => (38, true), 'K' => (40, true), 'L' => (37, true),
        'M' => (46, true), 'N' => (45, true), 'O' => (31, true), 'P' => (35, true),
        'Q' => (12, true), 'R' => (15, true), 'S' => (1, true), 'T' => (17, true),
        'U' => (32, true), 'V' => (9, true), 'W' => (13, true), 'X' => (7, true),
        'Y' => (16, true), 'Z' => (6, true),
        '0' => (29, false), '1' => (18, false), '2' => (19, false), '3' => (20, false),
        '4' => (21, false), '5' => (23, false), '6' => (22, false), '7' => (26, false),
        '8' => (28, false), '9' => (25, false),
        ')' => (29, true), '!' => (18, true), '@' => (19, true), '#' => (20, true),
        '$' => (21, true), '%' => (23, true), '^' => (22, true), '&' => (26, true),
        '*' => (28, true), '(' => (25, true),
        ' ' => (49, false), '\n' => (36, false), '\t' => (48, false),
        '-' => (27, false), '_' => (27, true),
        '=' => (24, false), '+' => (24, true),
        '[' => (33, false), '{' => (33, true),
        ']' => (30, false), '}' => (30, true),
        '\\' => (42, false), '|' => (42, true),
        ';' => (41, false), ':' => (41, true),
        '\'' => (39, false), '"' => (39, true),
        ',' => (43, false), '<' => (43, true),
        '.' => (47, false), '>' => (47, true),
        '/' => (44, false), '?' => (44, true),
        '`' => (50, false), '~' => (50, true),
        _ => return None,
    })
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

    /// Create a CGEventSource, returning Err on failure. Per-event calls use
    /// this non-panicking form so a transient window-server hiccup (display
    /// sleep, fast user switch) doesn't kill the injector thread permanently.
    fn make_source() -> Result<core_graphics::event_source::CGEventSource, String> {
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| "CGEventSource::new failed".to_string())
    }

    /// Post one mouse event. On failure, logs and drops the event; never panics.
    fn post_mouse(
        event_type: core_graphics::event::CGEventType,
        sx: f64,
        sy: f64,
    ) {
        use core_graphics::event::{CGEvent, CGEventTapLocation, CGMouseButton};
        use core_graphics::geometry::CGPoint;
        let source = match Self::make_source() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("post_mouse: {e}; dropping event");
                return;
            }
        };
        let pt = CGPoint::new(sx, sy);
        if let Ok(evt) =
            CGEvent::new_mouse_event(source, event_type, pt, CGMouseButton::Left)
        {
            evt.post(CGEventTapLocation::HID);
        } else {
            eprintln!("post_mouse: CGEvent::new_mouse_event failed; dropping event");
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

    fn scroll(&mut self, sx: f64, sy: f64, dx: f64, dy: f64) {
        use core_graphics::event::{
            CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
        };
        use core_graphics::geometry::CGPoint;
        // The scroll-wheel event applies at the *current* hardware cursor
        // location, and Mirroring scrolls the phone only when the cursor is
        // inside its window — so move the cursor to the gesture anchor first
        // (MouseMoved, no button held; this does not trigger a tap/long-press).
        if let Ok(source) = Self::make_source() {
            if let Ok(moved) = CGEvent::new_mouse_event(
                source,
                CGEventType::MouseMoved,
                CGPoint::new(sx, sy),
                CGMouseButton::Left,
            ) {
                moved.post(CGEventTapLocation::HID);
            }
        } else {
            eprintln!("scroll: CGEventSource unavailable; dropping move-cursor step");
        }
        let wheel1 = (dy * SCROLL_SCALE * SCROLL_DIR_V) as i32; // vertical
        let wheel2 = (dx * SCROLL_SCALE * SCROLL_DIR_H) as i32; // horizontal
        if wheel1 == 0 && wheel2 == 0 {
            return;
        }
        if let Ok(source) = Self::make_source() {
            if let Ok(scroll) = CGEvent::new_scroll_event(
                source,
                ScrollEventUnit::PIXEL,
                2, // wheel_count: vertical + horizontal
                wheel1,
                wheel2,
                0,
            ) {
                scroll.post(CGEventTapLocation::HID);
            }
        } else {
            eprintln!("scroll: CGEventSource unavailable; dropping scroll event");
        }
    }

    fn key(&mut self, name: &str) {
        // iPhone Mirroring forwards virtual KEYCODES to the phone — not Unicode,
        // not modifier flags.  Post a real keycode down+up to the HID tap.
        // Frontmost window is guaranteed by the injector layer before this call.
        // Hardware validation is pending (phone currently offline).
        use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
        let kc = match named_key_keycode(name) {
            Some(k) => k,
            None => {
                eprintln!("key {name:?}: unknown key name; skipping");
                return;
            }
        };
        let gap = || std::thread::sleep(std::time::Duration::from_millis(KEY_GAP_MS));
        if let Ok(source) = Self::make_source() {
            if let Ok(down) = CGEvent::new_keyboard_event(source, kc, true) {
                down.set_flags(CGEventFlags::empty());
                down.post(CGEventTapLocation::HID);
            }
        } else {
            eprintln!("key {name:?}: CGEventSource unavailable; skipping");
            return;
        }
        gap();
        if let Ok(source) = Self::make_source() {
            if let Ok(up) = CGEvent::new_keyboard_event(source, kc, false) {
                up.set_flags(CGEventFlags::empty());
                up.post(CGEventTapLocation::HID);
            }
        } else {
            eprintln!("key {name:?}: CGEventSource unavailable; skipping up");
        }
        gap();
    }

    fn text(&mut self, s: &str) {
        // iPhone Mirroring forwards the key's VIRTUAL KEYCODE to the phone, NOT
        // the CGEvent Unicode payload AND NOT the modifier *flags* — both are
        // dropped on the Mirroring boundary. So:
        //   1. post the real keycode per char (Unicode is ignored → keycode 0 = 'a');
        //   2. press a real Shift KEY (keycode 56) around shifted chars — relying on
        //      `CGEventFlagShift` alone made capitals arrive lowercase (`H` → `h`);
        //   3. space every transition by KEY_GAP_MS / CHAR_GAP_MS — events posted
        //      back-to-back too fast get dropped (`Hello123` → `hello13`, the `2` lost).
        // The phone field must already be focused.
        //
        // CJK / non-ASCII path (GitHub issue #10):
        //   If ANY character in `s` has no US keycode, the WHOLE string is sent via
        //   clipboard-paste instead of char-by-char synthesis.  Mixing both modes
        //   mid-string would reorder text; the decision is all-or-nothing per call.
        //   Clipboard-paste bypasses the on-phone Pinyin IME entirely, which prevents
        //   the digit-hijack problem (#10 — IME intercepts digits like "1" to select
        //   a candidate rather than inserting "1").
        //   Implementation: write to the Mac clipboard via `/usr/bin/pbcopy` (spawn +
        //   write stdin + wait — simplest approach, zero new dependencies; avoids
        //   NSPasteboard FFI which would require objc/core-foundation crates not yet in
        //   the workspace), sleep ~50 ms for clipboard to settle, then post a REAL
        //   Cmd+V: Command key (keycode 55) down, 'v' (keycode 9) down+up with
        //   CGEventFlagCommand, Command up — identical to the shortcut() real-modifier
        //   pattern.  iPhone Mirroring drops modifier flags; the real Cmd key is required.
        //   NOTE: the Mac clipboard is clobbered; not restored (acceptable trade-off;
        //   clipboard restore is left as a future enhancement).
        //   Hardware validation pending (targets the Pinyin digit-hijack in issue #10).
        // iPhone Mirroring forwards synthetic keycodes UNRELIABLY — issue #15:
        // even plain ASCII (an IP, a port) sometimes never lands. Clipboard
        // paste (real Cmd+V) is the one input method Mirroring accepts
        // dependably, so it is the DEFAULT for all text on this L3 path, and it
        // is now non-destructive (the Mac clipboard is saved + restored around
        // the paste). When WDA is up the daemon types via WDA's clean on-device
        // path and never reaches here. `PHONE_REMOTE_TEXT_KEYCODE=1` forces the
        // legacy char-by-char keycode path (still needed for CJK, which has no
        // US keycodes and always goes clipboard regardless).
        let force_keycode =
            std::env::var("PHONE_REMOTE_TEXT_KEYCODE").is_ok_and(|v| v == "1");
        if !s.is_empty() && (!force_keycode || needs_clipboard_paste(s)) {
            self.text_via_clipboard(s);
            return;
        }

        use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
        const KVK_SHIFT: u16 = 56;
        let gap = || std::thread::sleep(std::time::Duration::from_millis(KEY_GAP_MS));
        for ch in s.chars() {
            let (kc, shift) = match char_to_keycode(ch) {
                Some(v) => v,
                None => {
                    // Should not reach here after needs_clipboard_paste check,
                    // but guard defensively.
                    eprintln!("text: no US keycode for {ch:?}; skipping (non-ASCII?)");
                    continue;
                }
            };
            let flags = if shift {
                CGEventFlags::CGEventFlagShift
            } else {
                CGEventFlags::empty()
            };
            // Hold a real Shift key down for shifted chars — the flag alone is
            // dropped by Mirroring, so capitals/symbols need the physical key.
            if shift {
                match Self::make_source() {
                    Ok(source) => {
                        if let Ok(sd) = CGEvent::new_keyboard_event(source, KVK_SHIFT, true) {
                            sd.set_flags(CGEventFlags::CGEventFlagShift);
                            sd.post(CGEventTapLocation::HID);
                        }
                    }
                    Err(e) => { eprintln!("text: {e}; skipping shift-down for {ch:?}"); continue; }
                }
                gap();
            }
            match Self::make_source() {
                Ok(source) => {
                    if let Ok(down) = CGEvent::new_keyboard_event(source, kc, true) {
                        down.set_flags(flags);
                        down.post(CGEventTapLocation::HID);
                    }
                }
                Err(e) => { eprintln!("text: {e}; skipping key-down for {ch:?}"); continue; }
            }
            gap();
            match Self::make_source() {
                Ok(source) => {
                    if let Ok(up) = CGEvent::new_keyboard_event(source, kc, false) {
                        up.set_flags(flags);
                        up.post(CGEventTapLocation::HID);
                    }
                }
                Err(e) => eprintln!("text: {e}; skipping key-up for {ch:?}"),
            }
            gap();
            if shift {
                match Self::make_source() {
                    Ok(source) => {
                        if let Ok(su) = CGEvent::new_keyboard_event(source, KVK_SHIFT, false) {
                            su.set_flags(CGEventFlags::empty());
                            su.post(CGEventTapLocation::HID);
                        }
                    }
                    Err(e) => eprintln!("text: {e}; skipping shift-up for {ch:?}"),
                }
                gap();
            }
            // Inter-char spacing on top of the per-event gap so Mirroring does not
            // coalesce or drop adjacent presses.
            std::thread::sleep(std::time::Duration::from_millis(CHAR_GAP_MS));
        }
    }

    fn shortcut(&mut self, name: &str) {
        // iPhone Mirroring's View-menu shortcuts (⌘1/⌘2/⌘3) are handled on the
        // Mac side — post a real Command key (keycode 55) around the digit key.
        // CGEventFlagCommand alone is dropped by Mirroring; the physical Cmd key
        // must be held via its own keycode, exactly like Shift in text().
        // Frontmost window is guaranteed by the injector layer before this call.
        // Hardware validation is pending (phone currently offline).
        use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
        const KVK_COMMAND: u16 = 55;
        let digit_kc = match shortcut_keymap(name) {
            Some(k) => k,
            None => {
                eprintln!(
                    "shortcut {name:?}: unknown shortcut \
                     (want home|switcher|spotlight|controlcenter)"
                );
                return;
            }
        };
        let gap = || std::thread::sleep(std::time::Duration::from_millis(KEY_GAP_MS));
        // Command key down
        match Self::make_source() {
            Ok(source) => {
                if let Ok(cmd_down) = CGEvent::new_keyboard_event(source, KVK_COMMAND, true) {
                    cmd_down.set_flags(CGEventFlags::CGEventFlagCommand);
                    cmd_down.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => { eprintln!("shortcut {name:?}: {e}; skipping"); return; }
        }
        gap();
        // Digit key down (⌘ held)
        match Self::make_source() {
            Ok(source) => {
                if let Ok(digit_down) = CGEvent::new_keyboard_event(source, digit_kc, true) {
                    digit_down.set_flags(CGEventFlags::CGEventFlagCommand);
                    digit_down.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => eprintln!("shortcut {name:?}: {e}; skipping digit-down"),
        }
        gap();
        // Digit key up (⌘ still held)
        match Self::make_source() {
            Ok(source) => {
                if let Ok(digit_up) = CGEvent::new_keyboard_event(source, digit_kc, false) {
                    digit_up.set_flags(CGEventFlags::CGEventFlagCommand);
                    digit_up.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => eprintln!("shortcut {name:?}: {e}; skipping digit-up"),
        }
        gap();
        // Command key up
        match Self::make_source() {
            Ok(source) => {
                if let Ok(cmd_up) = CGEvent::new_keyboard_event(source, KVK_COMMAND, false) {
                    cmd_up.set_flags(CGEventFlags::empty());
                    cmd_up.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => eprintln!("shortcut {name:?}: {e}; skipping cmd-up"),
        }
        gap();
    }
}

// ── CgEventSink helpers (not part of the EventSink trait) ────────────────────

#[cfg(target_os = "macos")]
impl CgEventSink {
    /// Send `s` to the phone by writing it to the Mac clipboard and posting a
    /// real Cmd+V.  Called by [`EventSink::text`] when the string contains
    /// non-ASCII chars that have no US keycode.
    ///
    /// Steps:
    ///   1. Spawn `/usr/bin/pbcopy`, write `s` to its stdin, wait for it to exit.
    ///   2. Sleep 50 ms so the clipboard is settled before Mirroring reads it.
    ///   3. Post: Command-down (kc 55), 'v'-down (kc 9, with CGEventFlagCommand),
    ///      'v'-up, Command-up — the same real-modifier pattern as `shortcut()`.
    ///
    /// Uses `pbcopy` (rather than NSPasteboard FFI) to avoid pulling in
    /// `objc`/`core-foundation` crates that are not yet in the workspace.
    ///
    /// The Mac clipboard is clobbered and not restored; this is an acceptable
    /// trade-off for the initial implementation (issue #10).
    fn text_via_clipboard(&self, s: &str) {
        use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
        use std::io::Write;
        use std::process::{Command, Stdio};

        // 0. Save the current clipboard so the paste is non-destructive — the
        // user's clipboard is theirs, and an agent typing should not clobber it.
        // pbpaste captures TEXT only (image/file clipboards aren't preserved —
        // an acceptable edge), empty string if the clipboard is empty/non-text.
        let saved = Command::new("/usr/bin/pbpaste")
            .output()
            .ok()
            .map(|o| o.stdout);

        // 1. Write to clipboard via pbcopy
        match Command::new("/usr/bin/pbcopy")
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.take() {
                    let mut stdin = stdin;
                    if let Err(e) = stdin.write_all(s.as_bytes()) {
                        eprintln!("text_via_clipboard: pbcopy stdin write failed: {e}; skipping");
                        return;
                    }
                }
                if let Err(e) = child.wait() {
                    eprintln!("text_via_clipboard: pbcopy wait failed: {e}; skipping");
                    return;
                }
            }
            Err(e) => {
                eprintln!("text_via_clipboard: failed to spawn pbcopy: {e}; skipping");
                return;
            }
        }

        // 2. Wait for clipboard to settle
        std::thread::sleep(std::time::Duration::from_millis(50));

        // 3. Post real Cmd+V to HID tap — same real-modifier pattern as shortcut()
        const KVK_COMMAND: u16 = 55;
        const KVK_V: u16 = 9;
        let gap = || std::thread::sleep(std::time::Duration::from_millis(KEY_GAP_MS));

        // Command key down
        match Self::make_source() {
            Ok(source) => {
                if let Ok(cmd_down) = CGEvent::new_keyboard_event(source, KVK_COMMAND, true) {
                    cmd_down.set_flags(CGEventFlags::CGEventFlagCommand);
                    cmd_down.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => { eprintln!("text_via_clipboard: {e}; skipping cmd-down"); return; }
        }
        gap();
        // 'v' key down (⌘ held)
        match Self::make_source() {
            Ok(source) => {
                if let Ok(v_down) = CGEvent::new_keyboard_event(source, KVK_V, true) {
                    v_down.set_flags(CGEventFlags::CGEventFlagCommand);
                    v_down.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => eprintln!("text_via_clipboard: {e}; skipping v-down"),
        }
        gap();
        // 'v' key up (⌘ still held)
        match Self::make_source() {
            Ok(source) => {
                if let Ok(v_up) = CGEvent::new_keyboard_event(source, KVK_V, false) {
                    v_up.set_flags(CGEventFlags::CGEventFlagCommand);
                    v_up.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => eprintln!("text_via_clipboard: {e}; skipping v-up"),
        }
        gap();
        // Command key up
        match Self::make_source() {
            Ok(source) => {
                if let Ok(cmd_up) = CGEvent::new_keyboard_event(source, KVK_COMMAND, false) {
                    cmd_up.set_flags(CGEventFlags::empty());
                    cmd_up.post(CGEventTapLocation::HID);
                }
            }
            Err(e) => eprintln!("text_via_clipboard: {e}; skipping cmd-up"),
        }
        gap();

        // 4. Restore the saved clipboard. Wait for the paste to be consumed by
        // the phone first (the Cmd+V is async over the Mirroring boundary), then
        // put the user's original content back via pbcopy.
        if let Some(prev) = saved {
            std::thread::sleep(std::time::Duration::from_millis(120));
            if let Ok(mut child) = Command::new("/usr/bin/pbcopy").stdin(Stdio::piped()).spawn() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&prev);
                }
                let _ = child.wait();
            }
        }
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

    // ── Scroll ─────────────────────────────────────────────────────────────────

    #[test]
    fn scroll_maps_anchor_through_geometry_and_passes_deltas() {
        let geo = portrait_geo(); // content_rect x=100,y=200,w=300,h=600
        let mut sink = RecordingSink::default();
        inject(
            &InputEvent::Scroll { x: 0.5, y: 0.5, dx: 3.0, dy: -7.0 },
            &geo,
            &mut sink,
        )
        .unwrap();

        assert_eq!(sink.calls.len(), 1, "Scroll must emit exactly 1 call");
        assert_eq!(
            sink.calls[0],
            SinkCall::Scroll { sx: 250.0, sy: 500.0, dx: 3.0, dy: -7.0 },
            "anchor mapped to content center, deltas passed through untouched"
        );
    }

    #[test]
    fn scroll_out_of_bounds_anchor_emits_nothing() {
        let geo = portrait_geo();
        let mut sink = RecordingSink::default();
        let err = inject(
            &InputEvent::Scroll { x: 1.5, y: 0.5, dx: 0.0, dy: -5.0 },
            &geo,
            &mut sink,
        );
        assert_eq!(err, Err(InputError::OutOfBounds));
        assert!(sink.calls.is_empty(), "nothing emitted on out-of-bounds anchor");
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

    // ── shortcut_keymap — native CGEvent keycode mapping ────────────────────

    #[test]
    fn shortcut_keymap_matches_iphone_act() {
        // home=⌘1, switcher=⌘2, spotlight=⌘3 — iPhone Mirroring View-menu shortcuts
        assert_eq!(shortcut_keymap("home"),      Some(18_u16)); // '1' keycode
        assert_eq!(shortcut_keymap("switcher"),  Some(19_u16)); // '2' keycode
        assert_eq!(shortcut_keymap("spotlight"), Some(20_u16)); // '3' keycode
        assert_eq!(shortcut_keymap("nope"),      None);
    }

    #[test]
    fn shortcut_keymap_controlcenter_both_spellings() {
        // controlcenter = ⌘4 (macOS 27+ Mirroring View menu)
        // Both spellings must resolve to keycode 21 ('4')
        assert_eq!(shortcut_keymap("controlcenter"),  Some(21_u16));
        assert_eq!(shortcut_keymap("control_center"), Some(21_u16));
    }

    // ── needs_clipboard_paste — partition helper ─────────────────────────────

    #[test]
    fn needs_clipboard_paste_pure_ascii_is_false() {
        assert!(!needs_clipboard_paste("hello"));
        assert!(!needs_clipboard_paste("Hello, World!"));
        assert!(!needs_clipboard_paste("abc123!@#"));
    }

    #[test]
    fn needs_clipboard_paste_chinese_is_true() {
        assert!(needs_clipboard_paste("中文"));
        assert!(needs_clipboard_paste("你好"));
    }

    #[test]
    fn needs_clipboard_paste_mixed_is_true() {
        // Even a single non-ASCII char triggers the clipboard path for the whole string
        assert!(needs_clipboard_paste("hello中文"));
        assert!(needs_clipboard_paste("test你好world"));
    }

    #[test]
    fn needs_clipboard_paste_empty_is_false() {
        assert!(!needs_clipboard_paste(""));
    }

    // ── named_key_keycode — named key → macOS virtual keycode ───────────────

    #[test]
    fn named_key_keycode_covers_required_keys() {
        assert_eq!(named_key_keycode("return"),    Some(36_u16));
        assert_eq!(named_key_keycode("enter"),     Some(36_u16)); // alias
        assert_eq!(named_key_keycode("escape"),    Some(53_u16));
        assert_eq!(named_key_keycode("esc"),       Some(53_u16)); // alias
        assert_eq!(named_key_keycode("space"),     Some(49_u16));
        assert_eq!(named_key_keycode("tab"),       Some(48_u16));
        assert_eq!(named_key_keycode("delete"),    Some(51_u16));
        assert_eq!(named_key_keycode("backspace"), Some(51_u16)); // alias
        assert_eq!(named_key_keycode("up"),        Some(126_u16));
        assert_eq!(named_key_keycode("down"),      Some(125_u16));
        assert_eq!(named_key_keycode("left"),      Some(123_u16));
        assert_eq!(named_key_keycode("right"),     Some(124_u16));
        assert_eq!(named_key_keycode("unknown"),   None);
        assert_eq!(named_key_keycode(""),          None);
    }

    #[test]
    fn named_key_keycode_arrow_keys_are_distinct() {
        let up    = named_key_keycode("up").unwrap();
        let down  = named_key_keycode("down").unwrap();
        let left  = named_key_keycode("left").unwrap();
        let right = named_key_keycode("right").unwrap();
        // All four arrow key codes must be different from each other
        assert_ne!(up, down);
        assert_ne!(up, left);
        assert_ne!(up, right);
        assert_ne!(down, left);
        assert_ne!(down, right);
        assert_ne!(left, right);
    }

    #[test]
    fn char_to_keycode_covers_ascii() {
        assert_eq!(char_to_keycode('a'), Some((0, false)));
        assert_eq!(char_to_keycode('A'), Some((0, true)));
        assert_eq!(char_to_keycode('1'), Some((18, false)));
        assert_eq!(char_to_keycode('!'), Some((18, true))); // shift+1
        assert_eq!(char_to_keycode(' '), Some((49, false)));
        assert_eq!(char_to_keycode('?'), Some((44, true))); // shift+/
        assert_eq!(char_to_keycode('-'), Some((27, false)));
        assert_eq!(char_to_keycode('_'), Some((27, true)));
        assert_eq!(char_to_keycode('中'), None); // non-ASCII → skipped
    }

}
