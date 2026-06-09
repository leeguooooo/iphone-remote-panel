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
                std::thread::Builder::new()
                    .name("capture-watcher".into())
                    .spawn(move || {
                        let poll = std::time::Duration::from_millis(cfg.window_poll_millis.max(100));
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(poll);
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            match find_mirroring_window() {
                                Ok((new_win, w, h)) => {
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
                                    // restarts on a new id.
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
            })
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
pub use imp::{CaptureStream, FrameCallback};

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
}
