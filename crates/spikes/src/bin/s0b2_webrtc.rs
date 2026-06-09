//! Spike S0b-2 — ScreenCaptureKit capture + VideoToolbox H.264 + **WebRTC** to a browser
//!
//! Purpose: prove that the iPhone-Mirroring capture, hardware-encoded to realtime
//! H.264 by VideoToolbox (validated by S0b-1 / s0b_encode), can be streamed over
//! **WebRTC** (webrtc-rs) to an ordinary desktop browser on the LAN, which decodes
//! and displays it live. This is the last hop before the iOS-Safari step (S2) and
//! the Cloudflare-TURN step (S3).
//!
//! Throwaway validation probe. It does NOT decide PASS/FAIL — a human opens the
//! served page in a LAN browser and judges. See the RUNBOOK in the task report.
//!
//! Pipeline (front-half is the hardware-validated s0b_encode pipeline, copied here
//! verbatim so the existing bins keep building):
//!   1. NSApplicationLoad(); find the iPhone Mirroring window; SCStream -> BGRA 30fps.
//!   2. VTCompressionSession: H.264 Constrained Baseline, realtime, no B-frames,
//!      ~6 Mbps, keyframe interval 1s. AVCC output -> **Annex-B** with in-band
//!      SPS+PPS prepended before every IDR (exactly what TrackLocalStaticSample's
//!      H264 RTP payloader wants).
//!   3. Instead of writing a file, each encoded access unit is stored as the
//!      "latest frame" in shared state.
//!
//! Back-half (new):
//!   4. A tiny HTTP server (tiny_http) on 0.0.0.0:8088 serves index.html and a
//!      JSON signaling endpoint. The **browser is the offerer** (recvonly): it
//!      POSTs an SDP offer to /offer, we set it as the remote description, create
//!      + set our answer, gather ICE to completion (non-trickle — the answer SDP
//!      carries all candidates), and return the answer JSON. LAN-only, STUN only.
//!   5. One TrackLocalStaticSample (H264) is added to a single RTCPeerConnection.
//!      A tokio feeder task ticks at the frame interval and `write_sample`s the
//!      latest encoded access unit.
//!
//! STATIC-FRAME KEEPALIVE (validated need): SCK delivers NO new frames on a static
//! screen. The feeder always re-writes the latest stored access unit every tick,
//! so a static screen still produces a continuous RTP stream and a late joiner
//! gets video. On a new PeerConnection (and on RTCP PLI) we force a VT IDR so the
//! browser can start decoding immediately.
//!
//! Usage:  s0b2_webrtc      (then open http://<lan-ip>:8088 in a desktop browser)
//!
//! Requires: Screen Recording permission (TCC) granted to the terminal / binary,
//! a live iPhone Mirroring window. Runs in the granted GUI session (it captures).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_media_rs::cm_time::CMTime as SckCMTime;

use core_foundation::base::TCFType;

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

use core_media_rs::cm_sample_buffer::CMSampleBuffer as SckSampleBuffer;
use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::pixel_format::PixelFormat;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;

// webrtc-rs (pinned 0.17). Verified against the vendored crate source — see report.
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

const FPS: i32 = 30;
const BITRATE: i32 = 6_000_000; // 6 Mbps
const HTTP_ADDR: &str = "0.0.0.0:8088";
/// Feeder tick — also the keepalive cadence. ~33 ms = 30 fps. On a static screen
/// the feeder re-sends the last stored access unit every tick (>500 ms-safe).
const FEED_INTERVAL: Duration = Duration::from_millis(1000 / FPS as u64);
/// Browser's first profile to try. Safari may need 42e01f (see report/cheat-sheet).
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f";

// ---------------------------------------------------------------------------
// Shared state between the (sync) capture thread and the (async) feeder.
// ---------------------------------------------------------------------------

/// The most-recently-encoded Annex-B access unit. The feeder reads `data` every
/// tick and writes it as a webrtc Sample. `generation` lets a future optimisation
/// distinguish a genuinely-new frame from a keepalive repeat; we send on every
/// tick regardless (repeat-last-frame keepalive).
#[derive(Default)]
struct LatestFrame {
    data: Vec<u8>,
    generation: u64,
}

/// Shared between the capture/encode thread and the WebRTC feeder.
struct EncodeShared {
    latest: Mutex<LatestFrame>,
    /// When set, the *next* encoded frame is forced to be an IDR (SPS/PPS + a
    /// decodable keyframe). Set on a new PeerConnection and on RTCP PLI.
    force_idr: AtomicBool,
    frames_encoded: AtomicU64,
    logged_params: AtomicBool,
}

impl EncodeShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            latest: Mutex::new(LatestFrame::default()),
            force_idr: AtomicBool::new(true), // first frame is always a keyframe
            frames_encoded: AtomicU64::new(0),
            logged_params: AtomicBool::new(false),
        })
    }
}

// ---------------------------------------------------------------------------
// VideoToolbox output callback — copied from s0b_encode, but pushes the Annex-B
// access unit into shared state instead of a channel-to-file.
// ---------------------------------------------------------------------------

struct OutputContext {
    shared: Arc<EncodeShared>,
}

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
    // outlives every callback (the Box is leaked for the process lifetime). We
    // only take a shared &.
    let ctx = unsafe { &*(output_ref_con as *const OutputContext) };

    if status != 0 || sample_buffer.is_null() {
        return;
    }
    // SAFETY: VT guarantees a valid CMSampleBuffer for the duration of the call.
    let sample = unsafe { &*sample_buffer };

    match build_annex_b(sample, &ctx.shared) {
        Ok((annex_b, is_keyframe, params)) => {
            if let Some((sps, pps)) = params {
                println!(
                    "parameter sets: SPS={sps} bytes, PPS={pps} bytes (in-band, Annex-B per IDR)"
                );
            }
            let n = ctx.shared.frames_encoded.fetch_add(1, Ordering::Relaxed);
            if n < 5 || is_keyframe {
                println!(
                    "encoded frame {n:>5}: {} bytes{}",
                    annex_b.len(),
                    if is_keyframe { "  [KEYFRAME]" } else { "" }
                );
            }
            let mut latest = ctx.shared.latest.lock().unwrap();
            latest.generation += 1;
            latest.data = annex_b;
        }
        Err(e) => eprintln!("encode-callback: {e}"),
    }
}

/// A frame is a sync sample (keyframe) unless `kCMSampleAttachmentKey_NotSync`
/// is present (and true).
fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    // SAFETY: create_if_necessary=false; returns the immutable attachments array.
    let arr = match unsafe { sample.sample_attachments_array(false) } {
        Some(a) => a,
        None => return true,
    };
    if arr.count() == 0 {
        return true;
    }
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
    !not_sync_present
}

/// Convert one compressed CMSampleBuffer (AVCC, length-prefixed NALUs) into an
/// Annex-B byte vector. For keyframes, SPS+PPS (from the format description) are
/// prepended as their own start-code-delimited NALs. Returns
/// (annex_b, is_keyframe, Some((sps_size,pps_size)) on first keyframe).
fn build_annex_b(
    sample: &CMSampleBuffer,
    shared: &EncodeShared,
) -> Result<(Vec<u8>, bool, Option<(usize, usize)>), String> {
    const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
    let keyframe = is_keyframe(sample);
    let mut out: Vec<u8> = Vec::new();
    let mut param_sizes: Option<(usize, usize)> = None;

    if keyframe {
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

        if !shared.logged_params.swap(true, Ordering::Relaxed) {
            param_sizes = Some((sps_size, pps_size));
        }

        if nal_len != 4 {
            return Err(format!("unexpected NAL length size {nal_len} (expected 4)"));
        }
    }

    // Walk the AVCC block buffer: [4-byte BE length][NALU bytes] repeated.
    // SAFETY: a compressed sample buffer always carries a CMBlockBuffer.
    let block: CFRetained<CMBlockBuffer> = unsafe { sample.data_buffer() }
        .ok_or("compressed sample has no data buffer")?;

    let total = unsafe { block.data_length() };
    let mut data_ptr: *mut std::os::raw::c_char = std::ptr::null_mut();
    let mut length_at_offset: usize = 0;
    let mut total_len: usize = 0;
    // SAFETY: standard CMBlockBufferGetDataPointer call; out-params are valid.
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
    // SAFETY: `total` bytes are readable at data_ptr.
    let data = unsafe { std::slice::from_raw_parts(data_ptr as *const u8, total) };

    let mut i = 0usize;
    while i + 4 <= data.len() {
        let nal_len =
            u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
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

    Ok((out, keyframe, param_sizes))
}

// ---------------------------------------------------------------------------
// Capture handler — encodes each captured frame on the SCK delegate thread.
// ---------------------------------------------------------------------------

/// VTCompressionSession is documented thread-safe; we only call encode_frame from
/// the single SCK delegate thread. Wrap so the handler satisfies Send.
struct SendSession(CFRetained<VTCompressionSession>);
// SAFETY: access is serialized on the SCK delegate thread; the API is thread-safe.
unsafe impl Send for SendSession {}
unsafe impl Sync for SendSession {}

struct CaptureHandler {
    session: SendSession,
    shared: Arc<EncodeShared>,
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
        let pixel_buffer = match sample_buffer.get_pixel_buffer() {
            Ok(pb) => pb,
            Err(_) => return, // idle SCK frame; normal
        };

        let raw = pixel_buffer.as_concrete_TypeRef();
        if raw.is_null() {
            return;
        }
        // SAFETY: live CVPixelBuffer (== CVImageBuffer) retained by `pixel_buffer`.
        let image_buffer = unsafe { &*(raw as *const CVImageBuffer) };

        let idx = self.frame_index.get();
        self.frame_index.set(idx + 1);

        let pts = objc2_core_media::CMTime {
            value: idx,
            timescale: FPS,
            flags: objc2_core_media::CMTimeFlags(1),
            epoch: 0,
        };
        let duration = objc2_core_media::CMTime {
            value: 1,
            timescale: FPS,
            flags: objc2_core_media::CMTimeFlags(1),
            epoch: 0,
        };

        // Force an IDR on the first frame and whenever a viewer joins / PLI fires.
        let want_idr = idx == 0 || self.shared.force_idr.swap(false, Ordering::Relaxed);
        let frame_props: Option<CFRetained<CFDictionary>> = if want_idr {
            Some(make_force_keyframe_dict())
        } else {
            None
        };

        let mut info = VTEncodeInfoFlags(0);
        // SAFETY: image_buffer is live; frame_props (if any) holds correct types.
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
// CF helpers (copied from s0b_encode)
// ---------------------------------------------------------------------------

fn make_force_keyframe_dict() -> CFRetained<CFDictionary> {
    // SAFETY: static key/value are valid CF objects; build a 1-entry dict.
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
) -> Result<(), String> {
    // SAFETY: build a CFNumber(i32) and hand it to VTSessionSetProperty.
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

fn set_bool_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: bool,
) -> Result<(), String> {
    // SAFETY: reading the immortal kCFBoolean* CF singletons.
    let b = unsafe { if value { kCFBooleanTrue } else { kCFBooleanFalse } }
        .ok_or("kCFBoolean missing")?;
    let st = unsafe {
        VTSessionSetProperty(
            session.as_ref(),
            key,
            Some(&*(b as *const _ as *const CFType)),
        )
    };
    if st != 0 {
        return Err(format!("VTSessionSetProperty(bool) failed: status={st}"));
    }
    Ok(())
}

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
// Window picker (copied verbatim from s0_capture / s0b_encode — HW-validated)
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
        let mut listing = String::from("could not find an iPhone Mirroring window. windows seen:\n");
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
                .max_by(|a, b| area(a).partial_cmp(&area(b)).unwrap_or(std::cmp::Ordering::Equal))
                .cloned()
        })
        .or_else(|| candidates.first().cloned())
        .expect("candidates is non-empty");

    Ok(pick)
}

// ---------------------------------------------------------------------------
// Capture + encode bring-up (returns once the SCStream is running). The SCStream
// is leaked (kept alive for the process lifetime) — this is a daemon-style probe.
// ---------------------------------------------------------------------------

fn start_capture_encode(shared: Arc<EncodeShared>) -> Result<(), String> {
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
        "  bounds: {width}x{height} @ ({},{}) onScreen={}",
        frame.origin.x as i64,
        frame.origin.y as i64,
        window.is_on_screen(),
    );

    // VTCompressionSession (H.264, realtime, no B-frames). Output context is
    // leaked deliberately (daemon lifetime — callbacks fire until process exit).
    let ctx = Box::new(OutputContext {
        shared: shared.clone(),
    });
    let ctx_ptr = Box::into_raw(ctx);

    let session = {
        let mut session_out: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: standard VTCompressionSessionCreate; null specs let VT pick the
        // HW H.264 encoder; output_callback + our (leaked) refcon.
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
            unsafe { drop(Box::from_raw(ctx_ptr)) };
            return Err(format!("VTCompressionSessionCreate failed: status={st}"));
        }
        // SAFETY: adopt the +1 retain Create returned.
        unsafe { CFRetained::from_raw(NonNull::new(session_out).unwrap()) }
    };

    // SAFETY: these are immortal CFString constants exported by VideoToolbox.
    unsafe {
        set_bool_property(&session, kVTCompressionPropertyKey_RealTime, true)?;
        set_bool_property(&session, kVTCompressionPropertyKey_AllowFrameReordering, false)?;
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
        "VTCompressionSession ready: H.264 Constrained Baseline, realtime, no B-frames, \
         {} Mbps, keyframe every {FPS} frames",
        BITRATE / 1_000_000
    );

    let filter = SCContentFilter::new().with_desktop_independent_window(&window);
    let frame_interval = SckCMTime {
        value: 1,
        timescale: FPS,
        flags: 1,
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
        shared,
        frame_index: std::cell::Cell::new(0),
    };

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(handler, SCStreamOutputType::Screen);

    println!("starting capture+encode ...");
    stream
        .start_capture()
        .map_err(|e| format!("start_capture failed: {e:?}"))?;

    // Daemon-style: keep the stream + session alive for the process lifetime.
    std::mem::forget(stream);
    std::mem::forget(session);
    Ok(())
}

// ---------------------------------------------------------------------------
// WebRTC: one PeerConnection per browser offer.
// ---------------------------------------------------------------------------

/// Build the API (default codecs + interceptors), register the H264 codec, create
/// a PeerConnection, add the H264 track, spawn the feeder + PLI-reader tasks, set
/// the remote offer, create+set the answer, gather ICE to completion, and return
/// the answer SDP JSON string. Non-trickle: the returned answer carries all
/// candidates (LAN-only, so host candidates suffice).
async fn handle_offer(offer_sdp: String, shared: Arc<EncodeShared>) -> Result<String, String> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .map_err(|e| format!("register_default_codecs: {e}"))?;

    let registry = Registry::new();
    let registry = register_default_interceptors(registry, &mut media_engine)
        .map_err(|e| format!("register_default_interceptors: {e}"))?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let pc = Arc::new(
        api.new_peer_connection(config)
            .await
            .map_err(|e| format!("new_peer_connection: {e}"))?,
    );

    // H264 track. fmtp mirrors register_default_codecs / pion; Safari may need
    // 42e01f instead (see report) — swap H264_FMTP if the m-line drops.
    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            sdp_fmtp_line: H264_FMTP.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "iphone-mirror".to_owned(),
    ));

    let rtp_sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .map_err(|e| format!("add_track: {e}"))?;

    // PLI reader: read RTCP from the sender; force a VT IDR on PictureLossIndication
    // (and drain other RTCP so the interceptor pipeline keeps flowing).
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Ok((packets, _)) = rtp_sender.read_rtcp().await {
                for p in &packets {
                    if p.as_any().downcast_ref::<PictureLossIndication>().is_some() {
                        shared.force_idr.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // Feeder: tick at the frame interval, write the latest stored access unit.
    // This is also the static-frame keepalive (repeat-last-frame every tick).
    {
        let shared = shared.clone();
        let track = Arc::clone(&track);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(FEED_INTERVAL);
            loop {
                interval.tick().await;
                let data = {
                    let latest = shared.latest.lock().unwrap();
                    if latest.data.is_empty() {
                        continue; // nothing encoded yet
                    }
                    bytes::Bytes::copy_from_slice(&latest.data)
                };
                let sample = Sample {
                    data,
                    duration: FEED_INTERVAL,
                    ..Default::default()
                };
                if let Err(e) = track.write_sample(&sample).await {
                    // Track closed (peer gone) — stop feeding this connection.
                    eprintln!("feeder: write_sample ended: {e}");
                    break;
                }
            }
        });
    }

    // Force a fresh IDR so a newly-connected viewer can start decoding at once.
    {
        let shared = shared.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            println!("peer connection state: {state}");
            if state == RTCPeerConnectionState::Connected {
                shared.force_idr.store(true, Ordering::Relaxed);
            }
            Box::pin(async {})
        }));
    }

    // Browser is the offerer (recvonly). Set remote, answer, gather, return.
    let offer = RTCSessionDescription::offer(offer_sdp).map_err(|e| format!("parse offer: {e}"))?;
    pc.set_remote_description(offer)
        .await
        .map_err(|e| format!("set_remote_description: {e}"))?;

    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create_answer: {e}"))?;

    // Non-trickle: wait for ICE gathering to finish so the answer SDP carries all
    // host candidates (sufficient on a LAN).
    let mut gather_complete = pc.gathering_complete_promise().await;
    pc.set_local_description(answer)
        .await
        .map_err(|e| format!("set_local_description: {e}"))?;
    let _ = gather_complete.recv().await;

    let local = pc
        .local_description()
        .await
        .ok_or("no local description after gathering")?;

    // Leak the PeerConnection so it (and its spawned tasks / track) outlive this
    // request handler for the duration of the streaming session.
    std::mem::forget(pc);

    serde_json::to_string(&local).map_err(|e| format!("serialize answer: {e}"))
}

// ---------------------------------------------------------------------------
// HTTP signaling server (tiny_http, blocking — runs on the main thread).
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = include_str!("s0b2_index.html");

fn run_http_server(shared: Arc<EncodeShared>, rt: tokio::runtime::Handle) -> Result<(), String> {
    let server = tiny_http::Server::http(HTTP_ADDR)
        .map_err(|e| format!("tiny_http bind {HTTP_ADDR}: {e}"))?;
    println!("signaling server listening on http://{HTTP_ADDR}  (open it from a LAN browser)");

    for mut request in server.incoming_requests() {
        let url = request.url().to_owned();
        let method = request.method().to_string();

        match (method.as_str(), url.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                let resp = tiny_http::Response::from_string(INDEX_HTML).with_header(
                    tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap(),
                );
                let _ = request.respond(resp);
            }
            ("POST", "/offer") => {
                let mut body = String::new();
                if std::io::Read::read_to_string(request.as_reader(), &mut body).is_err() {
                    let _ = request.respond(
                        tiny_http::Response::from_string("bad body").with_status_code(400),
                    );
                    continue;
                }
                // body is the offer SDP JSON ({type, sdp}); pull the .sdp field,
                // or accept raw SDP.
                let offer_sdp = match serde_json::from_str::<RTCSessionDescription>(&body) {
                    Ok(desc) => desc.sdp,
                    Err(_) => body.clone(),
                };
                // Drive the async signaling on the tokio runtime; block this thread.
                let result = rt.block_on(handle_offer(offer_sdp, shared.clone()));
                match result {
                    Ok(answer_json) => {
                        let resp = tiny_http::Response::from_string(answer_json).with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"application/json"[..],
                            )
                            .unwrap(),
                        );
                        let _ = request.respond(resp);
                    }
                    Err(e) => {
                        eprintln!("offer handling failed: {e}");
                        let _ = request.respond(
                            tiny_http::Response::from_string(format!("offer failed: {e}"))
                                .with_status_code(500),
                        );
                    }
                }
            }
            _ => {
                let _ = request
                    .respond(tiny_http::Response::from_string("not found").with_status_code(404));
            }
        }
    }
    Ok(())
}

fn main() {
    // Same CGS_REQUIRE_INIT fix as s0_capture — bootstrap AppKit/CG before any SCK call.
    let ok = objc2_app_kit::NSApplicationLoad();
    eprintln!("NSApplicationLoad() -> {ok}");

    let shared = EncodeShared::new();

    // Tokio multi-thread runtime drives the webrtc peer connection + feeder tasks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let rt_handle = rt.handle().clone();

    // Start capture+encode (synchronous bring-up; the SCStream runs on its own
    // SCK-managed thread thereafter).
    if let Err(e) = start_capture_encode(shared.clone()) {
        eprintln!("error: capture/encode bring-up failed: {e}");
        std::process::exit(1);
    }

    // Block the main thread on the HTTP signaling server.
    if let Err(e) = run_http_server(shared, rt_handle) {
        eprintln!("error: http server failed: {e}");
        std::process::exit(1);
    }

    // Keep the runtime owned by main so spawned tasks survive.
    drop(rt);
}
