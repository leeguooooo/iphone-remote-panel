/// Coordinate mapping from normalized video-content space to absolute screen points.
///
/// Normalized coordinates are in [0,1] relative to the displayed video content rect.
/// Points outside [0,1] (e.g. letterbox/pillarbox bars) return `None`.

/// Axis-aligned rectangle in screen points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Device orientation as seen from the Mac display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Orientation {
    /// Normal upright portrait.
    Portrait,
    /// Device rotated 90° counter-clockwise (home button / action button on right).
    LandscapeLeft,
    /// Device rotated 90° clockwise (home button / action button on left).
    LandscapeRight,
}

/// Geometry of the current mirroring session.
#[derive(Debug, Clone)]
pub struct SessionGeometry {
    /// The content rect in screen (point) coordinates — origin top-left.
    pub content_rect: Rect,
    /// Display scale factor (points per pixel), informational.
    pub scale: f64,
    /// Current device orientation.
    pub orientation: Orientation,
}

/// Map a normalized point `(nx, ny)` in [0,1]×[0,1] to an absolute screen point.
///
/// Returns `None` if either component is outside [0.0, 1.0].
///
/// Orientation mapping (after clamping):
/// - `Portrait`:       screen = (rect.x + nx·w, rect.y + ny·h)
/// - `LandscapeLeft`:  device top→screen right; rotate: nx′=ny, ny′=1−nx
///                     then portrait map into content_rect.
/// - `LandscapeRight`: device top→screen left; rotate: nx′=1−ny, ny′=nx
///                     then portrait map into content_rect.
pub fn to_screen(norm: (f64, f64), geo: &SessionGeometry) -> Option<(f64, f64)> {
    let (nx, ny) = norm;
    if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) {
        return None;
    }

    let (rx, ry) = match geo.orientation {
        Orientation::Portrait => (nx, ny),
        Orientation::LandscapeLeft => (ny, 1.0 - nx),
        Orientation::LandscapeRight => (1.0 - ny, nx),
    };

    let r = &geo.content_rect;
    Some((r.x + rx * r.w, r.y + ry * r.h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portrait_geo() -> SessionGeometry {
        SessionGeometry {
            content_rect: Rect { x: 100.0, y: 200.0, w: 300.0, h: 600.0 },
            scale: 2.0,
            orientation: Orientation::Portrait,
        }
    }

    #[test]
    fn portrait_center_maps_to_content_rect_center() {
        let geo = portrait_geo();
        let result = to_screen((0.5, 0.5), &geo).unwrap();
        assert!((result.0 - 250.0).abs() < 1e-9, "x={}", result.0);
        assert!((result.1 - 500.0).abs() < 1e-9, "y={}", result.1);
    }

    #[test]
    fn portrait_origin_maps_to_content_rect_origin() {
        let geo = portrait_geo();
        let result = to_screen((0.0, 0.0), &geo).unwrap();
        assert!((result.0 - 100.0).abs() < 1e-9);
        assert!((result.1 - 200.0).abs() < 1e-9);
    }

    #[test]
    fn portrait_far_corner_maps_correctly() {
        let geo = portrait_geo();
        let result = to_screen((1.0, 1.0), &geo).unwrap();
        assert!((result.0 - 400.0).abs() < 1e-9);
        assert!((result.1 - 800.0).abs() < 1e-9);
    }

    #[test]
    fn out_of_range_returns_none() {
        let geo = portrait_geo();
        assert!(to_screen((-0.01, 0.5), &geo).is_none());
        assert!(to_screen((0.5, -0.01), &geo).is_none());
        assert!(to_screen((1.01, 0.5), &geo).is_none());
        assert!(to_screen((0.5, 1.01), &geo).is_none());
        assert!(to_screen((1.5, 2.0), &geo).is_none());
    }

    #[test]
    fn boundary_values_are_valid() {
        let geo = portrait_geo();
        assert!(to_screen((0.0, 0.0), &geo).is_some());
        assert!(to_screen((1.0, 1.0), &geo).is_some());
        assert!(to_screen((0.0, 1.0), &geo).is_some());
        assert!(to_screen((1.0, 0.0), &geo).is_some());
    }

    /// LandscapeLeft: device rotated 90° CCW.
    /// Mapping convention: norm (nx, ny) → rotated → (ny, 1-nx) in portrait coords,
    /// then into content_rect.
    #[test]
    fn landscape_left_center_maps_to_content_rect_center() {
        let geo = SessionGeometry {
            content_rect: Rect { x: 0.0, y: 0.0, w: 600.0, h: 300.0 },
            scale: 2.0,
            orientation: Orientation::LandscapeLeft,
        };
        let result = to_screen((0.5, 0.5), &geo).unwrap();
        assert!((result.0 - 300.0).abs() < 1e-9, "x={}", result.0);
        assert!((result.1 - 150.0).abs() < 1e-9, "y={}", result.1);
    }

    #[test]
    fn landscape_right_center_maps_to_content_rect_center() {
        let geo = SessionGeometry {
            content_rect: Rect { x: 0.0, y: 0.0, w: 600.0, h: 300.0 },
            scale: 2.0,
            orientation: Orientation::LandscapeRight,
        };
        let result = to_screen((0.5, 0.5), &geo).unwrap();
        assert!((result.0 - 300.0).abs() < 1e-9, "x={}", result.0);
        assert!((result.1 - 150.0).abs() < 1e-9, "y={}", result.1);
    }

    /// Resize test: two different content rects, same normalized point → proportional screen pts.
    #[test]
    fn resize_keeps_normalized_point_proportional() {
        let geo1 = SessionGeometry {
            content_rect: Rect { x: 0.0, y: 0.0, w: 200.0, h: 400.0 },
            scale: 1.0,
            orientation: Orientation::Portrait,
        };
        let geo2 = SessionGeometry {
            content_rect: Rect { x: 0.0, y: 0.0, w: 400.0, h: 800.0 },
            scale: 1.0,
            orientation: Orientation::Portrait,
        };
        let norm = (0.25, 0.75);
        let p1 = to_screen(norm, &geo1).unwrap();
        let p2 = to_screen(norm, &geo2).unwrap();
        // p2 should be exactly double p1
        assert!((p2.0 - p1.0 * 2.0).abs() < 1e-9);
        assert!((p2.1 - p1.1 * 2.0).abs() < 1e-9);
    }

    /// Landscape rotation correctness: norm (0.1,0.9) in LandscapeLeft should differ from portrait.
    #[test]
    fn landscape_left_corner_differs_from_portrait() {
        let geo_p = SessionGeometry {
            content_rect: Rect { x: 0.0, y: 0.0, w: 300.0, h: 600.0 },
            scale: 1.0,
            orientation: Orientation::Portrait,
        };
        let geo_l = SessionGeometry {
            content_rect: Rect { x: 0.0, y: 0.0, w: 600.0, h: 300.0 },
            scale: 1.0,
            orientation: Orientation::LandscapeLeft,
        };
        let p = to_screen((0.1, 0.9), &geo_p).unwrap();
        let l = to_screen((0.1, 0.9), &geo_l).unwrap();
        // They should differ (different rect dimensions + rotation)
        assert!(p.0 != l.0 || p.1 != l.1);
    }
}
