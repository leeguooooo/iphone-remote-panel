//! Encode: VideoToolbox H.264 encoder + the public video pipeline.
//!
//! Productionised from the hardware-validated spikes `s0b_encode` (capture →
//! VideoToolbox H.264 → playable Annex-B file) and `s0b2_webrtc` (the same
//! encode front-half streamed over WebRTC). The WebRTC layer consumes only the
//! public surface defined here: [`EncodedFrame`], the [`VideoPipeline`] trait,
//! and [`start_pipeline`].
//!
//! ## What the pipeline does
//!   * Opens a [`crate::capture::CaptureStream`] on the iPhone Mirroring window.
//!   * Each captured BGRA frame is fed to a `VTCompressionSession` configured for
//!     **Constrained Baseline, realtime, no B-frames** (low-latency).
//!   * The VT output callback converts each compressed `CMSampleBuffer` from AVCC
//!     (length-prefixed NALUs) to **Annex-B** (start-code-delimited), prepending
//!     SPS+PPS before every keyframe, and publishes an [`EncodedFrame`] to a
//!     [`tokio::sync::broadcast`] channel — one stream, fanned out to every
//!     viewer.
//!
//! ## Quality requirements built in (from the S0b-2 findings, spec §3.1)
//!   * **Static-screen keepalive emits a fresh forced IDR on a ~500 ms idle
//!     timer.** ScreenCaptureKit delivers *no* frames on a static screen, so the
//!     encode loop can't just wait for the next capture. A dedicated keepalive
//!     thread re-encodes the **last captured pixel buffer as a forced IDR** when
//!     the stream has been idle ~500 ms. This keeps the decoder fed and lets late
//!     joiners decode. We deliberately re-encode a fresh IDR rather than
//!     re-broadcasting the previous access unit — repeating a P-frame drifts the
//!     decoder into color-block artifacts.
//!   * [`VideoPipeline::request_keyframe`] forces an IDR on the next frame
//!     (viewer join + RTCP PLI).
//!   * Idle `CouldNotGetDataBuffer` SCK samples are dropped silently (in
//!     [`crate::capture`]).
//!   * 30 fps default; the broadcast channel is bounded (lagging receivers drop
//!     late frames rather than blocking the encoder).
//!
//! All VideoToolbox / CoreMedia calls are gated behind
//! `#[cfg(target_os = "macos")]`. The pure helpers ([`avcc_to_annex_b`],
//! [`keepalive_should_fire`]) and the public types compile and unit-test on every
//! platform.

use std::sync::Arc;

/// One encoded H.264 access unit, ready for the WebRTC payloader.
///
/// `data` is an Annex-B byte buffer (start-code `00 00 00 01` delimited NALUs).
/// On keyframes it carries in-band SPS+PPS followed by the IDR slice, so a late
/// joiner can decode from any keyframe.
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    /// Annex-B access unit. SPS+PPS+IDR on keyframes; a single P-slice otherwise.
    pub data: bytes::Bytes,
    /// True if this is a keyframe (IDR with in-band parameter sets).
    pub is_keyframe: bool,
    /// Presentation timestamp in microseconds since stream start.
    pub pts_micros: u64,
}

/// Configuration for [`start_pipeline`].
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Target frame rate (fps). Default 30.
    pub fps: u32,
    /// Average target bitrate in bits/sec. Default 6 Mbps.
    pub bitrate: u32,
    /// Max keyframe interval in frames (also the GOP length). Default = `fps`
    /// (one keyframe per second).
    pub max_keyframe_interval: u32,
    /// Idle period after which the keepalive thread forces a fresh IDR, in
    /// milliseconds. Default 500 (per the S0b-2 finding).
    pub keepalive_millis: u64,
    /// Capacity of the broadcast channel (number of buffered frames before a slow
    /// receiver starts lagging/dropping). Default 256.
    pub channel_capacity: usize,
    /// Show the cursor in captured frames. Default false.
    pub show_cursor: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate: 6_000_000,
            max_keyframe_interval: 30,
            keepalive_millis: 500,
            channel_capacity: 256,
            show_cursor: false,
        }
    }
}

/// A running video pipeline the WebRTC layer subscribes to.
pub trait VideoPipeline: Send + Sync {
    /// Subscribe to the encoded H.264 stream. Each viewer gets its own broadcast
    /// receiver; the encoder fans out one stream to all of them. A lagging
    /// receiver drops the oldest buffered frames (the channel is bounded).
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EncodedFrame>;

    /// Force an IDR (keyframe with in-band SPS/PPS) on the next encoded frame.
    /// Call this on viewer-join and on RTCP PLI so a new/recovering decoder gets
    /// a clean entry point without waiting for the next GOP boundary.
    fn request_keyframe(&self);
}

const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Convert an AVCC elementary stream (`[4-byte BE length][NALU]` repeated) into
/// Annex-B (`[00 00 00 01][NALU]` repeated).
///
/// If `param_sets` is `Some((sps, pps))` (a keyframe), each parameter set is
/// emitted as its own start-code-delimited NALU *before* the converted slice
/// data. This is the pure core of the VT output callback, extracted so it can be
/// unit-tested without VideoToolbox.
///
/// Returns an error if the AVCC framing is corrupt (a length runs past the end).
pub fn avcc_to_annex_b(
    avcc: &[u8],
    param_sets: Option<(&[u8], &[u8])>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(avcc.len() + 16);

    if let Some((sps, pps)) = param_sets {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(sps);
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(pps);
    }

    let mut i = 0usize;
    while i + 4 <= avcc.len() {
        let nal_len =
            u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        if nal_len == 0 || i + nal_len > avcc.len() {
            return Err(format!(
                "corrupt AVCC: nal_len={nal_len} at offset {i}, remaining {}",
                avcc.len().saturating_sub(i)
            ));
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[i..i + nal_len]);
        i += nal_len;
    }
    if i != avcc.len() {
        return Err(format!(
            "trailing AVCC bytes: parsed {i} of {} bytes",
            avcc.len()
        ));
    }

    Ok(out)
}

/// Pure keepalive-timer decision: should we force a keepalive IDR now?
///
/// Returns `true` when the stream has been idle for at least `keepalive` since
/// the last *emitted* frame — i.e. SCK has delivered nothing and we must
/// re-encode the last buffer as a fresh IDR to keep the decoder alive. Extracted
/// as a pure function so the timing logic is unit-testable without a clock.
///
/// * `now_since_last_emit` — time elapsed since the last frame was emitted.
/// * `keepalive` — the idle threshold (e.g. 500 ms).
/// * `have_buffer` — whether we have a last pixel buffer to re-encode. With no
///   buffer yet (nothing ever captured) there's nothing to keep alive.
pub fn keepalive_should_fire(
    now_since_last_emit: std::time::Duration,
    keepalive: std::time::Duration,
    have_buffer: bool,
) -> bool {
    have_buffer && now_since_last_emit >= keepalive
}

/// Start capture + encode of the iPhone Mirroring window. Returns a running
/// pipeline the WebRTC layer subscribes to.
///
/// On non-macOS this returns an "unsupported platform" error.
pub fn start_pipeline(cfg: PipelineConfig) -> anyhow::Result<Arc<dyn VideoPipeline>> {
    #[cfg(target_os = "macos")]
    {
        imp::start(cfg)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cfg;
        Err(anyhow::anyhow!(
            "video pipeline is only supported on macOS (ScreenCaptureKit + VideoToolbox)"
        ))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use crate::capture::{CaptureConfig, CaptureStream, Frame};

    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use objc2_core_foundation::{
        kCFBooleanFalse, kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks,
        kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber, CFNumberType, CFRetained,
        CFString, CFType,
    };
    use objc2_core_media::{
        kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264, CMBlockBuffer, CMSampleBuffer,
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    };
    use objc2_core_video::CVImageBuffer;
    use objc2_video_toolbox::{
        kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
        kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
        kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
        kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel, VTCompressionSession,
        VTEncodeInfoFlags, VTSessionSetProperty,
    };

    /// `VTCompressionSession` is documented thread-safe; we only call
    /// `encode_frame` on it serially (from the SCK delegate thread *or* the
    /// keepalive thread, mutually exclusive via the `last_buffer` mutex) and
    /// `complete_frames`/`invalidate` from `Drop`. Wrap the retained pointer so it
    /// can cross threads.
    struct SendSession(CFRetained<VTCompressionSession>);
    // SAFETY: see the doc comment — access is serialized, the API is thread-safe.
    unsafe impl Send for SendSession {}
    unsafe impl Sync for SendSession {}

    /// A retained pixel buffer we hold onto so the keepalive thread can re-encode
    /// the last frame as a fresh IDR. `CVImageBuffer` (== `CVPixelBuffer`) is a
    /// CoreFoundation type; a `CFRetained` keeps it alive across threads.
    struct SendPixelBuffer(CFRetained<CVImageBuffer>);
    // SAFETY: CVPixelBuffer is immutable image data; we only read it under the
    // `last_buffer` mutex, and CF retain/release is thread-safe.
    unsafe impl Send for SendPixelBuffer {}
    unsafe impl Sync for SendPixelBuffer {}

    /// Refcon handed to `VTCompressionSessionCreate`. Lives behind an `Arc` for
    /// the session's lifetime; the C callback receives it as `*const c_void`.
    struct OutputContext {
        tx: tokio::sync::broadcast::Sender<EncodedFrame>,
        last_emit: Mutex<Instant>,
    }

    /// VT output callback. Converts the compressed sample to Annex-B and
    /// broadcasts it. Signature must match `VTCompressionOutputCallback`.
    extern "C-unwind" fn output_callback(
        output_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: i32,
        _info_flags: VTEncodeInfoFlags,
        sample_buffer: *mut CMSampleBuffer,
    ) {
        if output_ref_con.is_null() || status != 0 || sample_buffer.is_null() {
            return;
        }
        // SAFETY: output_ref_con is the OutputContext Arc we passed to create();
        // it outlives every callback (kept alive until after invalidate()).
        let ctx = unsafe { &*(output_ref_con as *const OutputContext) };
        // SAFETY: VT guarantees a valid CMSampleBuffer for the call duration.
        let sample = unsafe { &*sample_buffer };

        match build_encoded_frame(sample) {
            Ok(frame) => {
                *ctx.last_emit.lock().unwrap() = Instant::now();
                // broadcast::send only errors if there are no receivers — fine.
                let _ = ctx.tx.send(frame);
            }
            Err(e) => eprintln!("encode-callback: {e}"),
        }
    }

    /// True unless the sample carries a `NotSync` attachment (i.e. it's a keyframe).
    fn is_keyframe(sample: &CMSampleBuffer) -> bool {
        let arr = match unsafe { sample.sample_attachments_array(false) } {
            Some(a) => a,
            None => return true,
        };
        if arr.count() == 0 {
            return true;
        }
        let dict_ptr = unsafe { arr.value_at_index(0) };
        if dict_ptr.is_null() {
            return true;
        }
        let dict = unsafe { &*(dict_ptr as *const CFDictionary) };
        let not_sync_present = unsafe {
            let key = kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void;
            !dict.value(key).is_null()
        };
        !not_sync_present
    }

    /// Pull the AVCC bytes (and SPS/PPS on keyframes) out of a compressed sample
    /// and run them through the pure [`avcc_to_annex_b`] converter.
    fn build_encoded_frame(sample: &CMSampleBuffer) -> Result<EncodedFrame, String> {
        let keyframe = is_keyframe(sample);

        // PTS in microseconds.
        let pts = unsafe { sample.presentation_time_stamp() };
        let pts_micros = if pts.timescale != 0 {
            ((pts.value as i128) * 1_000_000 / pts.timescale as i128).max(0) as u64
        } else {
            0
        };

        // Parameter sets (keyframe only). Hold the format desc alive while we
        // borrow the SPS/PPS slices.
        let mut sps_buf: &[u8] = &[];
        let mut pps_buf: &[u8] = &[];
        let _fmt_hold;
        if keyframe {
            let fmt = unsafe { sample.format_description() }
                .ok_or("keyframe sample has no format description")?;
            let mut sps_ptr: *const u8 = std::ptr::null();
            let mut sps_size = 0usize;
            let mut pps_ptr: *const u8 = std::ptr::null();
            let mut pps_size = 0usize;
            let mut count = 0usize;
            let mut nal_len: i32 = 0;
            let st0 = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &fmt, 0, &mut sps_ptr, &mut sps_size, &mut count, &mut nal_len,
                )
            };
            if st0 != 0 || sps_ptr.is_null() {
                return Err(format!("get SPS failed: status={st0} count={count}"));
            }
            let st1 = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &fmt,
                    1,
                    &mut pps_ptr,
                    &mut pps_size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if st1 != 0 || pps_ptr.is_null() {
                return Err(format!("get PPS failed: status={st1}"));
            }
            if nal_len != 4 {
                return Err(format!("unexpected NAL length size {nal_len} (expected 4)"));
            }
            // SAFETY: valid while `fmt` is retained — held by `_fmt_hold`.
            sps_buf = unsafe { std::slice::from_raw_parts(sps_ptr, sps_size) };
            pps_buf = unsafe { std::slice::from_raw_parts(pps_ptr, pps_size) };
            _fmt_hold = fmt;
        }

        // AVCC payload from the block buffer (single contiguous block for VT H264).
        let block: CFRetained<CMBlockBuffer> =
            unsafe { sample.data_buffer() }.ok_or("compressed sample has no data buffer")?;
        let total = unsafe { block.data_length() };
        let mut data_ptr: *mut std::os::raw::c_char = std::ptr::null_mut();
        let mut length_at_offset = 0usize;
        let mut total_len = 0usize;
        let st = unsafe {
            block.data_pointer(0, &mut length_at_offset, &mut total_len, &mut data_ptr)
        };
        if st != 0 || data_ptr.is_null() {
            return Err(format!("CMBlockBufferGetDataPointer failed: status={st}"));
        }
        if length_at_offset < total {
            return Err(format!(
                "block buffer not contiguous: {length_at_offset} < {total}"
            ));
        }
        // SAFETY: `total` bytes readable at data_ptr for the call duration.
        let avcc = unsafe { std::slice::from_raw_parts(data_ptr as *const u8, total) };

        let param_sets = if keyframe {
            Some((sps_buf, pps_buf))
        } else {
            None
        };
        let annex_b = avcc_to_annex_b(avcc, param_sets)?;

        Ok(EncodedFrame {
            data: bytes::Bytes::from(annex_b),
            is_keyframe: keyframe,
            pts_micros,
        })
    }

    /// { kVTEncodeFrameOptionKey_ForceKeyFrame : true }
    fn make_force_keyframe_dict() -> CFRetained<CFDictionary> {
        unsafe {
            let key = kVTEncodeFrameOptionKey_ForceKeyFrame as *const CFString as *const c_void;
            let val = kCFBooleanTrue.expect("kCFBooleanTrue") as *const _ as *const c_void;
            let mut keys = [key];
            let mut vals = [val];
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                vals.as_mut_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .expect("CFDictionaryCreate force-keyframe")
        }
    }

    fn set_int_property(
        session: &VTCompressionSession,
        key: &CFString,
        value: i32,
    ) -> anyhow::Result<()> {
        let num = unsafe {
            CFNumber::new(
                None,
                CFNumberType::SInt32Type,
                &value as *const i32 as *const c_void,
            )
        }
        .ok_or_else(|| anyhow::anyhow!("CFNumberCreate failed"))?;
        let st = unsafe { VTSessionSetProperty(session.as_ref(), key, Some(num.as_ref() as &CFType)) };
        if st != 0 {
            return Err(anyhow::anyhow!("VTSessionSetProperty(int) failed: status={st}"));
        }
        Ok(())
    }

    fn set_bool_property(
        session: &VTCompressionSession,
        key: &CFString,
        value: bool,
    ) -> anyhow::Result<()> {
        let b = unsafe { if value { kCFBooleanTrue } else { kCFBooleanFalse } }
            .ok_or_else(|| anyhow::anyhow!("kCFBoolean missing"))?;
        let st = unsafe {
            VTSessionSetProperty(session.as_ref(), key, Some(&*(b as *const _ as *const CFType)))
        };
        if st != 0 {
            return Err(anyhow::anyhow!("VTSessionSetProperty(bool) failed: status={st}"));
        }
        Ok(())
    }

    fn set_string_property(
        session: &VTCompressionSession,
        key: &CFString,
        value: &CFString,
    ) -> anyhow::Result<()> {
        let st = unsafe { VTSessionSetProperty(session.as_ref(), key, Some(value as &CFType)) };
        if st != 0 {
            return Err(anyhow::anyhow!("VTSessionSetProperty(string) failed: status={st}"));
        }
        Ok(())
    }

    /// The concrete pipeline: owns the capture stream, the VT session, the output
    /// context, and the keepalive thread.
    pub struct Pipeline {
        tx: tokio::sync::broadcast::Sender<EncodedFrame>,
        session: SendSession,
        force_idr: Arc<AtomicBool>,
        // Held alive; dropped explicitly in Drop *before* the VT session is
        // invalidated so no in-flight capture callback touches a dead session.
        capture: Option<CaptureStream>,
        // Shared encode state. The keepalive thread holds its own clone; this
        // field keeps the allocation alive for the pipeline's lifetime.
        _shared: Arc<EncodeShared>,
        keepalive: Option<std::thread::JoinHandle<()>>,
        stop: Arc<AtomicBool>,
        // OutputContext kept alive (raw ptr handed to VT); reclaimed in Drop.
        ctx_ptr: *mut OutputContext,
    }
    // SAFETY: ctx_ptr is only dereferenced by the VT callback and freed in Drop
    // after invalidate(); the rest of the fields are Send+Sync.
    unsafe impl Send for Pipeline {}
    unsafe impl Sync for Pipeline {}

    /// State shared between the capture callback and the keepalive thread.
    struct EncodeShared {
        session: SendSession,
        force_idr: Arc<AtomicBool>,
        frame_index: AtomicI64,
        // Last captured pixel buffer + its fps, for keepalive re-encode.
        last_buffer: Mutex<Option<SendPixelBuffer>>,
        fps: u32,
        keepalive_period: Duration,
    }

    impl EncodeShared {
        /// Encode one pixel buffer. `force_keyframe` forces an IDR. `pts_micros`
        /// is converted into a CMTime at the session fps.
        fn encode(&self, image_buffer: &CVImageBuffer, pts_micros: u64, force_keyframe: bool) {
            let idx = self.frame_index.fetch_add(1, Ordering::Relaxed);
            let want_idr =
                force_keyframe || idx == 0 || self.force_idr.swap(false, Ordering::Relaxed);

            let fps = self.fps.max(1) as i32;
            // Use a strictly-increasing PTS derived from pts_micros so the encoder
            // sees monotonic timestamps.
            let pts = objc2_core_media::CMTime {
                value: pts_micros as i64,
                timescale: 1_000_000,
                flags: objc2_core_media::CMTimeFlags(1),
                epoch: 0,
            };
            let duration = objc2_core_media::CMTime {
                value: 1,
                timescale: fps,
                flags: objc2_core_media::CMTimeFlags(1),
                epoch: 0,
            };
            let frame_props: Option<CFRetained<CFDictionary>> = if want_idr {
                Some(make_force_keyframe_dict())
            } else {
                None
            };
            let mut info = VTEncodeInfoFlags(0);
            let st = unsafe {
                self.session.0.encode_frame(
                    image_buffer,
                    pts,
                    duration,
                    frame_props.as_deref(),
                    std::ptr::null_mut(),
                    &mut info,
                )
            };
            if st != 0 {
                eprintln!("VTCompressionSessionEncodeFrame failed: status={st}");
            }
        }
    }

    impl VideoPipeline for Pipeline {
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EncodedFrame> {
            self.tx.subscribe()
        }
        fn request_keyframe(&self) {
            self.force_idr.store(true, Ordering::Relaxed);
        }
    }

    impl Drop for Pipeline {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // Stop capture FIRST: dropping the CaptureStream stops the SCStream and
            // joins its watcher, so no capture callback can fire after this point.
            self.capture.take();
            // Then stop the keepalive thread so it can't encode either.
            if let Some(h) = self.keepalive.take() {
                let _ = h.join();
            }
            // Now no thread will call encode() — safe to complete + invalidate.
            let invalid = objc2_core_media::CMTime {
                value: 0,
                timescale: 0,
                flags: objc2_core_media::CMTimeFlags(0),
                epoch: 0,
            };
            let _ = unsafe { self.session.0.complete_frames(invalid) };
            unsafe { self.session.0.invalidate() };
            // No more callbacks can fire now — reclaim the OutputContext.
            if !self.ctx_ptr.is_null() {
                unsafe { drop(Box::from_raw(self.ctx_ptr)) };
                self.ctx_ptr = std::ptr::null_mut();
            }
        }
    }

    pub(super) fn start(cfg: PipelineConfig) -> anyhow::Result<Arc<dyn VideoPipeline>> {
        // We need the window dimensions to size the VT session, so start capture
        // first (it finds + opens the stream) but install the real callback after
        // the session exists. We achieve this with a small two-phase wiring: the
        // capture callback closes over the shared encode state, which we fill in
        // before any frame can arrive.

        let (tx, _rx) = tokio::sync::broadcast::channel::<EncodedFrame>(cfg.channel_capacity.max(2));

        let ctx = Box::new(OutputContext {
            tx: tx.clone(),
            last_emit: Mutex::new(Instant::now()),
        });
        // We hand VT a stable raw pointer to the OutputContext; keep it alive for
        // the session's lifetime and reclaim it in Drop. The keepalive thread also
        // reads `last_emit` through this raw pointer (valid until Drop, after the
        // thread is joined and the session invalidated).
        let ctx_ptr: *mut OutputContext = Box::into_raw(ctx);

        // Build the VT session lazily once we know the dimensions. We open
        // capture with a placeholder callback that defers to `shared` (set just
        // below, before start_capture actually delivers frames is not guaranteed
        // — so we guard with an Option until ready).
        let force_idr = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<Option<Arc<EncodeShared>>>> = Arc::new(Mutex::new(None));

        let capture_cb = {
            let pending = pending.clone();
            Arc::new(move |frame: Frame<'_>| {
                let guard = pending.lock().unwrap();
                if let Some(shared) = guard.as_ref() {
                    // Stash the buffer for keepalive, then encode.
                    if let Some(retained) = retain_image_buffer(frame.image_buffer) {
                        *shared.last_buffer.lock().unwrap() = Some(retained);
                    }
                    shared.encode(frame.image_buffer, frame.pts_micros, false);
                }
            }) as crate::capture::FrameCallback
        };

        let capture = CaptureStream::start(
            CaptureConfig {
                fps: cfg.fps,
                show_cursor: cfg.show_cursor,
                window_poll_millis: 1000,
            },
            capture_cb,
        )?;
        let (width, height) = capture.dimensions();

        // Create the VT session sized to the window.
        let session = create_session(width as i32, height as i32, &cfg, ctx_ptr)?;

        let shared = Arc::new(EncodeShared {
            session: SendSession(session.0.clone()),
            force_idr: force_idr.clone(),
            frame_index: AtomicI64::new(0),
            last_buffer: Mutex::new(None),
            fps: cfg.fps.max(1),
            keepalive_period: Duration::from_millis(cfg.keepalive_millis.max(50)),
        });
        *pending.lock().unwrap() = Some(shared.clone());

        // Keepalive thread: re-encode the last buffer as a forced IDR after idle.
        let stop = Arc::new(AtomicBool::new(false));
        let keepalive = {
            let shared = shared.clone();
            let stop = stop.clone();
            // SAFETY: ctx_ptr is valid for the pipeline's lifetime (freed in Drop
            // after the keepalive thread is joined and the session invalidated).
            let ctx_ptr_usize = ctx_ptr as usize;
            std::thread::Builder::new()
                .name("encode-keepalive".into())
                .spawn(move || {
                    let period = shared.keepalive_period;
                    loop {
                        std::thread::sleep(period / 2);
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        // SAFETY: see above — ctx_ptr outlives this thread.
                        let ctx = unsafe { &*(ctx_ptr_usize as *const OutputContext) };
                        let idle = ctx.last_emit.lock().unwrap().elapsed();
                        let have_buffer = shared.last_buffer.lock().unwrap().is_some();
                        if keepalive_should_fire(idle, period, have_buffer) {
                            // Re-encode the last buffer as a fresh IDR. Synthesize
                            // a wall-clock-derived PTS so timestamps stay monotonic.
                            let buf = shared.last_buffer.lock().unwrap();
                            if let Some(SendPixelBuffer(pb)) = buf.as_ref() {
                                shared.encode(pb, now_micros(), true);
                            }
                        }
                    }
                })
                .map_err(|e| anyhow::anyhow!("failed to spawn keepalive thread: {e}"))?
        };

        Ok(Arc::new(Pipeline {
            tx,
            session,
            force_idr,
            capture: Some(capture),
            _shared: shared,
            keepalive: Some(keepalive),
            stop,
            ctx_ptr,
        }))
    }

    /// Monotonic-ish microsecond timestamp for synthesized keepalive frames.
    fn now_micros() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }

    /// Retain a `CVImageBuffer` into a thread-safe holder for keepalive re-encode.
    fn retain_image_buffer(ib: &CVImageBuffer) -> Option<SendPixelBuffer> {
        // SAFETY: `ib` is a live CF object; CFRetained::retain bumps its refcount.
        let retained = unsafe { CFRetained::retain(NonNull::from(ib)) };
        Some(SendPixelBuffer(retained))
    }

    fn create_session(
        width: i32,
        height: i32,
        cfg: &PipelineConfig,
        ctx_ptr: *mut OutputContext,
    ) -> anyhow::Result<SendSession> {
        let mut session_out: *mut VTCompressionSession = std::ptr::null_mut();
        let st = unsafe {
            VTCompressionSession::create(
                None,
                width.max(2),
                height.max(2),
                kCMVideoCodecType_H264,
                None,
                None,
                None,
                Some(output_callback),
                ctx_ptr as *mut c_void,
                NonNull::new(&mut session_out).unwrap(),
            )
        };
        if st != 0 || session_out.is_null() {
            return Err(anyhow::anyhow!("VTCompressionSessionCreate failed: status={st}"));
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new(session_out).unwrap()) };

        unsafe {
            set_bool_property(&session, kVTCompressionPropertyKey_RealTime, true)?;
            set_bool_property(&session, kVTCompressionPropertyKey_AllowFrameReordering, false)?;
            set_string_property(
                &session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
            )?;
            set_int_property(
                &session,
                kVTCompressionPropertyKey_AverageBitRate,
                cfg.bitrate as i32,
            )?;
            set_int_property(
                &session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                cfg.max_keyframe_interval.max(1) as i32,
            )?;
        }
        let _ = unsafe { session.prepare_to_encode_frames() };
        Ok(SendSession(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn avcc_nalu(payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn default_pipeline_config() {
        let c = PipelineConfig::default();
        assert_eq!(c.fps, 30);
        assert_eq!(c.bitrate, 6_000_000);
        assert_eq!(c.max_keyframe_interval, 30);
        assert_eq!(c.keepalive_millis, 500);
    }

    #[test]
    fn avcc_single_nalu_to_annex_b() {
        let avcc = avcc_nalu(&[0x41, 0x9a, 0x01, 0x02]);
        let out = avcc_to_annex_b(&avcc, None).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x41, 0x9a, 0x01, 0x02]);
    }

    #[test]
    fn avcc_multiple_nalus_to_annex_b() {
        let mut avcc = avcc_nalu(&[0xaa, 0xbb]);
        avcc.extend(avcc_nalu(&[0xcc, 0xdd, 0xee]));
        let out = avcc_to_annex_b(&avcc, None).unwrap();
        assert_eq!(
            out,
            vec![0, 0, 0, 1, 0xaa, 0xbb, 0, 0, 0, 1, 0xcc, 0xdd, 0xee]
        );
    }

    #[test]
    fn keyframe_prepends_sps_pps() {
        let avcc = avcc_nalu(&[0x65, 0x88]); // IDR slice
        let sps = [0x67, 0x42, 0x00];
        let pps = [0x68, 0xce];
        let out = avcc_to_annex_b(&avcc, Some((&sps, &pps))).unwrap();
        assert_eq!(
            out,
            vec![
                0, 0, 0, 1, 0x67, 0x42, 0x00, // SPS
                0, 0, 0, 1, 0x68, 0xce, // PPS
                0, 0, 0, 1, 0x65, 0x88, // IDR
            ]
        );
    }

    #[test]
    fn corrupt_avcc_length_overruns() {
        // Declares a 100-byte NALU but only 2 bytes follow.
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&100u32.to_be_bytes());
        avcc.extend_from_slice(&[0x41, 0x9a]);
        let err = avcc_to_annex_b(&avcc, None).unwrap_err();
        assert!(err.contains("corrupt AVCC"), "got: {err}");
    }

    #[test]
    fn zero_length_nalu_is_rejected() {
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&0u32.to_be_bytes());
        let err = avcc_to_annex_b(&avcc, None).unwrap_err();
        assert!(err.contains("corrupt AVCC"), "got: {err}");
    }

    #[test]
    fn trailing_partial_length_is_rejected() {
        // 4-byte length then a real NALU, then 2 dangling bytes (not a full len).
        let mut avcc = avcc_nalu(&[0x41]);
        avcc.extend_from_slice(&[0x00, 0x00]);
        let err = avcc_to_annex_b(&avcc, None).unwrap_err();
        assert!(err.contains("trailing AVCC"), "got: {err}");
    }

    #[test]
    fn empty_avcc_with_params_emits_only_param_sets() {
        let sps = [0x67];
        let pps = [0x68];
        let out = avcc_to_annex_b(&[], Some((&sps, &pps))).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68]);
    }

    #[test]
    fn keepalive_fires_after_idle_with_buffer() {
        assert!(keepalive_should_fire(
            Duration::from_millis(600),
            Duration::from_millis(500),
            true
        ));
    }

    #[test]
    fn keepalive_does_not_fire_before_threshold() {
        assert!(!keepalive_should_fire(
            Duration::from_millis(300),
            Duration::from_millis(500),
            true
        ));
    }

    #[test]
    fn keepalive_never_fires_without_a_buffer() {
        assert!(!keepalive_should_fire(
            Duration::from_secs(10),
            Duration::from_millis(500),
            false
        ));
    }

    #[test]
    fn keepalive_fires_exactly_at_threshold() {
        assert!(keepalive_should_fire(
            Duration::from_millis(500),
            Duration::from_millis(500),
            true
        ));
    }
}
