//! Capture: find the iPhone Mirroring window and stream it as raw BGRA frames.
//!
//! This is the capture half of the video pipeline, productionised from the
//! hardware-validated spikes `s0_capture` / `s0b_encode`. It does two things:
//!
//!   1. **Window selection** — enumerates on-screen windows via ScreenCaptureKit,
//!      maps them onto the pure [`crate::window::WindowInfo`] model, and runs the
//!      validated [`crate::window::select_mirroring`] picker (skips welcome /
//!      menubar windows, prefers the largest on-screen phone-shaped window).
//!   2. **Streaming** — opens an `SCStream` filtered to that single window
//!      (BGRA, ~30 fps, no cursor) and forwards each captured frame to an
//!      internal callback as a [`Frame`] carrying the raw `CVImageBuffer` plus a
//!      monotonically increasing PTS.
//!
//! The encoder ([`crate::encode`]) is the only consumer; it installs a callback
//! and feeds each `Frame` to VideoToolbox.
//!
//! ## Restart-on-window-change
//! The chosen window can disappear (phone disconnects) or be replaced (the user
//! re-opens Mirroring, getting a fresh window id). [`CaptureStream`] watches for
//! the selected window id changing and restarts the `SCStream` against the new
//! window. SCK delivers *no* frames on a static screen — that idle gap is the
//! encoder's keepalive concern, not the capture layer's.
//!
//! All ScreenCaptureKit / CoreVideo calls are gated behind
//! `#[cfg(target_os = "macos")]` so the crate builds and unit-tests cleanly on
//! every platform. The non-macOS path exposes the same public surface but
//! returns an "unsupported platform" error from [`CaptureStream::start`].

/// Configuration for a capture stream.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Target frame rate (frames per second). The capture layer sets the
    /// `SCStream` minimum frame interval to `1/fps`.
    pub fps: u32,
    /// Show the cursor in the captured frames. Off by default (the phone has no
    /// macOS cursor and a stray pointer just adds noise).
    pub show_cursor: bool,
    /// How often to re-enumerate windows and check whether the selected window
    /// id changed (and therefore the stream needs restarting), in milliseconds.
    pub window_poll_millis: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            show_cursor: false,
            window_poll_millis: 1000,
        }
    }
}

/// A captured raw frame, handed to the encoder callback.
///
/// On macOS, `image_buffer` points at the live `CVImageBuffer` for the duration
/// of the callback only — the callback must encode (or copy) synchronously and
/// must NOT retain the pointer past its return. `pts_micros` is a monotonically
/// increasing presentation timestamp in microseconds since stream start.
#[cfg(target_os = "macos")]
pub struct Frame<'a> {
    /// The live BGRA pixel buffer (`CVImageBuffer` == `CVPixelBuffer`).
    pub image_buffer: &'a objc2_core_video::CVImageBuffer,
    /// Presentation timestamp in microseconds, monotonically increasing.
    pub pts_micros: u64,
    /// Frame width in pixels (the selected window's width).
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use crate::window::{select_mirroring, WindowInfo};

    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use core_foundation::base::TCFType;
    use core_media_rs::cm_sample_buffer::CMSampleBuffer as SckSampleBuffer;
    use core_media_rs::cm_time::CMTime as SckCMTime;
    use objc2_core_video::CVImageBuffer;
    use screencapturekit::shareable_content::{SCShareableContent, SCWindow};
    use screencapturekit::stream::configuration::pixel_format::PixelFormat;
    use screencapturekit::stream::configuration::SCStreamConfiguration;
    use screencapturekit::stream::content_filter::SCContentFilter;
    use screencapturekit::stream::output_trait::SCStreamOutputTrait;
    use screencapturekit::stream::output_type::SCStreamOutputType;
    use screencapturekit::stream::SCStream;

    /// Callback invoked once per captured frame, on the SCK delegate thread.
    /// Must be `Send + Sync` because SCK calls it from its own thread.
    pub type FrameCallback = Arc<dyn Fn(Frame<'_>) + Send + Sync>;

    /// Enumerate on-screen windows and select the iPhone Mirroring window.
    /// Returns the chosen `SCWindow` plus its (width, height).
    pub(super) fn find_mirroring_window() -> anyhow::Result<(SCWindow, u32, u32)> {
        let content = SCShareableContent::get().map_err(|e| {
            anyhow::anyhow!(
                "SCShareableContent::get failed: {e:?} — is Screen Recording permission granted?"
            )
        })?;
        let windows = content.windows();

        // Map SCK windows onto the pure WindowInfo model so the validated
        // `select_mirroring` picker does the actual selection.
        let mut infos = Vec::with_capacity(windows.len());
        for win in windows.iter() {
            let app = win.owning_application();
            let frame = win.get_frame();
            infos.push(WindowInfo {
                id: win.window_id(),
                app: app.application_name(),
                bundle: app.bundle_identifier(),
                title: win.title(),
                on_screen: win.is_on_screen(),
                w: frame.size.width,
                h: frame.size.height,
                x: frame.origin.x,
                y: frame.origin.y,
            });
        }

        let chosen_id = match select_mirroring(&infos) {
            Some(info) => info.id,
            None => {
                let mut listing =
                    String::from("could not find an iPhone Mirroring window. windows seen:\n");
                for info in &infos {
                    listing.push_str(&format!(
                        "  id={:<6} owner={:?} bundle={:?} title={:?} onScreen={} {}x{}\n",
                        info.id,
                        info.app,
                        info.bundle,
                        info.title,
                        info.on_screen,
                        info.w as i64,
                        info.h as i64,
                    ));
                }
                return Err(anyhow::anyhow!(listing));
            }
        };

        // Find the SCWindow matching the chosen id.
        let win = windows
            .iter()
            .find(|w| w.window_id() == chosen_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("selected window id {chosen_id} vanished"))?;
        let frame = win.get_frame();
        let width = (frame.size.width as i32).max(2) as u32;
        let height = (frame.size.height as i32).max(2) as u32;
        Ok((win, width, height))
    }

    /// Resolve only the CGWindowID of the current Mirroring window.
    ///
    /// Thin wrapper around [`find_mirroring_window`] that discards the `SCWindow`
    /// and dimensions — used by [`screenshot_mirroring_png`] so the ID extraction
    /// logic lives in one place.
    pub(super) fn mirroring_window_id() -> anyhow::Result<u32> {
        let (win, _, _) = find_mirroring_window()?;
        Ok(win.window_id())
    }

    /// Build the `screencapture` argument list for the given window id and output
    /// path string.  Factored out so it can be unit-tested without hardware.
    ///
    /// Flags:
    ///   `-x`  — no sound effect on capture
    ///   `-o`  — no drop shadow (shadow-free rectangle, matches the bare phone UI)
    ///   `-l <id>` — capture exactly this CGWindowID (works for non-frontmost windows)
    pub(super) fn screencapture_args(window_id: u32, out_path: &str) -> Vec<String> {
        vec![
            "-x".into(),
            "-o".into(),
            "-l".into(),
            window_id.to_string(),
            out_path.into(),
        ]
    }

    /// Capture the iPhone Mirroring window as a PNG byte vector.
    ///
    /// # Why `/usr/sbin/screencapture` instead of `SCScreenshotManager`?
    ///
    /// The `screencapturekit` crate (0.3.6) lacks a clean one-shot screenshot API;
    /// `SCScreenshotManager` is available from macOS 14.0 but the crate binding
    /// is async/callback-heavy and would require a new Tokio runtime or unsafe
    /// CFRunLoop juggling.  The built-in `/usr/sbin/screencapture` CLI is stable,
    /// ships on every macOS version we target (11+), accepts a CGWindowID via `-l`,
    /// captures non-frontmost windows with `-o` (no shadow), and returns a
    /// standard PNG that every consumer can use without a custom pixel-format
    /// conversion.  The daemon already holds the Screen Recording TCC grant
    /// (required for `CaptureStream::start`); child processes spawned by a
    /// TCC-granted process inherit that authorization under macOS's responsible-
    /// process rule, so `screencapture` gets the permission it needs automatically.
    ///
    /// # Hardware validation note
    /// Validated at the API level; live hardware test pending (phone offline at
    /// time of implementation).  The integration pass should run:
    ///   `curl http://localhost:<port>/agent/screenshot -o /tmp/test.png`
    /// and verify the PNG opens correctly.
    pub fn screenshot_mirroring_png() -> anyhow::Result<Vec<u8>> {
        // Unique temp path: pid + monotonic counter avoids collisions between
        // concurrent callers (e.g., two agent requests racing).
        static SHOT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SHOT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let out_path =
            std::env::temp_dir().join(format!("iphone-use-shot-{}-{seq}.png", std::process::id()));
        let out_str = out_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("temp path is not valid UTF-8: {out_path:?}"))?
            .to_owned();

        let window_id = mirroring_window_id()?;
        let args = screencapture_args(window_id, &out_str);

        let status = std::process::Command::new("/usr/sbin/screencapture")
            .args(&args)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to spawn /usr/sbin/screencapture: {e}"))?;

        if !status.status.success() {
            // Best-effort cleanup even on failure.
            let _ = std::fs::remove_file(&out_path);
            return Err(anyhow::anyhow!(
                "screencapture exited {}: {}",
                status.status,
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }

        let bytes = std::fs::read(&out_path).map_err(|e| {
            anyhow::anyhow!(
                "screencapture reported success but output file is unreadable ({e}): {out_path:?}"
            )
        })?;
        let _ = std::fs::remove_file(&out_path); // best-effort cleanup
        if bytes.is_empty() {
            return Err(anyhow::anyhow!(
                "screencapture wrote an empty file — is the Mirroring window visible?"
            ));
        }
        Ok(bytes)
    }

    /// Classify what the iPhone Mirroring window is currently showing. The
    /// window *still exists* on both interstitials, so `find_mirroring_geometry`
    /// reports it present even when no input lands on the phone (issue #14: the
    /// false-positive `phone_target:true`). iPhone Mirroring is AX-hardened (0
    /// accessibility windows), so we classify by pixels — both interstitials are
    /// ~uniform dark gray (≈RGB 46,50,52, hardware-sampled ≈0.87), which a live
    /// mirror never is; the **"iPhone in Use"** screen is told apart from
    /// **"Connection Paused"** by its blue phone glyph (≈0.9% blue vs ~0%).
    pub fn mirroring_state() -> anyhow::Result<super::MirrorState> {
        let png_bytes = screenshot_mirroring_png()?;
        Ok(super::classify_mirror_png(&png_bytes)?)
    }

    /// Find the iPhone Mirroring window and build a [`crate::coords::SessionGeometry`]
    /// describing its on-screen content rect, for mapping normalized input
    /// coordinates back to absolute screen points.
    ///
    /// The content rect is the chosen window's on-screen frame (origin top-left,
    /// in points, as ScreenCaptureKit reports it). Orientation is inferred from
    /// the window aspect ratio: a window that is taller than it is wide is treated
    /// as `Portrait`; otherwise `LandscapeLeft`. iPhone Mirroring renders the phone
    /// upright inside the window regardless of physical device orientation, so the
    /// content rect mapping is correct for portrait; landscape is a best-effort
    /// inference (the orientation field is informational for `coords::to_screen`).
    ///
    /// `scale` is filled with the main display's backing scale factor (points per
    /// pixel) when available, defaulting to `2.0` (Retina) — it is informational.
    pub fn find_mirroring_geometry() -> anyhow::Result<crate::coords::SessionGeometry> {
        let (win, w_px, _h_px) = find_mirroring_window()?;
        let frame = win.get_frame();
        Ok(super::geometry_from_window(
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
            w_px,
        ))
    }

    /// SCK output handler: turns each captured `CMSampleBuffer` into a [`Frame`]
    /// and invokes the user callback. Idle SCK samples carry no pixel buffer
    /// (`CouldNotGetDataBuffer`) and are dropped silently — normal SCK behaviour.
    struct Handler {
        cb: FrameCallback,
        width: u32,
        height: u32,
        fps: u32,
        frame_index: AtomicI64,
    }

    impl SCStreamOutputTrait for Handler {
        fn did_output_sample_buffer(
            &self,
            sample_buffer: SckSampleBuffer,
            of_type: SCStreamOutputType,
        ) {
            if of_type != SCStreamOutputType::Screen {
                return;
            }
            // Idle frames have no image buffer -> CouldNotGetDataBuffer. Drop.
            let pixel_buffer = match sample_buffer.get_pixel_buffer() {
                Ok(pb) => pb,
                Err(_) => return,
            };
            let raw = pixel_buffer.as_concrete_TypeRef(); // *mut __CVPixelBufferRef
            if raw.is_null() {
                return;
            }
            // SAFETY: the raw pointer is a live CVPixelBuffer (== CVImageBuffer)
            // retained by `pixel_buffer` for the duration of this call only.
            let image_buffer = unsafe { &*(raw as *const CVImageBuffer) };

            let idx = self.frame_index.fetch_add(1, Ordering::Relaxed);
            // PTS in microseconds at the configured fps.
            let pts_micros = if self.fps > 0 {
                (idx as u64).saturating_mul(1_000_000) / self.fps as u64
            } else {
                idx as u64
            };

            (self.cb)(Frame {
                image_buffer,
                pts_micros,
                width: self.width,
                height: self.height,
            });
        }
    }

    /// A running capture stream. Owns the SCK `SCStream` and a background watcher
    /// thread that restarts the stream when the selected window id changes.
    pub struct CaptureStream {
        cb: FrameCallback,
        cfg: CaptureConfig,
        // The id of the window we're currently streaming, shared with the watcher.
        current_id: Arc<AtomicU32>,
        // Last known (width, height) — published so the encoder can resize.
        dimensions: Arc<(AtomicU32, AtomicU32)>,
        stop: Arc<AtomicBool>,
        // Held to keep the active stream alive; replaced on restart.
        stream: Arc<Mutex<Option<SCStream>>>,
        watcher: Option<std::thread::JoinHandle<()>>,
        // Whether the watcher could find a Mirroring window on its last poll.
        // `false` = the window is gone (Mirroring quit / phone fully disconnected)
        // — the keepalive keeps re-encoding the last frame, so frame flow alone
        // can't tell a viewer the phone is offline. The daemon surfaces this to
        // viewers as a `phone_status` signaling message.
        window_present: Arc<AtomicBool>,
    }

    impl CaptureStream {
        /// Find the window, open the stream, and start a watcher that restarts on
        /// window-id change. Frames flow to `cb` on the SCK delegate thread.
        pub fn start(cfg: CaptureConfig, cb: FrameCallback) -> anyhow::Result<Self> {
            let (win, width, height) = find_mirroring_window()?;
            let id = win.window_id();

            let current_id = Arc::new(AtomicU32::new(id));
            let dimensions = Arc::new((AtomicU32::new(width), AtomicU32::new(height)));
            let stop = Arc::new(AtomicBool::new(false));
            let window_present = Arc::new(AtomicBool::new(true)); // start() just found it
            let stream_slot: Arc<Mutex<Option<SCStream>>> = Arc::new(Mutex::new(None));

            let stream = open_stream(&win, width, height, &cfg, cb.clone())?;
            *stream_slot.lock().unwrap() = Some(stream);

            // Watcher thread: poll for window-id change and restart.
            let watcher = {
                let cb = cb.clone();
                let cfg = cfg.clone();
                let current_id = current_id.clone();
                let dimensions = dimensions.clone();
                let stop = stop.clone();
                let stream_slot = stream_slot.clone();
                let window_present = window_present.clone();
                std::thread::Builder::new()
                    .name("capture-watcher".into())
                    .spawn(move || {
                        let poll =
                            std::time::Duration::from_millis(cfg.window_poll_millis.max(100));
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(poll);
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            match find_mirroring_window() {
                                Ok((new_win, w, h)) => {
                                    window_present.store(true, Ordering::Relaxed);
                                    let new_id = new_win.window_id();
                                    if new_id != current_id.load(Ordering::Relaxed) {
                                        // Window changed — restart the stream.
                                        if let Ok(new_stream) =
                                            open_stream(&new_win, w, h, &cfg, cb.clone())
                                        {
                                            let mut slot = stream_slot.lock().unwrap();
                                            if let Some(old) = slot.take() {
                                                let _ = old.stop_capture();
                                            }
                                            *slot = Some(new_stream);
                                            current_id.store(new_id, Ordering::Relaxed);
                                            dimensions.0.store(w, Ordering::Relaxed);
                                            dimensions.1.store(h, Ordering::Relaxed);
                                        }
                                    }
                                }
                                Err(_) => {
                                    // Window temporarily gone (phone disconnected).
                                    // Keep the last stream; it will resume frames
                                    // when the window reappears, or the next poll
                                    // restarts on a new id. Publish the absence so
                                    // viewers get a "phone offline" state instead
                                    // of a keepalive-frozen frame.
                                    window_present.store(false, Ordering::Relaxed);
                                }
                            }
                        }
                    })
                    .map_err(|e| anyhow::anyhow!("failed to spawn capture watcher: {e}"))?
            };

            Ok(Self {
                cb,
                cfg,
                current_id,
                dimensions,
                stop,
                stream: stream_slot,
                watcher: Some(watcher),
                window_present,
            })
        }

        /// Whether the Mirroring window was found on the watcher's last poll.
        /// `false` while the window is gone (Mirroring quit / phone disconnected).
        pub fn window_present(&self) -> bool {
            self.window_present.load(Ordering::Relaxed)
        }

        /// Current frame dimensions (width, height).
        pub fn dimensions(&self) -> (u32, u32) {
            (
                self.dimensions.0.load(Ordering::Relaxed),
                self.dimensions.1.load(Ordering::Relaxed),
            )
        }

        /// The window id currently being streamed.
        pub fn window_id(&self) -> u32 {
            self.current_id.load(Ordering::Relaxed)
        }

        /// Frame callback (kept so callers can inspect; mostly for symmetry).
        pub fn config(&self) -> &CaptureConfig {
            &self.cfg
        }

        /// Internal: clone of the callback (used by the encoder if it re-wires).
        pub fn callback(&self) -> FrameCallback {
            self.cb.clone()
        }
    }

    impl Drop for CaptureStream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(w) = self.watcher.take() {
                let _ = w.join();
            }
            if let Some(stream) = self.stream.lock().unwrap().take() {
                let _ = stream.stop_capture();
            }
        }
    }

    /// Build the single-window filter + BGRA config and start an `SCStream`.
    fn open_stream(
        window: &SCWindow,
        width: u32,
        height: u32,
        cfg: &CaptureConfig,
        cb: FrameCallback,
    ) -> anyhow::Result<SCStream> {
        let filter = SCContentFilter::new().with_desktop_independent_window(window);
        let fps = cfg.fps.max(1);
        let frame_interval = SckCMTime {
            value: 1,
            timescale: fps as i32,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        };
        let config = SCStreamConfiguration::new()
            .set_width(width)
            .and_then(|c| c.set_height(height))
            .and_then(|c| c.set_pixel_format(PixelFormat::BGRA))
            .and_then(|c| c.set_shows_cursor(cfg.show_cursor))
            .and_then(|c| c.set_minimum_frame_interval(&frame_interval))
            .map_err(|e| anyhow::anyhow!("SCStreamConfiguration failed: {e:?}"))?;

        let handler = Handler {
            cb,
            width,
            height,
            fps,
            frame_index: AtomicI64::new(0),
        };

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(handler, SCStreamOutputType::Screen);
        stream
            .start_capture()
            .map_err(|e| anyhow::anyhow!("SCStream start_capture failed: {e:?}"))?;
        Ok(stream)
    }
}

#[cfg(target_os = "macos")]
pub use imp::{
    find_mirroring_geometry, mirroring_state, screenshot_mirroring_png, CaptureStream,
    FrameCallback,
};

/// What the iPhone Mirroring window is showing (issue #14/#3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorState {
    /// Live phone content — agent input via the mirror path lands.
    Active,
    /// "Connection Paused" (phone locked / out of range): a Resume button is
    /// present, so this is auto-recoverable by clicking it (issue #3).
    Paused,
    /// "iPhone in Use" (a human is on the phone): no Resume button — Mirroring
    /// reconnects only when the human stops. Do NOT auto-click; wait it out.
    InUse,
}

impl MirrorState {
    /// The `mirror_state` string in `/agent/status`.
    pub fn as_str(self) -> &'static str {
        match self {
            MirrorState::Active => "active",
            MirrorState::Paused => "paused",
            MirrorState::InUse => "in_use",
        }
    }
    /// Can an agent drive the phone through the mirror right now?
    pub fn drivable(self) -> bool {
        matches!(self, MirrorState::Active)
    }
}

/// `mirroring_state` stub off-macOS (no Mirroring window exists).
#[cfg(not(target_os = "macos"))]
pub fn mirroring_state() -> anyhow::Result<MirrorState> {
    Ok(MirrorState::Active)
}

/// Background colors of the iPhone Mirroring interstitials + per-channel match
/// tolerance. The interstitial follows the system appearance, so we match BOTH:
/// dark mode RGB(46,50,52) and light mode RGB(233,235,237) — hardware-sampled
/// from "Connection Paused" and "iPhone in Use" in each appearance (each ≈0.86
/// of the frame). A live mirror is never this uniform in either color.
const INTERSTITIAL_BGS: [[i32; 3]; 2] = [[46, 50, 52], [233, 235, 237]];
const PAUSED_GRAY_TOL: i32 = 16;
/// Min gray fraction to treat a frame as an interstitial (not live). Both paused
/// and in-use sample ≈0.87; live content is far below, so 0.70 has wide margin.
const INTERSTITIAL_GRAY_THRESHOLD: f32 = 0.70;
/// Min blue-glyph fraction that distinguishes "iPhone in Use" (≈0.9% blue) from
/// "Connection Paused" (~0% blue). 0.3% sits cleanly between them.
const IN_USE_BLUE_THRESHOLD: f32 = 0.003;

/// Classify a captured Mirroring frame. Pure (no capture) so it unit-tests
/// cross-platform. Samples every 6th pixel in both axes — enough for a
/// whole-screen uniformity check at a fraction of the cost. Only RGB/RGBA 8-bit
/// (what `screencapture` emits) is supported; anything else errors.
fn classify_mirror_png(bytes: &[u8]) -> anyhow::Result<MirrorState> {
    let (gray, blue) = sample_fractions(bytes)?;
    Ok(if gray < INTERSTITIAL_GRAY_THRESHOLD {
        MirrorState::Active
    } else if blue >= IN_USE_BLUE_THRESHOLD {
        MirrorState::InUse
    } else {
        MirrorState::Paused
    })
}

/// `(gray_fraction, blue_fraction)` over grid-sampled pixels: gray = within
/// tolerance of the interstitial background; blue = the "iPhone in Use" glyph
/// color (markedly blue: `b>120 && b-r>40 && b-g>20`).
fn sample_fractions(bytes: &[u8]) -> anyhow::Result<(f32, f32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| anyhow::anyhow!("png header: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| anyhow::anyhow!("png decode: {e}"))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(anyhow::anyhow!(
            "unsupported PNG bit depth {:?}",
            info.bit_depth
        ));
    }
    let channels = match info.color_type {
        png::ColorType::Rgb => 3usize,
        png::ColorType::Rgba => 4usize,
        other => return Err(anyhow::anyhow!("unsupported PNG color type {other:?}")),
    };
    let (w, h) = (info.width as usize, info.height as usize);
    let data = &buf[..info.buffer_size()];
    let stride = w * channels;
    // Count blue ONLY in the central glyph region. The blue/indigo iPhone
    // Mirroring badge in the TOP-LEFT corner is present on EVERY interstitial
    // ("Connection Paused", "iPhone in Use", "Connection Interrupted", "Timed
    // Out", "Wi-Fi Off"), so a whole-frame blue count mis-read the gray-phone
    // screens (Timed Out / Interrupted) as "iPhone in Use" and told the user to
    // LOCK the phone — wrong recovery (hardware bug, 2026-06-14). The real
    // distinguisher is the CENTER phone glyph: BLUE only on "iPhone in Use",
    // gray on every other interstitial.
    let (cx0, cx1) = ((w as f32 * 0.30) as usize, (w as f32 * 0.70) as usize);
    let (cy0, cy1) = ((h as f32 * 0.32) as usize, (h as f32 * 0.58) as usize);
    let (mut gray, mut blue, mut total) = (0u64, 0u64, 0u64);
    let mut y = 0;
    while y < h {
        let row = &data[y * stride..];
        let mut x = 0;
        while x < w {
            let p = &row[x * channels..];
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            // Background if within tolerance of EITHER appearance's interstitial color.
            if INTERSTITIAL_BGS.iter().any(|c| {
                (r - c[0]).abs() <= PAUSED_GRAY_TOL
                    && (g - c[1]).abs() <= PAUSED_GRAY_TOL
                    && (b - c[2]).abs() <= PAUSED_GRAY_TOL
            }) {
                gray += 1;
            }
            if x >= cx0 && x < cx1 && y >= cy0 && y < cy1 && b > 120 && b - r > 40 && b - g > 20 {
                blue += 1;
            }
            total += 1;
            x += 6;
        }
        y += 6;
    }
    if total == 0 {
        return Ok((0.0, 0.0));
    }
    Ok((gray as f32 / total as f32, blue as f32 / total as f32))
}

// ---------------------------------------------------------------------------
// Pure geometry construction (testable on every platform).
// ---------------------------------------------------------------------------

/// Build a [`crate::coords::SessionGeometry`] from a window's on-screen frame
/// (points, origin top-left) and its captured pixel width.
///
/// * `content_rect` is the frame as given (the iPhone Mirroring window content).
/// * `orientation` is inferred from the frame aspect ratio: `Portrait` when the
///   window is at least as tall as it is wide, else `LandscapeLeft`. iPhone
///   Mirroring renders the phone upright inside the window, so this matches the
///   common portrait case; landscape is a best-effort inference and the field is
///   informational for [`crate::coords::to_screen`].
/// * `scale` ≈ pixels-per-point (`width_px / w`), clamped to ≥ 1.0, defaulting to
///   `2.0` (Retina) when `w` is non-positive.
pub fn geometry_from_window(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    width_px: u32,
) -> crate::coords::SessionGeometry {
    use crate::coords::{Orientation, Rect, SessionGeometry};
    let orientation = if h >= w {
        Orientation::Portrait
    } else {
        Orientation::LandscapeLeft
    };
    let scale = if w > 0.0 {
        (width_px as f64 / w).max(1.0)
    } else {
        2.0
    };
    SessionGeometry {
        content_rect: Rect { x, y, w, h },
        scale,
        orientation,
    }
}

// ---------------------------------------------------------------------------
// Non-macOS stub for find_mirroring_geometry (keeps the public surface stable).
// ---------------------------------------------------------------------------

/// Find the iPhone Mirroring window and return its [`crate::coords::SessionGeometry`]
/// for input mapping. On non-macOS this always returns `None`.
#[cfg(not(target_os = "macos"))]
pub fn find_mirroring_geometry() -> anyhow::Result<crate::coords::SessionGeometry> {
    Err(anyhow::anyhow!(
        "find_mirroring_geometry is only supported on macOS (ScreenCaptureKit)"
    ))
}

// ---------------------------------------------------------------------------
// Non-macOS stub: same public surface, returns "unsupported" from start().
// ---------------------------------------------------------------------------

/// Callback invoked once per captured frame. On non-macOS this type exists only
/// to keep the public surface identical; it is never invoked.
#[cfg(not(target_os = "macos"))]
pub type FrameCallback = std::sync::Arc<dyn Fn() + Send + Sync>;

/// A running capture stream (non-macOS stub).
#[cfg(not(target_os = "macos"))]
pub struct CaptureStream;

#[cfg(not(target_os = "macos"))]
impl CaptureStream {
    /// Always fails on non-macOS — capture requires ScreenCaptureKit.
    pub fn start(_cfg: CaptureConfig, _cb: FrameCallback) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!(
            "capture is only supported on macOS (ScreenCaptureKit)"
        ))
    }

    /// Non-macOS stub — there is never a window.
    pub fn window_present(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Non-macOS stub for screenshot_mirroring_png.
// ---------------------------------------------------------------------------

/// Capture the iPhone Mirroring window as a PNG byte vector.
///
/// On non-macOS this always returns an error; the implementation requires
/// ScreenCaptureKit (window enumeration) and `/usr/sbin/screencapture` (both
/// macOS-only). See the macOS implementation in `imp::screenshot_mirroring_png`
/// for the full rationale.
#[cfg(not(target_os = "macos"))]
pub fn screenshot_mirroring_png() -> anyhow::Result<Vec<u8>> {
    Err(anyhow::anyhow!(
        "screenshot_mirroring_png is only supported on macOS"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_30fps_no_cursor() {
        let c = CaptureConfig::default();
        assert_eq!(c.fps, 30);
        assert!(!c.show_cursor);
        assert!(c.window_poll_millis >= 100);
    }

    #[test]
    fn config_is_clonable() {
        let c = CaptureConfig {
            fps: 60,
            show_cursor: true,
            window_poll_millis: 500,
        };
        let c2 = c.clone();
        assert_eq!(c2.fps, 60);
        assert!(c2.show_cursor);
    }

    // ── geometry_from_window (pure) ──────────────────────────────────────────

    use crate::coords::Orientation;

    #[test]
    fn geometry_portrait_window_is_portrait() {
        // A typical iPhone Mirroring window: 312×694 pts @ Retina (624 px wide).
        let geo = geometry_from_window(100.0, 50.0, 312.0, 694.0, 624);
        assert_eq!(geo.content_rect.x, 100.0);
        assert_eq!(geo.content_rect.y, 50.0);
        assert_eq!(geo.content_rect.w, 312.0);
        assert_eq!(geo.content_rect.h, 694.0);
        assert_eq!(geo.orientation, Orientation::Portrait);
        assert!((geo.scale - 2.0).abs() < 1e-9, "scale={}", geo.scale);
    }

    #[test]
    fn geometry_landscape_window_is_landscape() {
        let geo = geometry_from_window(0.0, 0.0, 694.0, 312.0, 1388);
        assert_eq!(geo.orientation, Orientation::LandscapeLeft);
    }

    #[test]
    fn geometry_square_window_defaults_portrait() {
        let geo = geometry_from_window(0.0, 0.0, 400.0, 400.0, 400);
        assert_eq!(geo.orientation, Orientation::Portrait);
        assert!((geo.scale - 1.0).abs() < 1e-9);
    }

    #[test]
    fn geometry_scale_clamped_and_default() {
        // width_px smaller than w → clamp scale to ≥1.0.
        let geo = geometry_from_window(0.0, 0.0, 312.0, 694.0, 100);
        assert!(geo.scale >= 1.0);
        // zero width → default 2.0.
        let geo0 = geometry_from_window(0.0, 0.0, 0.0, 694.0, 624);
        assert!((geo0.scale - 2.0).abs() < 1e-9);
    }

    #[test]
    fn geometry_maps_input_center_to_window_center() {
        // Cross-check with coords::to_screen: a normalized center maps to the
        // window-rect center.
        let geo = geometry_from_window(100.0, 200.0, 300.0, 600.0, 600);
        let p = crate::coords::to_screen((0.5, 0.5), &geo).unwrap();
        assert!((p.0 - 250.0).abs() < 1e-9);
        assert!((p.1 - 500.0).abs() < 1e-9);
    }

    // ── screenshot_mirroring_png helpers (pure / no hardware) ───────────────

    /// `screencapture_args` is defined only on macOS (inside `imp`); these tests
    /// are gated accordingly.
    #[cfg(target_os = "macos")]
    mod screenshot_tests {
        use super::super::imp::screencapture_args;

        #[test]
        fn screencapture_args_structure() {
            // Verify the exact argument layout expected by /usr/sbin/screencapture.
            // -x: no sound, -o: no shadow, -l <id>: window id, then the output path.
            let args = screencapture_args(12345, "/tmp/test.png");
            assert_eq!(args.len(), 5, "expected exactly 5 args: {args:?}");
            assert_eq!(args[0], "-x");
            assert_eq!(args[1], "-o");
            assert_eq!(args[2], "-l");
            assert_eq!(args[3], "12345");
            assert_eq!(args[4], "/tmp/test.png");
        }

        #[test]
        fn screencapture_args_window_id_is_stringified() {
            let args = screencapture_args(0, "/out.png");
            assert_eq!(args[3], "0");
            let args = screencapture_args(u32::MAX, "/out.png");
            assert_eq!(args[3], u32::MAX.to_string());
        }
    }

    /// Temp-path uniqueness test — platform-agnostic (uses the same formula as
    /// `screenshot_mirroring_png` on macOS and the non-macOS stub shares the
    /// same naming scheme).
    #[test]
    fn screenshot_temp_paths_are_unique() {
        // Simulate two concurrent path constructions (same pid, different seq).
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let pid = std::process::id();
        let make = || {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!("iphone-use-shot-{pid}-{seq}.png"))
        };
        let p1 = make();
        let p2 = make();
        assert_ne!(
            p1, p2,
            "two consecutive temp paths must differ: {p1:?} == {p2:?}"
        );
        // Both must share the same directory.
        assert_eq!(p1.parent(), p2.parent());
        // Both must end with .png.
        assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(p2.extension().and_then(|e| e.to_str()), Some("png"));
    }

    /// Encode an RGBA PNG: a solid `bg` with a centered rectangular `glyph`
    /// patch covering `glyph_frac` of the area (used to fake the in-use blue glyph).
    fn png_with_glyph(w: u32, h: u32, bg: [u8; 3], glyph: [u8; 3], glyph_frac: f32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            // Centered square patch of the requested area fraction.
            let side = ((w as f32 * h as f32 * glyph_frac).sqrt()) as u32;
            let (x0, y0) = ((w.saturating_sub(side)) / 2, (h.saturating_sub(side)) / 2);
            let mut data = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    let c = if side > 0 && x >= x0 && x < x0 + side && y >= y0 && y < y0 + side {
                        glyph
                    } else {
                        bg
                    };
                    data.extend_from_slice(&[c[0], c[1], c[2], 255]);
                }
            }
            writer.write_image_data(&data).unwrap();
        }
        out
    }

    fn solid_png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        png_with_glyph(w, h, rgb, rgb, 0.0)
    }

    #[test]
    fn classify_distinguishes_active_paused_in_use() {
        // Live content (white): below the gray threshold → Active.
        assert_eq!(
            classify_mirror_png(&solid_png(120, 200, [255, 255, 255])).unwrap(),
            MirrorState::Active
        );
        // Uniform interstitial gray, no blue glyph → Connection Paused (dark + light).
        assert_eq!(
            classify_mirror_png(&solid_png(120, 200, [46, 50, 52])).unwrap(),
            MirrorState::Paused
        );
        assert_eq!(
            classify_mirror_png(&solid_png(120, 200, [233, 235, 237])).unwrap(),
            MirrorState::Paused
        );
        // Same gray plus a blue phone glyph (~1% area) → iPhone in Use (dark + light).
        let in_use = png_with_glyph(160, 280, [46, 50, 52], [90, 150, 235], 0.01);
        assert_eq!(classify_mirror_png(&in_use).unwrap(), MirrorState::InUse);
        let in_use_light = png_with_glyph(160, 280, [233, 235, 237], [90, 150, 235], 0.01);
        assert_eq!(
            classify_mirror_png(&in_use_light).unwrap(),
            MirrorState::InUse
        );
        // State helpers.
        assert!(MirrorState::Active.drivable());
        assert!(!MirrorState::Paused.drivable());
        assert!(!MirrorState::InUse.drivable());
        assert_eq!(MirrorState::InUse.as_str(), "in_use");
    }
}
