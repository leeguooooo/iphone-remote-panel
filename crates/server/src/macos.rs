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
        // macOS 14+ "cooperative activation" silently DENIES focus-stealing by
        // background processes — `open -a` returns success but the window never
        // comes frontmost while the user is active in another app (observed on
        // macOS 26: open -a no-ops, agent input drops). AppleScript `activate`
        // works: the Apple Event asks the TARGET app to activate itself, which
        // the system permits. First use pops an Automation consent once
        // ("iPhoneUse" wants to control "iPhone Mirroring") — grant it once.
        for name in MIRRORING_NAMES {
            let status = std::process::Command::new("/usr/bin/osascript")
                .args(["-e", &format!(r#"tell application "{name}" to activate"#)])
                .status();
            if matches!(status, Ok(s) if s.success()) {
                return;
            }
        }
        // Fallback (helps when Automation consent was denied): open -a still
        // works when the user isn't actively focused elsewhere.
        for name in MIRRORING_NAMES {
            let status = std::process::Command::new("/usr/bin/open")
                .args(["-a", name])
                .status();
            if matches!(status, Ok(s) if s.success()) {
                return;
            }
        }
        tracing::debug!("could not bring iPhone Mirroring frontmost via osascript or `open -a`");
    }

    /// The known localized names of the iPhone Mirroring app (en + zh-CN).
    const MIRRORING_NAMES: [&str; 2] = ["iPhone Mirroring", "iPhone 镜像"];

    /// Ensure the Mirroring app is frontmost, **synchronously**.
    ///
    /// `open -a` activation is asynchronous — it returns before the window
    /// actually receives focus. Injecting a CGEvent immediately after loses
    /// that race: the click lands on whatever app is *still* frontmost (the
    /// user's editor / browser), so agent taps silently no-op on a busy Mac.
    /// Hit in practice the moment the daemon ran on a machine the user was
    /// actively working on.
    ///
    /// Activates, then polls [`mirroring_is_frontmost`] until it sticks or
    /// `deadline` passes. Returns whether Mirroring is frontmost at the end.
    pub fn ensure_mirroring_frontmost(deadline: std::time::Duration) -> bool {
        if mirroring_is_frontmost() {
            return true;
        }
        bring_mirroring_frontmost();
        let end = std::time::Instant::now() + deadline;
        while std::time::Instant::now() < end {
            if mirroring_is_frontmost() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        mirroring_is_frontmost()
    }

    /// Returns true if the iPhone Mirroring app is currently the frontmost app.
    ///
    /// Cheap in-process query (`NSWorkspace.frontmostApplication`) — no subprocess.
    /// The injector calls this before each event so it only pays the expensive
    /// `open -a` re-activation when focus was actually stolen (HID-tap events land
    /// only on the frontmost app; a hardware test confirmed taps no-op when another
    /// app steals focus, while scroll/tap both work while Mirroring is frontmost).
    pub fn mirroring_is_frontmost() -> bool {
        use objc2_app_kit::NSWorkspace;
        // Argument-free property reads; safe to query off the main thread.
        let front = NSWorkspace::sharedWorkspace().frontmostApplication();
        match front {
            Some(app) => match app.localizedName() {
                Some(name) => {
                    let name = name.to_string();
                    MIRRORING_NAMES.contains(&name.as_str())
                }
                None => false,
            },
            None => false,
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::{
    bring_mirroring_frontmost, ensure_mirroring_frontmost, mirroring_is_frontmost,
    ns_application_load, request_screen_capture, tcc_status, TccStatus,
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

#[cfg(not(target_os = "macos"))]
pub fn mirroring_is_frontmost() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_mirroring_frontmost(_deadline: std::time::Duration) -> bool {
    false
}
