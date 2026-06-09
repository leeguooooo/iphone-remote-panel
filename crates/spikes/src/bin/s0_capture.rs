//! Spike S0a — ScreenCaptureKit capture probe for the iPhone Mirroring window
//!
//! Purpose: answer ONE scary question on real hardware — can ScreenCaptureKit
//! capture the macOS "iPhone Mirroring" window as **non-black, changing**
//! frames, or does it emit blank / DRM-protected output?
//!
//! This is a throwaway validation probe. It does NOT decide PASS/FAIL — it
//! produces evidence (PNGs + per-frame luminance) that a human inspects.
//! See the RUNBOOK in the task report for how to interpret the output.
//!
//! What it does:
//!   1. Enumerates shareable windows, finds the iPhone Mirroring window by
//!      owning-application name / window title / bundle id
//!      (com.apple.ScreenContinuity). Prefers an on-screen window whose size
//!      is in the 200-900 wide x 400-1600 tall range. Prints the chosen
//!      window's id, owner, and bounds. If none found, lists the windows it
//!      DID see and exits non-zero.
//!   2. Opens an SCStream filtered to that single window
//!      (desktop-independent-window), BGRA pixel format, ~30fps, cursor hidden.
//!   3. Captures 30 frames, writing each to ./s0-frames/frame-NNN.png.
//!   4. Prints a crude non-black check per frame (avg luminance + non-zero
//!      pixel count) so the verdict is readable from stdout alone.
//!
//! Usage:  s0_capture
//!
//! Requires: Screen Recording permission (TCC) granted to the terminal /
//! binary running this. Without it, enumeration may return no windows or the
//! stream will produce black frames — grant it in
//! System Settings > Privacy & Security > Screen Recording, then re-run.

use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::Duration;

use core_media_rs::cm_sample_buffer::CMSampleBuffer;
use core_media_rs::cm_time::CMTime;
use core_video_rs::cv_pixel_buffer::lock::LockTrait;

use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::pixel_format::PixelFormat;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;

const TARGET_FRAMES: usize = 30;
const OUT_DIR: &str = "s0-frames";

/// One captured frame, already converted to tightly-packed RGBA and detached
/// from the (transient) CVPixelBuffer lock so it can cross the channel.
struct Frame {
    width: u32,
    height: u32,
    /// Tightly packed RGBA8 (width * height * 4), already de-strided.
    rgba: Vec<u8>,
}

/// Stream output handler. Runs on a ScreenCaptureKit-owned delegate thread.
/// It extracts pixels (the CVPixelBuffer lock is only valid here) and ships an
/// owned RGBA buffer to the main thread, which writes PNGs and counts frames.
struct CaptureHandler {
    tx: SyncSender<Frame>,
}

impl SCStreamOutputTrait for CaptureHandler {
    fn did_output_sample_buffer(
        &self,
        sample_buffer: CMSampleBuffer,
        of_type: SCStreamOutputType,
    ) {
        if of_type != SCStreamOutputType::Screen {
            return;
        }
        match extract_rgba(&sample_buffer) {
            Ok(frame) => {
                // Best-effort: if the receiver is gone (we already have 30
                // frames and stopped), just drop the frame.
                let _ = self.tx.try_send(frame);
            }
            Err(e) => eprintln!("frame extraction failed: {e}"),
        }
    }
}

/// Pull BGRA bytes out of a sample buffer's CVPixelBuffer and convert to
/// tightly-packed RGBA. Returns Err on the SCK "idle/blank" frames that carry
/// no image buffer, or on a lock failure.
fn extract_rgba(sample_buffer: &CMSampleBuffer) -> Result<Frame, String> {
    let pixel_buffer = sample_buffer
        .get_pixel_buffer()
        .map_err(|e| format!("get_pixel_buffer: {e:?}"))?;

    let width = pixel_buffer.get_width();
    let height = pixel_buffer.get_height();
    let bytes_per_row = pixel_buffer.get_bytes_per_row() as usize;

    // lock() returns a guard that unlocks on drop; as_slice() is BGRA bytes,
    // row-padded to bytes_per_row (>= width*4).
    let guard = pixel_buffer
        .lock()
        .map_err(|e| format!("lock base address: {e:?}"))?;
    let src = guard.as_slice();

    let w = width as usize;
    let h = height as usize;
    let row_pixels = w * 4;
    if src.len() < bytes_per_row * h {
        return Err(format!(
            "src buffer too small: {} < {}*{}",
            src.len(),
            bytes_per_row,
            h
        ));
    }

    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let src_row = &src[y * bytes_per_row..y * bytes_per_row + row_pixels];
        let dst_row = &mut rgba[y * row_pixels..(y + 1) * row_pixels];
        // BGRA -> RGBA
        for x in 0..w {
            let b = src_row[x * 4];
            let g = src_row[x * 4 + 1];
            let r = src_row[x * 4 + 2];
            let a = src_row[x * 4 + 3];
            dst_row[x * 4] = r;
            dst_row[x * 4 + 1] = g;
            dst_row[x * 4 + 2] = b;
            dst_row[x * 4 + 3] = a;
        }
    }

    Ok(Frame {
        width,
        height,
        rgba,
    })
}

/// Crude non-black check: average luminance (0-255) and count of pixels with
/// any non-zero RGB channel. Lets the human read "blank vs not" from stdout.
fn luminance_stats(frame: &Frame) -> (f64, u64) {
    let mut lum_sum: u64 = 0;
    let mut nonzero: u64 = 0;
    let px = (frame.width as u64) * (frame.height as u64);
    for chunk in frame.rgba.chunks_exact(4) {
        let r = chunk[0] as u64;
        let g = chunk[1] as u64;
        let b = chunk[2] as u64;
        // Rec. 601 luma, integer-ish.
        lum_sum += (r * 299 + g * 587 + b * 114) / 1000;
        if r != 0 || g != 0 || b != 0 {
            nonzero += 1;
        }
    }
    let avg = if px == 0 { 0.0 } else { lum_sum as f64 / px as f64 };
    (avg, nonzero)
}

/// Find the iPhone Mirroring window among shareable content. Prints what it
/// considered. Returns the chosen window or an error string.
fn find_mirroring_window(
    content: &SCShareableContent,
) -> Result<screencapturekit::shareable_content::SCWindow, String> {
    let windows = content.windows();

    // A candidate is anything whose owner app name, bundle id, or title hints
    // at iPhone Mirroring. Names vary by locale: "iPhone Mirroring",
    // "iPhone\u{955c}\u{50cf}" (CN), bundle com.apple.ScreenContinuity.
    let is_match = |app_name: &str, bundle: &str, title: &str| -> bool {
        let hay = format!(
            "{}\u{1}{}\u{1}{}",
            app_name.to_lowercase(),
            bundle.to_lowercase(),
            title.to_lowercase()
        );
        hay.contains("iphone mirroring")
            || hay.contains("screencontinuity")
            || hay.contains("iphone\u{955c}\u{50cf}") // iPhone镜像
            || hay.contains("\u{955c}\u{50cf}") // 镜像
    };

    let size_ok = |w: f64, h: f64| -> bool {
        (200.0..=900.0).contains(&w) && (400.0..=1600.0).contains(&h)
    };

    let mut candidates = Vec::new();
    for win in windows.iter() {
        let app = win.owning_application();
        let app_name = app.application_name();
        let bundle = app.bundle_identifier();
        let title = win.title();
        if is_match(&app_name, &bundle, &title) {
            candidates.push(win.clone());
        }
    }

    if candidates.is_empty() {
        let mut listing = String::from(
            "could not find an iPhone Mirroring window. windows seen:\n",
        );
        for win in windows.iter() {
            let app = win.owning_application();
            let frame = win.get_frame();
            listing.push_str(&format!(
                "  id={:<6} owner={:?} bundle={:?} title={:?} onScreen={} {}x{} @ ({},{})\n",
                win.window_id(),
                app.application_name(),
                app.bundle_identifier(),
                win.title(),
                win.is_on_screen(),
                frame.size.width as i64,
                frame.size.height as i64,
                frame.origin.x as i64,
                frame.origin.y as i64,
            ));
        }
        listing.push_str(
            "\nadjust the match in find_mirroring_window() if the window is \
             listed above under a different name.",
        );
        return Err(listing);
    }

    // Prefer: on-screen AND size in the phone-ish range, then on-screen, then
    // anything.
    let pick = candidates
        .iter()
        .find(|w| {
            let f = w.get_frame();
            w.is_on_screen() && size_ok(f.size.width, f.size.height)
        })
        .or_else(|| candidates.iter().find(|w| w.is_on_screen()))
        .or_else(|| candidates.first())
        .cloned()
        .expect("candidates is non-empty");

    Ok(pick)
}

fn write_png(path: &Path, frame: &Frame) -> Result<(), String> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| "RgbaImage::from_raw: buffer size mismatch".to_string())?;
    img.save(path).map_err(|e| format!("png save: {e}"))
}

fn run() -> Result<(), String> {
    println!("s0_capture — ScreenCaptureKit iPhone Mirroring capture probe");

    // 1. Enumerate + find the window.
    let content = SCShareableContent::get()
        .map_err(|e| format!("SCShareableContent::get failed: {e:?} — Screen Recording permission granted?"))?;
    let window = find_mirroring_window(&content)?;

    let app = window.owning_application();
    let frame = window.get_frame();
    println!(
        "chosen window: id={} owner={:?} bundle={:?} title={:?}",
        window.window_id(),
        app.application_name(),
        app.bundle_identifier(),
        window.title(),
    );
    println!(
        "  bounds: {}x{} @ ({},{}) onScreen={}",
        frame.size.width as i64,
        frame.size.height as i64,
        frame.origin.x as i64,
        frame.origin.y as i64,
        window.is_on_screen(),
    );

    // 2. Build the single-window filter + 30fps BGRA config.
    let width = frame.size.width as u32;
    let height = frame.size.height as u32;
    let filter = SCContentFilter::new().with_desktop_independent_window(&window);

    // minimumFrameInterval = 1/30 s
    let frame_interval = CMTime {
        value: 1,
        timescale: 30,
        flags: 1, // kCMTimeFlags_Valid
        epoch: 0,
    };
    let config = SCStreamConfiguration::new()
        .set_width(width.max(2))
        .and_then(|c| c.set_height(height.max(2)))
        .and_then(|c| c.set_pixel_format(PixelFormat::BGRA))
        .and_then(|c| c.set_shows_cursor(false))
        .and_then(|c| c.set_minimum_frame_interval(&frame_interval))
        .map_err(|e| format!("configuration failed: {e:?}"))?;

    // 3. Channel from the SCK delegate thread to main. Bounded so a stalled
    // writer can't grow memory unbounded; a couple of frames of slack.
    let (tx, rx): (SyncSender<Frame>, Receiver<Frame>) = sync_channel(4);
    let handler = CaptureHandler { tx };

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(handler, SCStreamOutputType::Screen);

    std::fs::create_dir_all(OUT_DIR)
        .map_err(|e| format!("could not create {OUT_DIR}: {e}"))?;

    println!("starting capture — target {TARGET_FRAMES} frames into ./{OUT_DIR}/ ...");
    stream
        .start_capture()
        .map_err(|e| format!("start_capture failed: {e:?}"))?;

    // 4. Receive frames, write PNGs, print non-black stats.
    let mut saved = 0usize;
    let mut got_nonblank = false;
    while saved < TARGET_FRAMES {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(frame) => {
                let (avg_lum, nonzero) = luminance_stats(&frame);
                if avg_lum > 0.5 {
                    got_nonblank = true;
                }
                let path = format!("{OUT_DIR}/frame-{saved:03}.png");
                match write_png(Path::new(&path), &frame) {
                    Ok(()) => println!(
                        "frame {saved:03}: {}x{} avg_lum={avg_lum:6.2} nonzero_px={nonzero} -> {path}",
                        frame.width, frame.height
                    ),
                    Err(e) => eprintln!("frame {saved:03}: write failed: {e}"),
                }
                saved += 1;
            }
            Err(_) => {
                eprintln!(
                    "timed out waiting for a frame after {saved} frames — \
                     stream may be producing nothing (permission? window gone?)."
                );
                break;
            }
        }
    }

    let _ = stream.stop_capture();

    println!("captured {saved} frame(s) into ./{OUT_DIR}/");
    if !got_nonblank {
        println!(
            "WARNING: every captured frame had ~zero average luminance — \
             frames look BLANK/BLACK. This is the scary outcome S0a exists to \
             detect; inspect the PNGs and re-check the RUNBOOK before judging."
        );
    } else {
        println!(
            "at least one frame had non-zero luminance — open the PNGs to \
             confirm they show the actual iPhone screen (not e.g. a placeholder)."
        );
    }
    Ok(())
}

fn main() {
    // S0 re-probe fix for `Assertion failed: (did_initialize), CGS_REQUIRE_INIT`:
    // a bare CLI process never initialises the CoreGraphics/WindowServer app
    // connection that SCStream.startCapture requires. NSApplicationLoad()
    // bootstraps the AppKit/CG app context (the same context screenpipe's SCK
    // capture runs inside). Must run BEFORE any ScreenCaptureKit call.
    let ok = objc2_app_kit::NSApplicationLoad();
    eprintln!("NSApplicationLoad() -> {ok}");

    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
