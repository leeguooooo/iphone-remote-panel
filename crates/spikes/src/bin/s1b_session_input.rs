//! Spike S1b — input injection via the SESSION / HID event tap.
//!
//! S1 (s1_input) used `CGEvent::post_to_pid` and the iPhone Mirroring window
//! ignored every event, while the proven `iphone-act`/`cua-driver` path works.
//! Hypothesis: iPhone Mirroring (com.apple.ScreenContinuity) forwards only input
//! it observes on the system **session/HID event tap** at the global cursor
//! location — NOT events injected into a process's private queue via
//! CGEventPostToPid. This probe changes exactly ONE variable: the post target,
//! from `post_to_pid(pid)` to `post(CGEventTapLocation::HID|Session)` at the
//! same global screen coordinate.
//!
//! Because these are real session-level events, they MOVE THE MAC'S REAL CURSOR
//! and only land on the Mirroring window when it is FRONTMOST and the (x, y)
//! point is inside its content. Bring iPhone Mirroring to the front first.
//!
//! Usage:
//!   s1b_session_input <x> <y> [hid|session]
//!     x, y  — global screen point (logical pixels, top-left origin),
//!             inside the iPhone Mirroring window content.
//!     tap   — which tap location to post to (default: hid).
//!
//! Sends mouseDown@(x,y) -> 5 dragged steps moving 4px down each -> mouseUp.

use core_graphics::event::{CGEvent, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::event::CGEventTapLocation;
use core_graphics::geometry::CGPoint;
use std::thread;
use std::time::Duration;

fn post_mouse(
    source: &CGEventSource,
    event_type: CGEventType,
    x: f64,
    y: f64,
    tap: CGEventTapLocation,
    label: &str,
) {
    let pt = CGPoint::new(x, y);
    let evt = CGEvent::new_mouse_event(source.clone(), event_type, pt, CGMouseButton::Left)
        .expect("CGEvent::new_mouse_event failed");
    println!("posting {label} at ({x}, {y}) via {tap:?} tap");
    evt.post(tap);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("usage: s1b_session_input <x> <y> [hid|session]");
        eprintln!("  bring iPhone Mirroring to the FRONT first; (x,y) is a global");
        eprintln!("  screen point inside its window content.");
        std::process::exit(1);
    }

    let x: f64 = args[1].parse().expect("x must be a number");
    let y: f64 = args[2].parse().expect("y must be a number");
    let tap = match args.get(3).map(String::as_str) {
        Some("session") => CGEventTapLocation::Session,
        Some("hid") | None => CGEventTapLocation::HID,
        Some(other) => {
            eprintln!("unknown tap location {other:?}; use 'hid' or 'session'");
            std::process::exit(1);
        }
    };

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .expect("CGEventSource::new failed — running without a window server?");

    post_mouse(&source, CGEventType::LeftMouseDown, x, y, tap, "mouseDown");
    thread::sleep(Duration::from_millis(16));

    const STEPS: u8 = 5;
    const STEP_PX: f64 = 4.0;
    for i in 1..=STEPS {
        let dy = y + f64::from(i) * STEP_PX;
        post_mouse(&source, CGEventType::LeftMouseDragged, x, dy, tap, "mouseDragged");
        thread::sleep(Duration::from_millis(16));
    }

    let y_end = y + f64::from(STEPS) * STEP_PX;
    post_mouse(&source, CGEventType::LeftMouseUp, x, y_end, tap, "mouseUp");

    println!("done — gesture complete (tap={tap:?})");
}
