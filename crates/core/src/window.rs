/// Pure window-selection heuristic for iPhone Mirroring.
/// No OS calls — receives a slice of `WindowInfo` fixtures.

/// Metadata about a single on-screen window, supplied by the caller.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u32,
    pub app: String,
    pub bundle: String,
    pub title: String,
    pub on_screen: bool,
    pub w: f64,
    pub h: f64,
    pub x: f64,
    pub y: f64,
}

/// Keywords used to identify iPhone Mirroring windows (case-insensitive).
const KEYWORDS: &[&str] = &[
    "iphone mirroring",
    "screencontinuity",
    "\u{955c}\u{50cf}", // 镜像
];

/// Keywords that mark setup/welcome windows (case-insensitive).
const SETUP_KEYWORDS: &[&str] = &["welcome", "\u{6b22}\u{8fce}"]; // 欢迎

/// Width range for a "real" phone content window.
const W_MIN: f64 = 200.0;
const W_MAX: f64 = 900.0;
/// Height range for a "real" phone content window.
const H_MIN: f64 = 400.0;
const H_MAX: f64 = 1600.0;

/// Return `true` if this window looks like an iPhone Mirroring window.
fn is_candidate(w: &WindowInfo) -> bool {
    let app_lc = w.app.to_lowercase();
    let bundle_lc = w.bundle.to_lowercase();
    let title_lc = w.title.to_lowercase();
    KEYWORDS
        .iter()
        .any(|kw| app_lc.contains(kw) || bundle_lc.contains(kw) || title_lc.contains(kw))
}

/// Return `true` if dimensions are in the expected phone-screen range.
fn size_ok(w: &WindowInfo) -> bool {
    w.w >= W_MIN && w.w <= W_MAX && w.h >= H_MIN && w.h <= H_MAX
}

/// Return `true` if this looks like a setup / welcome / onboarding window.
fn is_setup(w: &WindowInfo) -> bool {
    let title_lc = w.title.to_lowercase();
    SETUP_KEYWORDS.iter().any(|kw| title_lc.contains(kw))
}

fn area(w: &WindowInfo) -> f64 {
    w.w * w.h
}

/// Minimum height/width ratio for a real phone content window.
///
/// iPhone Mirroring is portrait-locked, so the live phone window is always
/// clearly taller than wide (a real 312×694 window is 2.22; even an iPhone SE
/// is ~1.78). A spurious near-square SCK window — e.g. the hidden 500×500
/// off-screen one a hardware test caught winning on raw area — is rejected here.
const PHONE_MIN_ASPECT: f64 = 1.5;

/// Return `true` if the window is portrait phone-shaped (tall, not square/wide).
fn is_phone_shaped(w: &WindowInfo) -> bool {
    w.w > 0.0 && w.h >= w.w * PHONE_MIN_ASPECT
}

/// Select the best iPhone Mirroring window from a slice.
///
/// Algorithm:
/// 1. Filter to candidates (app/bundle/title contains a keyword).
/// 2. Among candidates, find those with `size_ok`.
/// 3. If any `size_ok` candidates exist:
///    a. If there are also `size_ok` non-setup candidates, discard setup windows.
///    b. Sort survivors by the ranking below, pick first.
/// 4. If no `size_ok` candidate: fall back to the same ranking over all candidates.
/// 5. Return `None` if no candidates at all.
///
/// # Ranking (most significant first)
///
/// 1. **`on_screen` DESC** — primary discriminator.  A hardware test on macOS 15
///    caught a hidden, off-screen 500×500 SCK window that outscored the real
///    312×694 phone on raw area.  Checking `on_screen` first beats it without
///    making any shape assumptions.
///
/// 2. **`is_phone_shaped` DESC** (tiebreak) — on macOS 15/26 where both the real
///    phone window and a junk window happen to be on-screen, portrait shape
///    (`h ≥ w * 1.5`) still gives the right answer.  This is deliberately a
///    *tiebreak* rather than the primary key so that on macOS 27 "Golden Gate" —
///    where iPhone Mirroring windows are resizable and may be landscape or
///    square — a real, user-resized window is never rejected just because it is
///    no longer portrait.
///
/// 3. **area DESC** — largest window wins among ties.
///
/// 4. **lower id** — stable, deterministic tiebreaker.
pub fn select_mirroring(windows: &[WindowInfo]) -> Option<&WindowInfo> {
    let candidates: Vec<&WindowInfo> = windows.iter().filter(|w| is_candidate(w)).collect();
    if candidates.is_empty() {
        return None;
    }

    let size_ok_candidates: Vec<&WindowInfo> =
        candidates.iter().copied().filter(|w| size_ok(w)).collect();

    let pool: Vec<&WindowInfo> = if !size_ok_candidates.is_empty() {
        // Check whether there are non-setup windows among the size_ok set.
        let non_setup: Vec<&WindowInfo> = size_ok_candidates
            .iter()
            .copied()
            .filter(|w| !is_setup(w))
            .collect();
        if !non_setup.is_empty() {
            non_setup
        } else {
            size_ok_candidates
        }
    } else {
        // No size_ok candidates — fall back to all candidates.
        candidates
    };

    // Rank: on_screen first (beats the hidden junk window without shape assumptions);
    // then phone-shaped as a tiebreak (still helps on macOS 15/26 when both windows
    // happen to be on-screen); then largest area; then lower id for stability.
    // on_screen is PRIMARY so that on macOS 27 "Golden Gate" a real window that the
    // user has resized to landscape/square is never demoted below an off-screen junk
    // window just because it lost the portrait-shape test.
    use std::cmp::Ordering;
    pool.into_iter().max_by(|a, b| {
        match (a.on_screen, b.on_screen) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        // Tiebreak: portrait shape still helps on macOS 15/26 where both windows
        // may be on-screen; ignored on macOS 27 where real windows may be landscape.
        match (is_phone_shaped(a), is_phone_shaped(b)) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        let area_cmp = area(a).partial_cmp(&area(b)).unwrap_or(Ordering::Equal);
        if area_cmp != Ordering::Equal {
            return area_cmp;
        }
        // stable: prefer the lower window id (so it's `Greater` in max_by)
        b.id.cmp(&a.id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone_window(id: u32, w: f64, h: f64) -> WindowInfo {
        WindowInfo {
            id,
            app: "iPhone Mirroring".to_string(),
            bundle: "com.apple.ScreenContinuity".to_string(),
            title: "iPhone".to_string(),
            on_screen: true,
            w,
            h,
            x: 0.0,
            y: 0.0,
        }
    }

    fn menubar_window() -> WindowInfo {
        WindowInfo {
            id: 99,
            app: "iPhone Mirroring".to_string(),
            bundle: "com.apple.ScreenContinuity".to_string(),
            title: "iPhone Mirroring".to_string(),
            on_screen: true,
            w: 1800.0,
            h: 39.0,
            x: 0.0,
            y: 0.0,
        }
    }

    fn welcome_window() -> WindowInfo {
        WindowInfo {
            id: 50,
            app: "iPhone Mirroring".to_string(),
            bundle: "com.apple.ScreenContinuity".to_string(),
            title: "欢迎使用iPhone镜像".to_string(),
            on_screen: true,
            w: 640.0,
            h: 662.0,
            x: 0.0,
            y: 0.0,
        }
    }

    fn unrelated_window() -> WindowInfo {
        WindowInfo {
            id: 1,
            app: "Safari".to_string(),
            bundle: "com.apple.Safari".to_string(),
            title: "Apple".to_string(),
            on_screen: true,
            w: 1200.0,
            h: 800.0,
            x: 0.0,
            y: 0.0,
        }
    }

    // --- Basic selection ---

    #[test]
    fn picks_phone_window_over_menubar_and_welcome() {
        let windows = vec![
            phone_window(10, 312.0, 694.0),
            menubar_window(),
            welcome_window(),
        ];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(sel.id, 10);
    }

    #[test]
    fn returns_none_when_no_candidate_matches() {
        let windows = vec![unrelated_window()];
        assert!(select_mirroring(&windows).is_none());
    }

    #[test]
    fn returns_none_for_empty_slice() {
        assert!(select_mirroring(&[]).is_none());
    }

    #[test]
    fn picks_largest_when_two_phone_windows() {
        let windows = vec![phone_window(1, 312.0, 694.0), phone_window(2, 390.0, 844.0)];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(sel.id, 2); // 390*844 > 312*694
    }

    // --- Setup/welcome window skipping ---

    #[test]
    fn skips_welcome_window_when_real_phone_exists() {
        let windows = vec![welcome_window(), phone_window(5, 312.0, 694.0)];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(sel.id, 5);
    }

    #[test]
    fn returns_welcome_window_when_no_other_candidate() {
        let windows = vec![welcome_window()];
        // welcome is size_ok (640x662 is in 200..=900 x 400..=1600)
        // but it's the only candidate — must still return it
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(sel.id, 50);
    }

    // --- Bundle / title matching ---

    #[test]
    fn matches_by_bundle_screencontinuity() {
        let w = WindowInfo {
            id: 7,
            app: "Something".to_string(),
            bundle: "com.apple.ScreenContinuity".to_string(),
            title: "Whatever".to_string(),
            on_screen: true,
            w: 312.0,
            h: 694.0,
            x: 0.0,
            y: 0.0,
        };
        assert!(select_mirroring(&[w]).is_some());
    }

    #[test]
    fn matches_case_insensitive_iphone_mirroring_in_title() {
        let w = WindowInfo {
            id: 8,
            app: "App".to_string(),
            bundle: "com.other.bundle".to_string(),
            title: "iPhone Mirroring — Paused".to_string(),
            on_screen: false,
            w: 312.0,
            h: 694.0,
            x: 0.0,
            y: 0.0,
        };
        assert!(select_mirroring(&[w]).is_some());
    }

    // --- on_screen tie-breaking ---

    #[test]
    fn prefers_on_screen_window_among_equal_area() {
        let mut w1 = phone_window(1, 312.0, 694.0);
        w1.on_screen = false;
        let mut w2 = phone_window(2, 312.0, 694.0);
        w2.on_screen = true;
        let windows = [w1, w2];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(sel.id, 2);
    }

    // --- spurious near-square SCK window must not win on area ---

    #[test]
    fn rejects_square_window_outscoring_real_phone_on_area() {
        // Hardware-caught regression: a hidden 500×500 SCK window (area 250000)
        // outscored the real 312×694 phone (area 216528) under area-first ranking.
        // `on_screen` is the PRIMARY ranking key — the off-screen square must lose
        // to the on-screen phone regardless of its larger area (shape is only a
        // tiebreak now; see the macOS 27 rationale on select_mirroring).
        let mut square = phone_window(77, 500.0, 500.0);
        square.title = String::new(); // off-screen/empty-title junk window
        square.on_screen = false;
        let windows = vec![square, phone_window(10, 312.0, 694.0)];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(
            sel.id, 10,
            "must pick the on-screen phone, not the bigger off-screen square"
        );
    }

    /// When BOTH windows are on-screen, `is_phone_shaped` is the tiebreak: on
    /// macOS 15/26 (portrait-locked) an on-screen portrait phone beats an
    /// on-screen square junk window even when the square has more area.  This
    /// pins the tiebreak semantics — on macOS 27 a user-resized landscape real
    /// window with an on-screen portrait junk sibling would lose this tiebreak,
    /// a known limitation until hardware evidence says otherwise.
    #[test]
    fn onscreen_portrait_beats_onscreen_square_via_shape_tiebreak() {
        let mut square = phone_window(77, 500.0, 500.0); // on_screen=true by default
        square.title = String::new();
        let windows = vec![square, phone_window(10, 312.0, 694.0)];
        assert_eq!(select_mirroring(&windows).unwrap().id, 10);
    }

    #[test]
    fn among_two_portrait_phones_larger_area_still_wins() {
        // Shape tie → area still decides (no regression to picks_largest).
        let windows = vec![phone_window(1, 312.0, 694.0), phone_window(2, 390.0, 844.0)];
        assert_eq!(select_mirroring(&windows).unwrap().id, 2);
    }

    // --- macOS 27 "Golden Gate" compatibility ---

    /// macOS 27 makes the Mirroring window resizable; the user may have dragged it
    /// to a landscape 800×600 layout.  The real window (on_screen=true, landscape)
    /// must beat an off-screen portrait window because `on_screen` is the primary
    /// ranking key — portrait shape is only a tiebreak.
    #[test]
    fn macos27_landscape_on_screen_beats_offscreen_portrait() {
        // Landscape "real" window the user resized on macOS 27.
        let mut landscape = phone_window(20, 800.0, 600.0);
        landscape.on_screen = true;

        // Old portrait window that is now off-screen (minimised, wrong display, etc.).
        let mut portrait_offscreen = phone_window(10, 312.0, 694.0);
        portrait_offscreen.on_screen = false;

        let windows = vec![portrait_offscreen, landscape];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(
            sel.id, 20,
            "on-screen landscape window must win over off-screen portrait on macOS 27"
        );
    }

    /// Single on-screen landscape window (macOS 27 resized) — no portrait competitor.
    /// Must be selected without any rejection based on aspect ratio.
    #[test]
    fn macos27_landscape_only_candidate_is_selected() {
        let mut landscape = phone_window(30, 800.0, 600.0);
        landscape.on_screen = true;

        let windows = vec![landscape];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(
            sel.id, 30,
            "a lone on-screen landscape window must not be rejected on macOS 27"
        );
    }

    // --- menubar (out-of-size) falls back ---

    #[test]
    fn menubar_only_still_returns_it_as_fallback() {
        // menubar is a match (bundle/app) but h=39 is below 400 → not size_ok
        // → falls back to "largest" among all candidates
        let windows = vec![menubar_window()];
        let sel = select_mirroring(&windows).unwrap();
        assert_eq!(sel.id, 99);
    }
}
