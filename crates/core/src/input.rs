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
pub const SCROLL_SCALE: f64 = 1.6;

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
/// - **Text**: synthesized as CGEvent keyboard events with the Unicode string
///   set (`CGEventKeyboardSetUnicodeString`), posted to the HID tap — the same
///   path the mouse uses, which Mirroring forwards to the phone. (cua-driver
///   `type_text` inserts into the Mac window's AX tree, which the phone never
///   receives.) The phone field must already be focused.
///
/// - **Key**: a named key delegates to `cua-driver call press_key` (cua-driver
///   0.5.x has NO top-level `key`/`type`/`shortcut` subcommand — only `call`).
///
/// - **Shortcut**: mapped to the proven `iphone-act` hotkeys and sent via
///   `cua-driver call press_key` — `home` = ⌘1, `switcher` = ⌘2, `spotlight` = ⌘3.
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
    /// Path to the `cua-driver` binary used for key/text/shortcut injection.
    cua_driver: String,
    /// `(pid, window_id)` of the Mirroring window, for cua-driver `call` tools.
    /// `None` → key/text/shortcut are logged and skipped (the pointer path,
    /// which uses CGEvent directly, still works without a target).
    target: Option<(i32, u32)>,
}

/// Map a named shortcut to the cua-driver `press_key` key (always with ⌘).
/// Matches the proven `iphone-act` mapping. Pure — unit-tested.
fn shortcut_keymap(name: &str) -> Option<&'static str> {
    match name {
        "home" => Some("1"),
        "switcher" => Some("2"),
        "spotlight" => Some("3"),
        _ => None,
    }
}

/// Minimal JSON string escaping for cua-driver `call` payloads.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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

/// Build the JSON for `cua-driver call press_key`.
fn press_key_json(pid: i32, window_id: u32, key: &str, cmd: bool) -> String {
    let mods = if cmd { r#","modifiers":["cmd"]"# } else { "" };
    format!(
        r#"{{"pid":{pid},"window_id":{window_id},"key":"{}"{mods}}}"#,
        json_escape(key)
    )
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
            cua_driver: "cua-driver".to_string(),
            target: None,
        }
    }

    /// Create a sink wired to the Mirroring window's `pid` + `window_id` and a
    /// `cua-driver` binary path, so key/text/shortcut injection works.
    pub fn with_cua(cua_driver: impl Into<String>, pid: i32, window_id: u32) -> Self {
        let mut s = Self::new();
        s.cua_driver = cua_driver.into();
        s.target = Some((pid, window_id));
        s
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

    /// Run `cua-driver call <tool> '<json>'`. Best-effort; logs on failure.
    fn cua_call(&self, tool: &str, json: &str) {
        match std::process::Command::new(&self.cua_driver)
            .args(["call", tool, json])
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("cua-driver call {tool} exited {s}"),
            Err(e) => eprintln!("cua-driver ({}) not found / failed: {e}", self.cua_driver),
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
        if let Ok(moved) = CGEvent::new_mouse_event(
            Self::make_source(),
            CGEventType::MouseMoved,
            CGPoint::new(sx, sy),
            CGMouseButton::Left,
        ) {
            moved.post(CGEventTapLocation::HID);
        }
        let wheel1 = (dy * SCROLL_SCALE * SCROLL_DIR_V) as i32; // vertical
        let wheel2 = (dx * SCROLL_SCALE * SCROLL_DIR_H) as i32; // horizontal
        if wheel1 == 0 && wheel2 == 0 {
            return;
        }
        if let Ok(scroll) = CGEvent::new_scroll_event(
            Self::make_source(),
            ScrollEventUnit::PIXEL,
            2, // wheel_count: vertical + horizontal
            wheel1,
            wheel2,
            0,
        ) {
            scroll.post(CGEventTapLocation::HID);
        }
    }

    fn key(&mut self, name: &str) {
        match self.target {
            Some((pid, wid)) => self.cua_call("press_key", &press_key_json(pid, wid, name, false)),
            None => eprintln!("key {name:?}: no Mirroring window target; skipping"),
        }
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
        // The phone field must already be focused. Non-ASCII (CJK) has no US keycode
        // and is skipped — that needs an on-phone IME, out of scope here.
        use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
        const KVK_SHIFT: u16 = 56;
        let gap = || std::thread::sleep(std::time::Duration::from_millis(KEY_GAP_MS));
        for ch in s.chars() {
            let (kc, shift) = match char_to_keycode(ch) {
                Some(v) => v,
                None => {
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
                if let Ok(sd) = CGEvent::new_keyboard_event(Self::make_source(), KVK_SHIFT, true) {
                    sd.set_flags(CGEventFlags::CGEventFlagShift);
                    sd.post(CGEventTapLocation::HID);
                }
                gap();
            }
            if let Ok(down) = CGEvent::new_keyboard_event(Self::make_source(), kc, true) {
                down.set_flags(flags);
                down.post(CGEventTapLocation::HID);
            }
            gap();
            if let Ok(up) = CGEvent::new_keyboard_event(Self::make_source(), kc, false) {
                up.set_flags(flags);
                up.post(CGEventTapLocation::HID);
            }
            gap();
            if shift {
                if let Ok(su) = CGEvent::new_keyboard_event(Self::make_source(), KVK_SHIFT, false) {
                    su.set_flags(CGEventFlags::empty());
                    su.post(CGEventTapLocation::HID);
                }
                gap();
            }
            // Inter-char spacing on top of the per-event gap so Mirroring does not
            // coalesce or drop adjacent presses.
            std::thread::sleep(std::time::Duration::from_millis(CHAR_GAP_MS));
        }
    }

    fn shortcut(&mut self, name: &str) {
        match (shortcut_keymap(name), self.target) {
            (Some(key), Some((pid, wid))) => {
                self.cua_call("press_key", &press_key_json(pid, wid, key, true))
            }
            (None, _) => eprintln!("unknown shortcut {name:?} (want home|spotlight|switcher)"),
            (Some(_), None) => eprintln!("shortcut {name:?}: no Mirroring window target; skipping"),
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

    // ── cua-driver invocation builders (the bug Hermes found) ────────────────

    #[test]
    fn shortcut_keymap_matches_iphone_act() {
        assert_eq!(shortcut_keymap("home"), Some("1"));
        assert_eq!(shortcut_keymap("switcher"), Some("2"));
        assert_eq!(shortcut_keymap("spotlight"), Some("3"));
        assert_eq!(shortcut_keymap("nope"), None);
    }

    #[test]
    fn press_key_json_shape() {
        // spotlight = ⌘3 on the Mirroring window
        assert_eq!(
            press_key_json(40374, 353, "3", true),
            r#"{"pid":40374,"window_id":353,"key":"3","modifiers":["cmd"]}"#
        );
        // a plain key (no cmd)
        assert_eq!(
            press_key_json(7, 9, "a", false),
            r#"{"pid":7,"window_id":9,"key":"a"}"#
        );
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

    #[test]
    fn json_escape_control_chars() {
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("\"\\"), "\\\"\\\\");
    }
}
