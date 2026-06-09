//! macOS-only glue: TCC preflight, AppKit bootstrap, and bringing the iPhone
//! Mirroring window frontmost before injecting input.
//!
//! Everything here is gated behind `cfg(target_os = "macos")`; the non-macOS
//! stubs let the rest of the daemon compile and unit-test on any platform.

// ---------------------------------------------------------------------------
// macOS implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        /// Returns true if the calling process already has Screen Recording
        /// permission (does NOT prompt).
        fn CGPreflightScreenCaptureAccess() -> bool;
        /// Requests Screen Recording permission, prompting the user if needed.
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        /// Returns true if the process is a trusted accessibility client.
        fn AXIsProcessTrusted() -> bool;
    }

    /// Result of the TCC preflight: which permissions are missing.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct TccStatus {
        pub screen_recording: bool,
        pub accessibility: bool,
    }

    impl TccStatus {
        pub fn ok(&self) -> bool {
            self.screen_recording && self.accessibility
        }
    }

    /// Check Screen Recording (capture) + Accessibility (HID input) grants.
    pub fn tcc_status() -> TccStatus {
        // SAFETY: both are argument-free C predicates with no preconditions.
        let screen_recording = unsafe { CGPreflightScreenCaptureAccess() };
        let accessibility = unsafe { AXIsProcessTrusted() };
        TccStatus {
            screen_recording,
            accessibility,
        }
    }

    /// Trigger the Screen Recording permission prompt (no-op if already granted).
    pub fn request_screen_capture() {
        // SAFETY: argument-free C call.
        let _ = unsafe { CGRequestScreenCaptureAccess() };
    }

    /// Bootstrap AppKit/CoreGraphics so ScreenCaptureKit's `startCapture` works
    /// (fixes `CGS_REQUIRE_INIT`). Must run on the main thread before any SCK call.
    pub fn ns_application_load() {
        // The spike `s0b2_webrtc` validated this exact call; it is the
        // CGS_REQUIRE_INIT bootstrap SCK needs. (Deprecated alias of
        // NSApplication::load, but the free function is what the probe used.)
        #[allow(deprecated)]
        let ok = objc2_app_kit::NSApplicationLoad();
        tracing::debug!("NSApplicationLoad() -> {ok}");
    }

    /// Bring the iPhone Mirroring app frontmost so HID-tap events land on it.
    ///
    /// Best-effort: shells out to `open -a "iPhone Mirroring"`; if that app name
    /// is localized differently this is a no-op and input may not register until
    /// the user focuses the window manually. Logged at debug level on failure.
    pub fn bring_mirroring_frontmost() {
        // `open -a` activates the app by name; on a zh-CN system the app is named
        // "iPhone 镜像" — try both.
        for name in ["iPhone Mirroring", "iPhone 镜像"] {
            let status = std::process::Command::new("/usr/bin/open")
                .args(["-a", name])
                .status();
            if matches!(status, Ok(s) if s.success()) {
                return;
            }
        }
        tracing::debug!("could not bring iPhone Mirroring frontmost via `open -a`");
    }
}

#[cfg(target_os = "macos")]
pub use imp::{
    bring_mirroring_frontmost, ns_application_load, request_screen_capture, tcc_status, TccStatus,
};

// ---------------------------------------------------------------------------
// Non-macOS stubs
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Default, Clone, Copy)]
pub struct TccStatus {
    pub screen_recording: bool,
    pub accessibility: bool,
}

#[cfg(not(target_os = "macos"))]
impl TccStatus {
    pub fn ok(&self) -> bool {
        false
    }
}

/// Non-macOS: capture/input unsupported, so report both missing.
#[cfg(not(target_os = "macos"))]
pub fn tcc_status() -> TccStatus {
    TccStatus::default()
}

#[cfg(not(target_os = "macos"))]
pub fn request_screen_capture() {}

#[cfg(not(target_os = "macos"))]
pub fn ns_application_load() {}

#[cfg(not(target_os = "macos"))]
pub fn bring_mirroring_frontmost() {}
