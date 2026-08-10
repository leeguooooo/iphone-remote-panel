//! Spike S0b-1 — ScreenCaptureKit capture + VideoToolbox H.264 encode-to-file
//!
//! Purpose: prove that the iPhone Mirroring window, once captured as BGRA frames
//! (validated by S0a / s0_capture), can be **hardware-encoded to realtime H.264**
//! by VideoToolbox and written to a **playable file** on disk. This is the encode
//! half of the eventual WebRTC pipe — if VT can't produce a clean Annex-B stream
//! here, the Safari/WebRTC step downstream is hopeless.
//!
//! This is a throwaway validation probe. It does NOT decide PASS/FAIL — it
//! produces an artifact (`s0b.h264`) a human plays back. See the RUNBOOK in the
//! task report for how to judge it.
//!
//! What it does:
//!   1. Same capture front-half as s0_capture: NSApplicationLoad(), find the
//!      iPhone Mirroring window, open an SCStream filtered to it (BGRA, ~30fps).
//!   2. Creates a VTCompressionSession for H.264:
//!        - Constrained Baseline profile (AutoLevel)
//!        - RealTime = true
//!        - AllowFrameReordering = false (no B-frames — low latency)
//!        - AverageBitRate ~6 Mbps, MaxKeyFrameInterval = 30 (1s @ 30fps)
//!      Forces a keyframe on the FIRST frame.
//!   3. For each captured CVPixelBuffer, calls VTCompressionSessionEncodeFrame.
//!      The async output callback converts each compressed CMSampleBuffer from
//!      AVCC (4-byte length-prefixed NALUs) to **Annex-B** (00 00 00 01 start
//!      codes), prepending SPS+PPS (pulled from the format description) before
//!      every keyframe. Bytes are shipped to main over a channel.
//!   4. Collects ~150 frames (~5s), writes them to ./s0b.h264, prints per-frame
//!      encoded size + keyframe flag, and the SPS/PPS sizes once.
//!
//! Usage:  s0b_encode
//!
//! Requires: Screen Recording permission (TCC) granted to the terminal / binary,
//! a live iPhone Mirroring window. Without the grant the stream produces nothing
//! (the capture probe S0a covers that failure mode).
//!
//! Idle SCK samples surface as `get_pixel_buffer: CouldNotGetDataBuffer` and are
//! dropped silently — this is normal ScreenCaptureKit behaviour, not an error.

use std::ffi::c_void;
use std::io::Write as _;
use std::ptr::NonNull;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::Duration;

use core_media_rs::cm_sample_buffer::CMSampleBuffer as SckSampleBuffer;
use core_media_rs::cm_time::CMTime as SckCMTime;

// core-foundation 0.10 (what core-video-rs is built on) — used only to pull the
// raw CVPixelBufferRef out of the SCK-captured pixel buffer.
use core_foundation::base::TCFType;

// objc2 framework crates — the real VideoToolbox / CoreMedia / CoreFoundation API.
use objc2_core_foundation::{
    kCFBooleanFalse, kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString,
    CFType,
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
    kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel, VTCompressionSession, VTEncodeInfoFlags,
    VTSessionSetProperty,
};

use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::pixel_format::PixelFormat;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;

const TARGET_FRAMES: usize = 150; // ~5s @ 30fps
const FPS: i32 = 30;
const BITRATE: i32 = 6_000_000; // 6 Mbps
const OUT_FILE: &str = "s0b.h264";

/// One encoded access unit, already converted to Annex-B (with SPS/PPS prepended
/// for keyframes), detached so it can cross the channel to the writer thread.
struct EncodedUnit {
    annex_b: Vec<u8>,
    is_keyframe: bool,
    /// SPS/PPS sizes, only set the first time we emit them (for the one-shot log).
    param_sizes: Option<(usize, usize)>,
}

// ---------------------------------------------------------------------------
// VideoToolbox output callback
// ---------------------------------------------------------------------------

/// The refcon handed to VTCompressionSessionCreate. Lives on the heap for the
/// session's lifetime; the C callback receives it as a `*mut c_void`.
struct OutputContext {
    tx: SyncSender<EncodedUnit>,
    /// Set once, after we've logged the first SPS/PPS, so we don't re-log.
    logged_params: std::cell::Cell<bool>,
}

/// VTCompressionOutputCallback. Called (possibly async, on a VT thread) once per
/// compressed frame. Extracts the encoded bytes, converts AVCC -> Annex-B,
/// prepends SPS/PPS on keyframes, and ships the result to the writer thread.
///
/// Signature must match `VTCompressionOutputCallback`:
///   fn(*mut c_void, *mut c_void, OSStatus, VTEncodeInfoFlags, *mut CMSampleBuffer)
extern "C-unwind" fn output_callback(
    output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    if output_ref_con.is_null() {
        return;
    }
    // SAFETY: output_ref_con is the OutputContext we passed to create(); it
    // outlives every callback (we keep the Box alive until after the session is
    // invalidated). We only take a shared &.
    let ctx = unsafe { &*(output_ref_con as *const OutputContext) };

    if status != 0 || sample_buffer.is_null() {
        // Dropped frame or encode error — nothing to emit. (Not fatal for a probe.)
        return;
    }
    // SAFETY: VT guarantees a valid CMSampleBuffer here for the duration of the call.
    let sample = unsafe { &*sample_buffer };

    match build_annex_b(sample, ctx) {
        Ok(unit) => {
            let _ = ctx.tx.try_send(unit);
        }
        Err(e) => eprintln!("encode-callback: {e}"),
    }
}

/// Determine keyframe-ness from the sample attachments: a frame is a sync sample
/// (keyframe) unless `kCMSampleAttachmentKey_NotSync` is present and true.
fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    // SAFETY: create_if_necessary=false; returns the immutable attachments array.
    let arr = match unsafe { sample.sample_attachments_array(false) } {
        Some(a) => a,
        None => return true, // no attachments -> treat as sync/keyframe
    };
    if arr.count() == 0 {
        return true;
    }
    // dict for sample 0
    // SAFETY: index 0 is in range (count > 0); value is a CFDictionary.
    let dict_ptr = unsafe { arr.value_at_index(0) };
    if dict_ptr.is_null() {
        return true;
    }
    let dict = unsafe { &*(dict_ptr as *const CFDictionary) };
    // SAFETY: kCMSampleAttachmentKey_NotSync is a valid CFString key pointer.
    let not_sync_present = unsafe {
        let key = kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void;
        !dict.value(key).is_null()
    };
    // If NotSync is present at all, this is a non-keyframe (its value is true).
    !not_sync_present
}

/// Convert one compressed CMSampleBuffer (AVCC, length-prefixed NALUs) into an
/// Annex-B byte vector. For keyframes, SPS+PPS (from the format description) are
/// prepended, each as its own start-code-delimited NALU.
fn build_annex_b(sample: &CMSampleBuffer, ctx: &OutputContext) -> Result<EncodedUnit, String> {
    const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
    let keyframe = is_keyframe(sample);
    let mut out: Vec<u8> = Vec::new();
    let mut param_sizes: Option<(usize, usize)> = None;

    if keyframe {
        // Pull SPS (index 0) and PPS (index 1) out of the AVC format description.
        // SAFETY: format_description() returns the desc owned by the sample buffer.
        let fmt = unsafe { sample.format_description() }
            .ok_or("keyframe sample has no format description")?;
        let mut sps_ptr: *const u8 = std::ptr::null();
        let mut sps_size: usize = 0;
        let mut pps_ptr: *const u8 = std::ptr::null();
        let mut pps_size: usize = 0;
        let mut count: usize = 0;
        let mut nal_len: i32 = 0;

        // SAFETY: all out-params are valid pointers; fmt is a live H264 video desc.
        let st0 = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &fmt,
                0,
                &mut sps_ptr,
                &mut sps_size,
                &mut count,
                &mut nal_len,
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

        // SAFETY: pointers/sizes come from CoreMedia; valid while fmt is retained.
        let sps = unsafe { std::slice::from_raw_parts(sps_ptr, sps_size) };
        let pps = unsafe { std::slice::from_raw_parts(pps_ptr, pps_size) };

        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(sps);
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(pps);

        if !ctx.logged_params.get() {
            ctx.logged_params.set(true);
            param_sizes = Some((sps_size, pps_size));
        }

        // NB: VideoToolbox writes AVCC NALUs with a 4-byte length prefix; we
        // assume nal_len == 4 below (the standard, and what we assert on).
        if nal_len != 4 {
            return Err(format!("unexpected NAL length size {nal_len} (expected 4)"));
        }
    }

    // Walk the AVCC block buffer: [4-byte BE length][NALU bytes] repeated.
    // SAFETY: a compressed sample buffer always carries a CMBlockBuffer.
    let block: CFRetained<CMBlockBuffer> =
        unsafe { sample.data_buffer() }.ok_or("compressed sample has no data buffer")?;

    let total = unsafe { block.data_length() };
    let mut data_ptr: *mut std::os::raw::c_char = std::ptr::null_mut();
    let mut length_at_offset: usize = 0;
    let mut total_len: usize = 0;
    // SAFETY: standard CMBlockBufferGetDataPointer call; out-params are valid.
    let st = unsafe { block.data_pointer(0, &mut length_at_offset, &mut total_len, &mut data_ptr) };
    if st != 0 || data_ptr.is_null() {
        return Err(format!("CMBlockBufferGetDataPointer failed: status={st}"));
    }
    // We require a single contiguous block (true for VT H.264 output).
    if length_at_offset < total {
        return Err(format!(
            "block buffer not contiguous: {length_at_offset} < {total}"
        ));
    }
    // SAFETY: `total` bytes are readable at data_ptr.
    let data = unsafe { std::slice::from_raw_parts(data_ptr as *const u8, total) };

    let mut i = 0usize;
    while i + 4 <= data.len() {
        let nal_len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if nal_len == 0 || i + nal_len > data.len() {
            return Err(format!(
                "corrupt AVCC: nal_len={nal_len} at offset {i}, remaining {}",
                data.len() - i
            ));
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[i..i + nal_len]);
        i += nal_len;
    }

    Ok(EncodedUnit {
        annex_b: out,
        is_keyframe: keyframe,
        param_sizes,
    })
}

// ---------------------------------------------------------------------------
// Capture handler — encodes each captured frame on the SCK delegate thread
// ---------------------------------------------------------------------------

/// `CFRetained<VTCompressionSession>` isn't auto-`Send`/`Sync` (it holds a raw
/// pointer), but a VTCompressionSession is documented thread-safe and we only
/// ever call `encode_frame` on it serially from the single SCK delegate thread
/// (and `complete_frames`/`invalidate` from main *after* the stream is stopped).
/// Wrap it so the handler can satisfy `SCStreamOutputTrait: Send`.
struct SendSession(CFRetained<VTCompressionSession>);
// SAFETY: see the doc comment — access is serialized, the API is thread-safe.
unsafe impl Send for SendSession {}
unsafe impl Sync for SendSession {}

struct CaptureHandler {
    session: SendSession,
    frame_index: std::cell::Cell<i64>,
}

impl SCStreamOutputTrait for CaptureHandler {
    fn did_output_sample_buffer(
        &self,
        sample_buffer: SckSampleBuffer,
        of_type: SCStreamOutputType,
    ) {
        if of_type != SCStreamOutputType::Screen {
            return;
        }
        // Idle SCK frames carry no image buffer -> CouldNotGetDataBuffer. Normal;
        // drop silently.
        let pixel_buffer = match sample_buffer.get_pixel_buffer() {
            Ok(pb) => pb,
            Err(_) => return,
        };

        // Bridge: core-video-rs CVPixelBuffer -> raw CVPixelBufferRef -> objc2
        // CVImageBuffer. Same underlying Apple object; VT takes a CVImageBuffer.
        let raw = pixel_buffer.as_concrete_TypeRef(); // *mut __CVPixelBufferRef
        if raw.is_null() {
            return;
        }
        // SAFETY: the raw pointer is a live CVPixelBuffer (== CVImageBuffer)
        // retained by `pixel_buffer` for the duration of this call.
        let image_buffer = unsafe { &*(raw as *const CVImageBuffer) };

        let idx = self.frame_index.get();
        self.frame_index.set(idx + 1);

        // Strictly increasing PTS at FPS; duration = 1 frame.
        let pts = objc2_core_media::CMTime {
            value: idx,
            timescale: FPS,
            flags: objc2_core_media::CMTimeFlags(1), // kCMTimeFlags_Valid
            epoch: 0,
        };
        let duration = objc2_core_media::CMTime {
            value: 1,
            timescale: FPS,
            flags: objc2_core_media::CMTimeFlags(1),
            epoch: 0,
        };

        // Force a keyframe on the very first frame so SPS/PPS land up front.
        let frame_props: Option<CFRetained<CFDictionary>> = if idx == 0 {
            Some(make_force_keyframe_dict())
        } else {
            None
        };

        let mut info = VTEncodeInfoFlags(0);
        // SAFETY: image_buffer is a live CVImageBuffer; frame_props (if any) holds
        // the correct key/value types; null refcon/info are acceptable.
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

// ---------------------------------------------------------------------------
// CF helpers
// ---------------------------------------------------------------------------

/// Build { kVTEncodeFrameOptionKey_ForceKeyFrame : kCFBooleanTrue }.
fn make_force_keyframe_dict() -> CFRetained<CFDictionary> {
    // SAFETY: the static key/value are valid CF objects; we build a 1-entry dict
    // using the default CF-type retain/release callbacks.
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

/// Set a session property to a CFNumber(i32).
fn set_int_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: i32,
) -> Result<(), String> {
    // SAFETY: build a CFNumber from an i32 and hand it to VTSessionSetProperty.
    let num = unsafe {
        CFNumber::new(
            None,
            CFNumberType::SInt32Type,
            &value as *const i32 as *const c_void,
        )
    }
    .ok_or("CFNumberCreate failed")?;
    let st = unsafe { VTSessionSetProperty(session.as_ref(), key, Some(num.as_ref() as &CFType)) };
    if st != 0 {
        return Err(format!("VTSessionSetProperty(int) failed: status={st}"));
    }
    Ok(())
}

/// Set a session property to a CFBoolean.
fn set_bool_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: bool,
) -> Result<(), String> {
    // SAFETY: reading the immortal kCFBoolean* CF singletons.
    let b = unsafe {
        if value {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
    }
    .ok_or("kCFBoolean missing")?;
    let st = unsafe {
        VTSessionSetProperty(
            session.as_ref(),
            key,
            // CFBoolean is a CFType.
            Some(&*(b as *const _ as *const CFType)),
        )
    };
    if st != 0 {
        return Err(format!("VTSessionSetProperty(bool) failed: status={st}"));
    }
    Ok(())
}

/// Set a session property to a CFString (e.g. the profile/level).
fn set_string_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: &CFString,
) -> Result<(), String> {
    let st = unsafe { VTSessionSetProperty(session.as_ref(), key, Some(value as &CFType)) };
    if st != 0 {
        return Err(format!("VTSessionSetProperty(string) failed: status={st}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Window picker (copied verbatim from s0_capture — hardware-validated)
// ---------------------------------------------------------------------------

fn find_mirroring_window(
    content: &SCShareableContent,
) -> Result<screencapturekit::shareable_content::SCWindow, String> {
    let windows = content.windows();

    let is_match = |app_name: &str, bundle: &str, title: &str| -> bool {
        let hay = format!(
            "{}\u{1}{}\u{1}{}",
            app_name.to_lowercase(),
            bundle.to_lowercase(),
            title.to_lowercase()
        );
        hay.contains("iphone mirroring")
            || hay.contains("screencontinuity")
            || hay.contains("iphone\u{955c}\u{50cf}")
            || hay.contains("\u{955c}\u{50cf}")
    };

    let size_ok =
        |w: f64, h: f64| -> bool { (200.0..=900.0).contains(&w) && (400.0..=1600.0).contains(&h) };

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
        let mut listing =
            String::from("could not find an iPhone Mirroring window. windows seen:\n");
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

    let area = |w: &screencapturekit::shareable_content::SCWindow| {
        let f = w.get_frame();
        f.size.width * f.size.height
    };
    let is_setup = |w: &screencapturekit::shareable_content::SCWindow| {
        let t = w.title().to_lowercase();
        t.contains("welcome") || t.contains("\u{6b22}\u{8fce}")
    };
    let mut size_matches: Vec<_> = candidates
        .iter()
        .filter(|w| {
            let f = w.get_frame();
            size_ok(f.size.width, f.size.height)
        })
        .collect();
    if size_matches.iter().any(|w| !is_setup(w)) {
        size_matches.retain(|w| !is_setup(w));
    }

    let pick = size_matches
        .iter()
        .max_by(|a, b| {
            (a.is_on_screen(), area(a))
                .partial_cmp(&(b.is_on_screen(), area(b)))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|w| (*w).clone())
        .or_else(|| candidates.iter().find(|w| w.is_on_screen()).cloned())
        .or_else(|| {
            candidates
                .iter()
                .max_by(|a, b| {
                    area(a)
                        .partial_cmp(&area(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .cloned()
        })
        .or_else(|| candidates.first().cloned())
        .expect("candidates is non-empty");

    Ok(pick)
}

// ---------------------------------------------------------------------------
// Main flow
// ---------------------------------------------------------------------------

fn run() -> Result<(), String> {
    println!("s0b_encode — ScreenCaptureKit capture + VideoToolbox H.264 encode probe");

    // 1. Enumerate + find the window.
    let content = SCShareableContent::get().map_err(|e| {
        format!("SCShareableContent::get failed: {e:?} — Screen Recording permission granted?")
    })?;
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
    let width = (frame.size.width as i32).max(2);
    let height = (frame.size.height as i32).max(2);
    println!(
        "  bounds: {}x{} @ ({},{}) onScreen={}",
        width,
        height,
        frame.origin.x as i64,
        frame.origin.y as i64,
        window.is_on_screen(),
    );

    // 2. Build the VTCompressionSession (H.264, realtime, no B-frames).
    let (etx, erx): (SyncSender<EncodedUnit>, Receiver<EncodedUnit>) = sync_channel(256);
    let ctx = Box::new(OutputContext {
        tx: etx,
        logged_params: std::cell::Cell::new(false),
    });
    let ctx_ptr = Box::into_raw(ctx);

    let session = {
        let mut session_out: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: standard VTCompressionSessionCreate; null encoder spec / source
        // attrs lets VT pick the HW H.264 encoder; output_callback + our refcon.
        let st = unsafe {
            VTCompressionSession::create(
                None,
                width,
                height,
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
            // Reclaim the leaked context before bailing.
            unsafe { drop(Box::from_raw(ctx_ptr)) };
            return Err(format!("VTCompressionSessionCreate failed: status={st}"));
        }
        // Adopt the +1 retain that Create returned.
        unsafe { CFRetained::from_raw(NonNull::new(session_out).unwrap()) }
    };

    // Properties. The kVT* keys are extern statics; reading them is `unsafe`.
    // SAFETY: these are immortal CFString constants exported by VideoToolbox.
    unsafe {
        set_bool_property(&session, kVTCompressionPropertyKey_RealTime, true)?;
        set_bool_property(
            &session,
            kVTCompressionPropertyKey_AllowFrameReordering,
            false,
        )?;
        set_string_property(
            &session,
            kVTCompressionPropertyKey_ProfileLevel,
            kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
        )?;
        set_int_property(&session, kVTCompressionPropertyKey_AverageBitRate, BITRATE)?;
        set_int_property(&session, kVTCompressionPropertyKey_MaxKeyFrameInterval, FPS)?;
    }
    let _ = unsafe { session.prepare_to_encode_frames() };

    println!(
        "VTCompressionSession ready: H.264 Constrained Baseline, realtime, \
         no B-frames, {} Mbps, keyframe every {} frames",
        BITRATE / 1_000_000,
        FPS
    );

    // 3. Build the single-window filter + 30fps BGRA config (same as s0_capture).
    let filter = SCContentFilter::new().with_desktop_independent_window(&window);
    let frame_interval = SckCMTime {
        value: 1,
        timescale: FPS,
        flags: 1, // kCMTimeFlags_Valid
        epoch: 0,
    };
    let config = SCStreamConfiguration::new()
        .set_width(width as u32)
        .and_then(|c| c.set_height(height as u32))
        .and_then(|c| c.set_pixel_format(PixelFormat::BGRA))
        .and_then(|c| c.set_shows_cursor(false))
        .and_then(|c| c.set_minimum_frame_interval(&frame_interval))
        .map_err(|e| format!("configuration failed: {e:?}"))?;

    let handler = CaptureHandler {
        session: SendSession(session.clone()),
        frame_index: std::cell::Cell::new(0),
    };

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(handler, SCStreamOutputType::Screen);

    println!("starting capture+encode — target {TARGET_FRAMES} frames -> ./{OUT_FILE} ...");
    stream
        .start_capture()
        .map_err(|e| format!("start_capture failed: {e:?}"))?;

    // 4. Drain encoded units, write the Annex-B file, print per-frame stats.
    let mut file =
        std::fs::File::create(OUT_FILE).map_err(|e| format!("could not create {OUT_FILE}: {e}"))?;
    let mut written = 0usize;
    let mut keyframes = 0usize;
    let mut total_bytes = 0usize;

    while written < TARGET_FRAMES {
        match erx.recv_timeout(Duration::from_secs(10)) {
            Ok(unit) => {
                if let Some((sps, pps)) = unit.param_sizes {
                    println!("parameter sets: SPS={sps} bytes, PPS={pps} bytes (in-band, Annex-B)");
                }
                file.write_all(&unit.annex_b)
                    .map_err(|e| format!("write failed: {e}"))?;
                total_bytes += unit.annex_b.len();
                if unit.is_keyframe {
                    keyframes += 1;
                }
                println!(
                    "frame {written:03}: {} bytes{}",
                    unit.annex_b.len(),
                    if unit.is_keyframe { "  [KEYFRAME]" } else { "" }
                );
                written += 1;
            }
            Err(_) => {
                eprintln!(
                    "timed out waiting for an encoded frame after {written} frames — \
                     capture may be producing nothing (permission? window gone?)."
                );
                break;
            }
        }
    }

    let _ = stream.stop_capture();
    // Flush any frames still in the encoder, then tear it down.
    // SAFETY: kCMTimeInvalid completes all pending frames.
    let invalid = objc2_core_media::CMTime {
        value: 0,
        timescale: 0,
        flags: objc2_core_media::CMTimeFlags(0),
        epoch: 0,
    };
    let _ = unsafe { session.complete_frames(invalid) };
    // Drain any late callbacks (non-blocking).
    while let Ok(unit) = erx.try_recv() {
        if written >= TARGET_FRAMES {
            break;
        }
        let _ = file.write_all(&unit.annex_b);
        total_bytes += unit.annex_b.len();
        if unit.is_keyframe {
            keyframes += 1;
        }
        written += 1;
    }
    let _ = file.flush();
    unsafe { session.invalidate() };
    // Now safe to reclaim the output context — no more callbacks will fire.
    unsafe { drop(Box::from_raw(ctx_ptr)) };

    println!(
        "wrote {written} frame(s), {keyframes} keyframe(s), {total_bytes} bytes -> ./{OUT_FILE}"
    );
    if written == 0 {
        println!(
            "WARNING: no encoded frames — capture produced nothing. Check the \
             Screen Recording grant and that a live iPhone Mirroring window exists."
        );
    } else {
        println!(
            "play it back: `ffplay {OUT_FILE}` or open in VLC. PASS = it shows the \
             live iPhone screen. CouldNotGetDataBuffer drops during capture are expected."
        );
    }
    Ok(())
}

fn main() {
    // Same CGS_REQUIRE_INIT fix as s0_capture — bootstrap the AppKit/CG app
    // context before any ScreenCaptureKit call.
    let ok = objc2_app_kit::NSApplicationLoad();
    eprintln!("NSApplicationLoad() -> {ok}");

    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
