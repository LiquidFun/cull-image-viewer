//! View transform: EXIF orientation, zoom, pan, and the two fit modes.
//!
//! Deliberately free of any GPU or windowing types so it can be tested exhaustively
//! (REQUIREMENTS.md R5, R6, R7). The renderer consumes the outputs; it makes no
//! decisions of its own.
//!
//! ## Coordinate conventions
//!
//! * *Displayed* size is the image size **after** orientation, so orientations 5-8 have
//!   width and height swapped relative to the stored pixels.
//! * `pan` is the offset, in screen pixels, of the image centre from the viewport
//!   centre. `(0, 0)` is centred.
//! * `zoom` is screen pixels per displayed image pixel. `1.0` is 1:1.

/// EXIF orientation, 1..=8.
///
/// Applied as a texture-coordinate transform rather than by rotating pixels, so it costs
/// nothing and never touches the decoded buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Orientation(u16);

impl Default for Orientation {
    fn default() -> Self {
        Orientation(1)
    }
}

impl Orientation {
    /// Values outside 1..=8 fall back to 1, matching how viewers treat junk EXIF.
    pub fn new(v: u16) -> Self {
        Orientation(if (1..=8).contains(&v) { v } else { 1 })
    }

    pub fn value(self) -> u16 {
        self.0
    }

    /// True when the orientation exchanges the width and height axes.
    pub fn swaps_axes(self) -> bool {
        matches!(self.0, 5..=8)
    }

    /// Displayed size for stored pixel dimensions.
    pub fn displayed_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_axes() {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// 2x2 matrix mapping displayed UV to source UV, both centred on 0.5.
    ///
    /// That is: `uv_src = M * (uv_displayed - 0.5) + 0.5`. Row-major `[m00, m01, m10, m11]`.
    pub fn uv_matrix(self) -> [f32; 4] {
        match self.0 {
            2 => [-1.0, 0.0, 0.0, 1.0],  // flip horizontal
            3 => [-1.0, 0.0, 0.0, -1.0], // rotate 180
            4 => [1.0, 0.0, 0.0, -1.0],  // flip vertical
            5 => [0.0, 1.0, 1.0, 0.0],   // transpose
            6 => [0.0, 1.0, -1.0, 0.0],  // rotate 90 CW
            7 => [0.0, -1.0, -1.0, 0.0], // transverse
            8 => [0.0, -1.0, 1.0, 0.0],  // rotate 270 CW
            _ => [1.0, 0.0, 0.0, 1.0],   // identity
        }
    }

    /// Apply the transform to a displayed UV, yielding the source UV to sample.
    pub fn apply_uv(self, u: f32, v: f32) -> (f32, f32) {
        let m = self.uv_matrix();
        let (a, b) = (u - 0.5, v - 0.5);
        (m[0] * a + m[1] * b + 0.5, m[2] * a + m[3] * b + 0.5)
    }
}

/// What happens to zoom and pan when the displayed image changes (R6).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FitMode {
    /// Reset to fit-to-window, centred, on every switch.
    #[default]
    Refit,
    /// Carry zoom and pan across switches, for comparing the same region between frames.
    Preserve,
}

impl FitMode {
    pub fn toggled(self) -> Self {
        match self {
            FitMode::Refit => FitMode::Preserve,
            FitMode::Preserve => FitMode::Refit,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FitMode::Refit => "refit",
            FitMode::Preserve => "keep zoom",
        }
    }
}

/// Hard bounds on zoom, so a stray wheel spin cannot leave the user lost.
const MIN_ZOOM: f64 = 0.02;
const MAX_ZOOM: f64 = 64.0;

/// Multiplicative zoom step per wheel detent.
pub const ZOOM_STEP: f64 = 1.25;

#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: f64,
    pub height: f64,
}

impl Viewport {
    pub fn new(width: f64, height: f64) -> Self {
        // Guard against a zero-sized window during startup or minimisation, which
        // would otherwise produce NaN throughout.
        Self {
            width: width.max(1.0),
            height: height.max(1.0),
        }
    }
}

/// Zoom and pan for the current image.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub zoom: f64,
    /// Screen-pixel offset of the image centre from the viewport centre.
    pub pan: (f64, f64),
}

impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: (0.0, 0.0),
        }
    }
}

impl View {
    /// Zoom that fits the whole image inside the viewport (letterboxed, never cropped).
    ///
    /// Images smaller than the viewport are **not** enlarged: fit never exceeds 1:1, so
    /// a small export is shown at native size rather than blown up.
    pub fn fit_zoom(displayed: (u32, u32), vp: Viewport) -> f64 {
        let (w, h) = (displayed.0.max(1) as f64, displayed.1.max(1) as f64);
        (vp.width / w).min(vp.height / h).min(1.0).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// Fit-to-window, centred.
    pub fn fitted(displayed: (u32, u32), vp: Viewport) -> Self {
        Self {
            zoom: Self::fit_zoom(displayed, vp),
            pan: (0.0, 0.0),
        }
    }

    /// On-screen size of the image at the current zoom.
    pub fn scaled_size(&self, displayed: (u32, u32)) -> (f64, f64) {
        (
            displayed.0 as f64 * self.zoom,
            displayed.1 as f64 * self.zoom,
        )
    }

    /// Clamp pan so the image cannot be dragged away into empty space.
    ///
    /// Per axis: if the image is larger than the viewport, its edges may not move inside
    /// the viewport edges; if it is smaller, it is pinned to centre. This mirrors how
    /// geeqie behaves and stops the image being lost off-screen.
    pub fn clamped(mut self, displayed: (u32, u32), vp: Viewport) -> Self {
        let (sw, sh) = self.scaled_size(displayed);
        let limit = |scaled: f64, viewport: f64| -> f64 {
            if scaled <= viewport {
                0.0
            } else {
                (scaled - viewport) / 2.0
            }
        };
        let lx = limit(sw, vp.width);
        let ly = limit(sh, vp.height);
        self.pan = (self.pan.0.clamp(-lx, lx), self.pan.1.clamp(-ly, ly));
        self
    }

    /// Zoom by `factor`, keeping the image point under `cursor` stationary.
    ///
    /// `cursor` is in screen pixels **relative to the viewport centre**.
    pub fn zoom_at(mut self, factor: f64, cursor: (f64, f64)) -> Self {
        let target = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        // Clamping may have reduced the effective factor; recompute it so the anchor
        // stays exact at the zoom limits instead of drifting.
        let effective = target / self.zoom;
        self.pan = (
            cursor.0 - effective * (cursor.0 - self.pan.0),
            cursor.1 - effective * (cursor.1 - self.pan.1),
        );
        self.zoom = target;
        self
    }

    /// Drag by a screen-pixel delta.
    pub fn panned(mut self, delta: (f64, f64)) -> Self {
        self.pan = (self.pan.0 + delta.0, self.pan.1 + delta.1);
        self
    }

    /// Map a screen point (relative to viewport centre) to displayed image pixels,
    /// with the image centre at the origin. Inverse of the render transform.
    pub fn screen_to_image(&self, screen: (f64, f64)) -> (f64, f64) {
        (
            (screen.0 - self.pan.0) / self.zoom,
            (screen.1 - self.pan.1) / self.zoom,
        )
    }

    /// What the next image should start from, given the mode (R6).
    pub fn on_image_change(
        self,
        mode: FitMode,
        displayed: (u32, u32),
        vp: Viewport,
    ) -> Self {
        match mode {
            FitMode::Refit => Self::fitted(displayed, vp),
            // Re-clamp, because the new image may be a different size (or a different
            // orientation) and the old pan could now be out of bounds.
            FitMode::Preserve => self.clamped(displayed, vp),
        }
    }

    /// Half-extent of the quad in clip space, for the vertex shader.
    ///
    /// Returns `(sx, sy, tx, ty)`: scale and translation in normalised device
    /// coordinates, where the viewport spans -1..1.
    pub fn clip_transform(&self, displayed: (u32, u32), vp: Viewport) -> (f32, f32, f32, f32) {
        let (sw, sh) = self.scaled_size(displayed);
        (
            (sw / vp.width) as f32,
            (sh / vp.height) as f32,
            (2.0 * self.pan.0 / vp.width) as f32,
            // Screen y grows downward, clip y grows upward.
            (-2.0 * self.pan.1 / vp.height) as f32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: Viewport = Viewport {
        width: 1000.0,
        height: 800.0,
    };

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    /// UV maths is done in f32, so round-trips only hold to f32 precision.
    fn approx_uv(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    /// The clip transform is emitted as f32, so aspect comparisons need a relative
    /// tolerance rather than an absolute one.
    fn relative_error(a: f64, b: f64) -> f64 {
        ((a - b) / b).abs()
    }

    // --- Orientation ---

    #[test]
    fn orientation_rejects_out_of_range() {
        assert_eq!(Orientation::new(0).value(), 1);
        assert_eq!(Orientation::new(9).value(), 1);
        assert_eq!(Orientation::new(65535).value(), 1);
        for v in 1..=8 {
            assert_eq!(Orientation::new(v).value(), v);
        }
    }

    #[test]
    fn only_orientations_5_to_8_swap_axes() {
        for v in 1..=8u16 {
            let expect = (5..=8).contains(&v);
            assert_eq!(Orientation::new(v).swaps_axes(), expect, "orientation {v}");
        }
        // The real data contains orientation 8, so this case matters concretely.
        assert_eq!(
            Orientation::new(8).displayed_size(6192, 4128),
            (4128, 6192),
            "a portrait Sony frame must report portrait dimensions"
        );
        assert_eq!(Orientation::new(1).displayed_size(6192, 4128), (6192, 4128));
    }

    #[test]
    fn identity_orientation_leaves_uv_untouched() {
        let o = Orientation::new(1);
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.5, 0.5)] {
            let (a, b) = o.apply_uv(u, v);
            approx(a as f64, u as f64);
            approx(b as f64, v as f64);
        }
    }

    #[test]
    fn rotate_90_cw_maps_displayed_corners_to_source() {
        // Orientation 6: the displayed top-left comes from the source bottom-left.
        let o = Orientation::new(6);
        let (u, v) = o.apply_uv(0.0, 0.0);
        approx(u as f64, 0.0);
        approx(v as f64, 1.0);
        // Displayed top-right comes from source top-left.
        let (u, v) = o.apply_uv(1.0, 0.0);
        approx(u as f64, 0.0);
        approx(v as f64, 0.0);
    }

    #[test]
    fn rotate_270_cw_maps_displayed_corners_to_source() {
        // Orientation 8 is the one present in the real data.
        let o = Orientation::new(8);
        let (u, v) = o.apply_uv(0.0, 0.0);
        approx(u as f64, 1.0);
        approx(v as f64, 0.0);
        let (u, v) = o.apply_uv(1.0, 0.0);
        approx(u as f64, 1.0);
        approx(v as f64, 1.0);
    }

    #[test]
    fn flips_mirror_the_expected_axis() {
        let (u, v) = Orientation::new(2).apply_uv(0.0, 0.25);
        approx(u as f64, 1.0);
        approx(v as f64, 0.25);

        let (u, v) = Orientation::new(4).apply_uv(0.25, 0.0);
        approx(u as f64, 0.25);
        approx(v as f64, 1.0);
    }

    #[test]
    fn rotate_180_maps_corner_to_opposite_corner() {
        let (u, v) = Orientation::new(3).apply_uv(0.0, 0.0);
        approx(u as f64, 1.0);
        approx(v as f64, 1.0);
    }

    #[test]
    fn every_orientation_is_a_bijection_on_the_unit_square() {
        // Each transform must map the unit square onto itself, or we would sample
        // outside the texture and show clamped edge pixels.
        for o in (1..=8).map(Orientation::new) {
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let (a, b) = o.apply_uv(u, v);
                assert!(
                    (-1e-6..=1.0 + 1e-6).contains(&a) && (-1e-6..=1.0 + 1e-6).contains(&b),
                    "orientation {} sent ({u},{v}) to ({a},{b})",
                    o.value()
                );
            }
        }
    }

    #[test]
    fn orientation_transforms_are_involutive_or_cyclic() {
        // Applying orientation 3 (180 degrees) twice returns to the start.
        let o = Orientation::new(3);
        let (u, v) = o.apply_uv(0.3, 0.7);
        let (u2, v2) = o.apply_uv(u, v);
        approx_uv(u2, 0.3);
        approx_uv(v2, 0.7);

        // Rotating 90 CW four times is also the identity.
        let o = Orientation::new(6);
        let (mut u, mut v) = (0.2f32, 0.9f32);
        for _ in 0..4 {
            (u, v) = o.apply_uv(u, v);
        }
        approx_uv(u, 0.2);
        approx_uv(v, 0.9);
    }

    // --- Fit ---

    #[test]
    fn fit_letterboxes_a_large_landscape_image() {
        // 6192x4128 into 1000x800: width-limited at 1000/6192.
        let z = View::fit_zoom((6192, 4128), VP);
        approx(z, 1000.0 / 6192.0);
        // The fitted image must be inside the viewport on both axes.
        let v = View::fitted((6192, 4128), VP);
        let (w, h) = v.scaled_size((6192, 4128));
        assert!(w <= VP.width + 1e-9 && h <= VP.height + 1e-9);
    }

    #[test]
    fn fit_is_height_limited_for_a_portrait_image() {
        // Orientation 8 applied to 6192x4128 gives 4128x6192.
        let displayed = Orientation::new(8).displayed_size(6192, 4128);
        let z = View::fit_zoom(displayed, VP);
        approx(z, 800.0 / 6192.0);
    }

    #[test]
    fn fit_never_enlarges_small_images() {
        // A 100x100 image in a 1000x800 viewport stays 1:1 rather than being blown up.
        approx(View::fit_zoom((100, 100), VP), 1.0);
    }

    #[test]
    fn fit_survives_degenerate_inputs() {
        // Zero dimensions must not produce NaN or infinity.
        let z = View::fit_zoom((0, 0), VP);
        assert!(z.is_finite() && z > 0.0, "got {z}");
        let z = View::fit_zoom((6192, 4128), Viewport::new(0.0, 0.0));
        assert!(z.is_finite() && z > 0.0, "got {z}");
    }

    // --- Zoom anchoring ---

    #[test]
    fn zoom_keeps_the_point_under_the_cursor_fixed() {
        let displayed = (2000u32, 1000u32);
        let v = View { zoom: 0.5, pan: (30.0, -20.0) };
        let cursor = (120.0, -75.0);

        let before = v.screen_to_image(cursor);
        let after = v.zoom_at(ZOOM_STEP, cursor).screen_to_image(cursor);

        approx(before.0, after.0);
        approx(before.1, after.1);
        let _ = displayed;
    }

    #[test]
    fn zoom_out_also_keeps_the_cursor_anchored() {
        let v = View { zoom: 3.0, pan: (-200.0, 90.0) };
        let cursor = (-310.0, 45.0);
        let before = v.screen_to_image(cursor);
        let after = v.zoom_at(1.0 / ZOOM_STEP, cursor).screen_to_image(cursor);
        approx(before.0, after.0);
        approx(before.1, after.1);
    }

    #[test]
    fn zoom_at_viewport_centre_leaves_pan_proportional() {
        // Anchoring at the centre with zero pan must not introduce any pan.
        let v = View { zoom: 1.0, pan: (0.0, 0.0) };
        let z = v.zoom_at(2.0, (0.0, 0.0));
        approx(z.pan.0, 0.0);
        approx(z.pan.1, 0.0);
        approx(z.zoom, 2.0);
    }

    #[test]
    fn zoom_respects_limits_and_stays_anchored_there() {
        let v = View { zoom: MAX_ZOOM, pan: (10.0, 10.0) };
        let cursor = (50.0, 60.0);
        let before = v.screen_to_image(cursor);
        let z = v.zoom_at(4.0, cursor);
        approx(z.zoom, MAX_ZOOM);
        // Zoom was refused, so the anchor must be untouched rather than drifting.
        let after = z.screen_to_image(cursor);
        approx(before.0, after.0);
        approx(before.1, after.1);

        let v = View { zoom: MIN_ZOOM, pan: (0.0, 0.0) };
        approx(v.zoom_at(0.001, (0.0, 0.0)).zoom, MIN_ZOOM);
    }

    #[test]
    fn many_zoom_steps_do_not_drift() {
        // Zoom in ten times then out ten times at a fixed cursor: we must land back
        // where we started, or repeated wheel use would slowly wander.
        let cursor = (137.0, -88.0);
        let start = View { zoom: 0.4, pan: (12.0, -5.0) };
        let mut v = start;
        for _ in 0..10 {
            v = v.zoom_at(ZOOM_STEP, cursor);
        }
        for _ in 0..10 {
            v = v.zoom_at(1.0 / ZOOM_STEP, cursor);
        }
        assert!((v.zoom - start.zoom).abs() < 1e-9, "zoom drifted to {}", v.zoom);
        assert!((v.pan.0 - start.pan.0).abs() < 1e-6, "pan.x drifted to {}", v.pan.0);
        assert!((v.pan.1 - start.pan.1).abs() < 1e-6, "pan.y drifted to {}", v.pan.1);
    }

    // --- Pan clamping ---

    #[test]
    fn pan_is_pinned_to_centre_when_image_is_smaller_than_viewport() {
        let v = View { zoom: 0.1, pan: (500.0, 500.0) }.clamped((2000, 1000), VP);
        approx(v.pan.0, 0.0);
        approx(v.pan.1, 0.0);
    }

    #[test]
    fn pan_is_bounded_when_image_is_larger_than_viewport() {
        // 2000x1000 at zoom 1 is wider and taller than 1000x800.
        let displayed = (2000u32, 1000u32);
        let v = View { zoom: 1.0, pan: (99999.0, -99999.0) }.clamped(displayed, VP);
        approx(v.pan.0, (2000.0 - 1000.0) / 2.0);
        approx(v.pan.1, -(1000.0 - 800.0) / 2.0);
    }

    #[test]
    fn clamping_is_per_axis() {
        // Wide but short: x is bounded, y is pinned to centre.
        let displayed = (4000u32, 100u32);
        let v = View { zoom: 1.0, pan: (5000.0, 300.0) }.clamped(displayed, VP);
        approx(v.pan.0, 1500.0);
        approx(v.pan.1, 0.0);
    }

    #[test]
    fn clamping_is_idempotent() {
        let displayed = (3000u32, 2000u32);
        let a = View { zoom: 1.0, pan: (9000.0, 9000.0) }.clamped(displayed, VP);
        let b = a.clamped(displayed, VP);
        approx(a.pan.0, b.pan.0);
        approx(a.pan.1, b.pan.1);
    }

    // --- Fit modes (R6) ---

    #[test]
    fn refit_mode_resets_zoom_and_pan_on_every_switch() {
        let zoomed = View { zoom: 8.0, pan: (400.0, 300.0) };
        let next = zoomed.on_image_change(FitMode::Refit, (6192, 4128), VP);
        approx(next.zoom, View::fit_zoom((6192, 4128), VP));
        approx(next.pan.0, 0.0);
        approx(next.pan.1, 0.0);
    }

    #[test]
    fn preserve_mode_keeps_zoom_across_switches() {
        // The focus-checking workflow: same magnification on the next frame.
        let zoomed = View { zoom: 2.0, pan: (100.0, 50.0) };
        let next = zoomed.on_image_change(FitMode::Preserve, (6192, 4128), VP);
        approx(next.zoom, 2.0);
        approx(next.pan.0, 100.0);
        approx(next.pan.1, 50.0);
    }

    #[test]
    fn preserve_mode_reclamps_when_the_next_image_is_smaller() {
        // Panned far into a big image, then onto a small one: pan must be brought back
        // in bounds rather than leaving the small image off-screen.
        let panned = View { zoom: 1.0, pan: (1500.0, 0.0) };
        let next = panned.on_image_change(FitMode::Preserve, (200, 200), VP);
        approx(next.pan.0, 0.0);
        approx(next.zoom, 1.0);
    }

    #[test]
    fn fit_mode_toggles_and_round_trips() {
        assert_eq!(FitMode::Refit.toggled(), FitMode::Preserve);
        assert_eq!(FitMode::Refit.toggled().toggled(), FitMode::Refit);
        assert_eq!(FitMode::default(), FitMode::Refit);
    }

    // --- Clip-space transform ---

    #[test]
    fn fitted_image_spans_at_most_the_full_viewport_in_clip_space() {
        let displayed = (6192u32, 4128u32);
        let v = View::fitted(displayed, VP);
        let (sx, sy, tx, ty) = v.clip_transform(displayed, VP);
        assert!(sx <= 1.0 + 1e-6 && sy <= 1.0 + 1e-6, "sx {sx} sy {sy}");
        // One axis must touch the edge exactly, since fit is a tight fit.
        assert!((sx - 1.0).abs() < 1e-6 || (sy - 1.0).abs() < 1e-6);
        approx(tx as f64, 0.0);
        approx(ty as f64, 0.0);
    }

    #[test]
    fn clip_transform_flips_pan_y_for_screen_coordinates() {
        let displayed = (1000u32, 800u32);
        let v = View { zoom: 1.0, pan: (0.0, 100.0) };
        let (_, _, _, ty) = v.clip_transform(displayed, VP);
        // Positive screen-y pan (downward) must become negative clip-y.
        assert!(ty < 0.0, "expected downward pan to be negative in clip space");
    }

    /// Regression for the "images look squished" report.
    ///
    /// `clip_transform` is only correct if clip space maps onto exactly the rect whose
    /// size was passed as `vp`. The renderer originally drew across the whole window
    /// while passing the narrower sidebar-excluded width, stretching everything
    /// horizontally by window/(window - sidebar). This checks the contract the renderer
    /// must uphold: with a correct viewport, on-screen aspect equals image aspect.
    #[test]
    fn clip_transform_preserves_aspect_ratio() {
        // A 3:2 landscape frame and its portrait counterpart, in a 16:9 image area.
        for displayed in [(6192u32, 4128u32), (4128, 6192)] {
            let area = Viewport::new(1320.0, 1000.0);
            let v = View::fitted(displayed, area);
            let (sx, sy, _, _) = v.clip_transform(displayed, area);

            // Half-extents in clip space become pixels by scaling with the area size.
            let px_w = f64::from(sx) * area.width;
            let px_h = f64::from(sy) * area.height;
            let on_screen = px_w / px_h;
            let image = f64::from(displayed.0) / f64::from(displayed.1);
            assert!(
                relative_error(on_screen, image) < 1e-6,
                "aspect distorted for {displayed:?}: on-screen {on_screen}, image {image}"
            );
        }
    }

    /// The same check across a range of area shapes, including very wide and very tall.
    #[test]
    fn aspect_ratio_holds_for_any_area_shape() {
        let displayed = (6192u32, 4128u32);
        for (w, h) in [
            (1920.0, 1080.0),
            (1000.0, 1000.0),
            (600.0, 1400.0),
            (3000.0, 400.0),
            (1320.0, 980.0),
        ] {
            let area = Viewport::new(w, h);
            let v = View::fitted(displayed, area);
            let (sx, sy, _, _) = v.clip_transform(displayed, area);
            let on_screen = (f64::from(sx) * area.width) / (f64::from(sy) * area.height);
            let image = f64::from(displayed.0) / f64::from(displayed.1);
            assert!(
                relative_error(on_screen, image) < 1e-6,
                "aspect distorted in {w}x{h}: {on_screen} vs {image}"
            );
        }
    }

    /// Zoom must scale both axes equally, or the image would distort as it is zoomed.
    #[test]
    fn zooming_does_not_distort_aspect() {
        let displayed = (4128u32, 6192u32);
        let area = Viewport::new(1320.0, 1000.0);
        let mut v = View::fitted(displayed, area);
        let image = f64::from(displayed.0) / f64::from(displayed.1);
        for _ in 0..8 {
            v = v.zoom_at(ZOOM_STEP, (50.0, -30.0));
            let (sx, sy, _, _) = v.clip_transform(displayed, area);
            let on_screen = (f64::from(sx) * area.width) / (f64::from(sy) * area.height);
            assert!(
                relative_error(on_screen, image) < 1e-6,
                "aspect drifted while zooming: {on_screen} vs {image}"
            );
        }
    }

    #[test]
    fn clip_transform_is_finite_for_degenerate_viewport() {
        let (sx, sy, tx, ty) = View::default().clip_transform((0, 0), Viewport::new(0.0, 0.0));
        for v in [sx, sy, tx, ty] {
            assert!(v.is_finite(), "non-finite clip transform component {v}");
        }
    }
}
