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
    "\u{955c}\u{50cf}",  // 镜像
];

/// Keywords that mark setup/welcome windows (case-insensitive).
const SETUP_KEYWORDS: &[&str] = &["welcome", "\u{6b22}\u{8fce}"];  // 欢迎

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
    KEYWORDS.iter().any(|kw| {
        app_lc.contains(kw) || bundle_lc.contains(kw) || title_lc.contains(kw)
    })
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

/// Select the best iPhone Mirroring window from a slice.
///
/// Algorithm:
/// 1. Filter to candidates (app/bundle/title contains a keyword).
/// 2. Among candidates, find those with `size_ok`.
/// 3. If any `size_ok` candidates exist:
///    a. If there are also `size_ok` non-setup candidates, discard setup windows.
///    b. Sort survivors by: (area DESC, on_screen DESC), pick first.
/// 4. If no `size_ok` candidate: fall back to on_screen first, then largest, then first.
/// 5. Return `None` if no candidates at all.
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

    // Pick best: largest area first, break ties by on_screen (true > false), then by id (stable).
    pool.into_iter().max_by(|a, b| {
        let area_cmp = area(a).partial_cmp(&area(b)).unwrap_or(std::cmp::Ordering::Equal);
        if area_cmp != std::cmp::Ordering::Equal {
            return area_cmp;
        }
        // tie-break: on_screen preferred
        match (a.on_screen, b.on_screen) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Equal,
        }
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
        let windows = vec![
            phone_window(1, 312.0, 694.0),
            phone_window(2, 390.0, 844.0),
        ];
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
