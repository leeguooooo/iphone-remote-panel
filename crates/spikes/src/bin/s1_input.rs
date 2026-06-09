//! Spike S1 — CGEventPostToPid input-injection probe
//!
//! Purpose: validate whether CGEventPostToPid delivers mouse events to an
//! iPhone Mirroring window without requiring it to be frontmost.
//!
//! Usage:
//!   s1_input <pid> <x> <y>
//!
//! Sends a left-button mouseDown at (x, y), then five LeftMouseDragged steps
//! moving 20 px downward (4 px per step), then mouseUp — all posted directly
//! to the named pid via CGEventPostToPid.
//!
//! Coordinate-space assumption (CONFIRM on hardware as part of S1 verdict):
//!   x and y are **global screen points in the macOS coordinate system**,
//!   i.e. top-left of the primary display is (0, 0) and y increases downward.
//!   On a Retina display pass LOGICAL points, not physical pixels.
//!   If iPhone Mirroring uses a different coordinate space (e.g. relative to
//!   the window's content rect) this probe will still compile but the gesture
//!   will land at the wrong location — note this in the S1 result.
//!
//! Event source: CGEventSourceStateID::HIDSystemState is used so the
//! synthesised events look like real HID hardware to the receiving process.

use core_graphics::event::{CGEvent, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::thread;
use std::time::Duration;

fn post_mouse(
    source: &CGEventSource,
    event_type: CGEventType,
    x: f64,
    y: f64,
    pid: i32,
    label: &str,
) {
    let pt = CGPoint::new(x, y);
    let evt = CGEvent::new_mouse_event(source.clone(), event_type, pt, CGMouseButton::Left)
        .expect("CGEvent::new_mouse_event failed");
    println!("posting {} at ({}, {}) to pid {}", label, x, y, pid);
    evt.post_to_pid(pid);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: s1_input <pid> <x> <y>");
        eprintln!("  pid  — target process id (iPhone Mirroring)");
        eprintln!("  x, y — global screen point (logical pixels, top-left origin)");
        std::process::exit(1);
    }

    let pid: i32 = args[1].parse().expect("pid must be an integer");
    let x: f64 = args[2].parse().expect("x must be a number");
    let y: f64 = args[3].parse().expect("y must be a number");

    // Build a source that mimics real HID hardware.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .expect("CGEventSource::new failed — running in a context without a window server?");

    // --- mouseDown ---
    post_mouse(&source, CGEventType::LeftMouseDown, x, y, pid, "mouseDown");
    thread::sleep(Duration::from_millis(16));

    // --- five LeftMouseDragged steps, 4 px downward each ---
    const STEPS: u8 = 5;
    const STEP_PX: f64 = 4.0;
    for i in 1..=STEPS {
        let dy = y + f64::from(i) * STEP_PX;
        post_mouse(
            &source,
            CGEventType::LeftMouseDragged,
            x,
            dy,
            pid,
            "mouseDragged",
        );
        thread::sleep(Duration::from_millis(16));
    }

    // --- mouseUp at the final position ---
    let y_end = y + f64::from(STEPS) * STEP_PX;
    post_mouse(&source, CGEventType::LeftMouseUp, x, y_end, pid, "mouseUp");

    println!("done — gesture complete");
}
