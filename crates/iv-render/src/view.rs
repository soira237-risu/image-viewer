//! Zoom and pan state, in pure math so it can be tested without a GPU.
//!
//! Coordinates: *image space* is the displayed (already oriented) image in pixels, origin at
//! its top-left. *Screen space* is the viewport in physical pixels, origin at its top-left.

/// Smallest and largest zoom (screen pixels per image pixel).
pub const MIN_ZOOM: f64 = 0.01;
pub const MAX_ZOOM: f64 = 64.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoomMode {
    /// Shrink to fit the viewport, never enlarge past 100 %.
    Fit,
    /// User-chosen zoom; kept across viewport resizes.
    Manual,
}

/// Where the image lands on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug)]
pub struct ViewState {
    image: (f64, f64),
    viewport: (f64, f64),
    zoom: f64,
    /// Image-space point shown at the viewport centre.
    center: (f64, f64),
    mode: ZoomMode,
}

impl ViewState {
    /// A view of an image of `image_size` (display size) in a viewport, fitted.
    pub fn new(image_size: (u32, u32), viewport: (u32, u32)) -> Self {
        let mut v = Self {
            image: (image_size.0.max(1) as f64, image_size.1.max(1) as f64),
            viewport: (viewport.0.max(1) as f64, viewport.1.max(1) as f64),
            zoom: 1.0,
            center: (0.0, 0.0),
            mode: ZoomMode::Fit,
        };
        v.fit();
        v
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn mode(&self) -> ZoomMode {
        self.mode
    }

    pub fn image_size(&self) -> (f64, f64) {
        self.image
    }

    /// Zoom that makes the whole image visible, capped at 1.0.
    pub fn fit_zoom(&self) -> f64 {
        (self.viewport.0 / self.image.0).min(self.viewport.1 / self.image.1).min(1.0).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    pub fn fit(&mut self) {
        self.mode = ZoomMode::Fit;
        self.zoom = self.fit_zoom();
        self.center = (self.image.0 / 2.0, self.image.1 / 2.0);
    }

    /// Replaces the image (e.g. a preview being swapped for the full decode). Keeps the view when
    /// the displayed size matches, refits otherwise.
    pub fn set_image_size(&mut self, image_size: (u32, u32)) {
        let new = (image_size.0.max(1) as f64, image_size.1.max(1) as f64);
        if new == self.image {
            return;
        }
        self.image = new;
        self.fit();
    }

    pub fn set_viewport(&mut self, viewport: (u32, u32)) {
        self.viewport = (viewport.0.max(1) as f64, viewport.1.max(1) as f64);
        match self.mode {
            ZoomMode::Fit => self.fit(),
            ZoomMode::Manual => self.clamp_center(),
        }
    }

    /// Sets an absolute zoom, keeping the image point under `anchor` (screen space) in place.
    pub fn set_zoom_at(&mut self, zoom: f64, anchor: (f64, f64)) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let before = self.screen_to_image(anchor);
        self.zoom = zoom;
        self.mode = ZoomMode::Manual;
        // put `before` back under the anchor
        self.center = (
            before.0 - (anchor.0 - self.viewport.0 / 2.0) / zoom,
            before.1 - (anchor.1 - self.viewport.1 / 2.0) / zoom,
        );
        self.clamp_center();
    }

    /// Multiplies the zoom by `factor` around `anchor`.
    pub fn zoom_by_at(&mut self, factor: f64, anchor: (f64, f64)) {
        self.set_zoom_at(self.zoom * factor, anchor);
    }

    /// Zoom by `factor` around the viewport centre.
    pub fn zoom_by(&mut self, factor: f64) {
        self.zoom_by_at(factor, (self.viewport.0 / 2.0, self.viewport.1 / 2.0));
    }

    /// 100 % around `anchor`.
    pub fn actual_size_at(&mut self, anchor: (f64, f64)) {
        self.set_zoom_at(1.0, anchor);
    }

    /// Moves the image by a screen-space delta (dragging right moves the image right).
    pub fn pan_by(&mut self, dx: f64, dy: f64) {
        if self.mode == ZoomMode::Fit {
            self.mode = ZoomMode::Manual;
        }
        self.center.0 -= dx / self.zoom;
        self.center.1 -= dy / self.zoom;
        self.clamp_center();
    }

    pub fn screen_to_image(&self, p: (f64, f64)) -> (f64, f64) {
        (
            self.center.0 + (p.0 - self.viewport.0 / 2.0) / self.zoom,
            self.center.1 + (p.1 - self.viewport.1 / 2.0) / self.zoom,
        )
    }

    /// Screen rectangle covered by the whole image.
    pub fn dest_rect(&self) -> ScreenRect {
        let width = self.image.0 * self.zoom;
        let height = self.image.1 * self.zoom;
        ScreenRect {
            x: self.viewport.0 / 2.0 - self.center.0 * self.zoom,
            y: self.viewport.1 / 2.0 - self.center.1 * self.zoom,
            width,
            height,
        }
    }

    /// Per axis: an image narrower than the viewport is centred; a wider one may not leave
    /// empty space between its edge and the viewport edge.
    fn clamp_center(&mut self) {
        fn axis(center: f64, image: f64, viewport: f64, zoom: f64) -> f64 {
            let half_view = viewport / 2.0 / zoom;
            if image <= viewport / zoom { image / 2.0 } else { center.clamp(half_view, image - half_view) }
        }
        self.center.0 = axis(self.center.0, self.image.0, self.viewport.0, self.zoom);
        self.center.1 = axis(self.center.1, self.image.1, self.viewport.1, self.zoom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn fit_shrinks_large_images_and_never_enlarges_small_ones() {
        let v = ViewState::new((4000, 2000), (1000, 1000));
        assert!(close(v.zoom(), 0.25));
        let r = v.dest_rect();
        assert!(close(r.width, 1000.0) && close(r.height, 500.0));
        assert!(close(r.x, 0.0) && close(r.y, 250.0), "centred: {r:?}");

        let small = ViewState::new((200, 100), (1000, 1000));
        assert!(close(small.zoom(), 1.0));
        assert!(close(small.dest_rect().x, 400.0));
    }

    #[test]
    fn zoom_keeps_the_anchor_point_fixed() {
        let mut v = ViewState::new((4000, 3000), (800, 600));
        let anchor = (100.0, 450.0);
        let before = v.screen_to_image(anchor);
        v.zoom_by_at(4.0, anchor);
        let after = v.screen_to_image(anchor);
        assert!(close(before.0, after.0) && close(before.1, after.1), "{before:?} vs {after:?}");
        assert_eq!(v.mode(), ZoomMode::Manual);
    }

    #[test]
    fn pan_cannot_push_a_large_image_off_screen() {
        let mut v = ViewState::new((4000, 3000), (800, 600));
        v.actual_size_at((400.0, 300.0));
        v.pan_by(1e6, 1e6);
        let r = v.dest_rect();
        assert!(close(r.x, 0.0) && close(r.y, 0.0), "left/top edge pinned: {r:?}");
        v.pan_by(-1e7, -1e7);
        let r = v.dest_rect();
        assert!(close(r.x + r.width, 800.0) && close(r.y + r.height, 600.0), "right/bottom pinned: {r:?}");
    }

    #[test]
    fn small_axis_stays_centred_while_panning() {
        let mut v = ViewState::new((4000, 100), (800, 600));
        v.actual_size_at((400.0, 300.0));
        v.pan_by(0.0, 500.0);
        let r = v.dest_rect();
        assert!(close(r.y, 250.0), "{r:?}");
    }

    #[test]
    fn resize_refits_in_fit_mode_and_keeps_zoom_in_manual_mode() {
        let mut v = ViewState::new((2000, 1000), (1000, 1000));
        v.set_viewport((500, 500));
        assert!(close(v.zoom(), 0.25));
        v.zoom_by(2.0);
        v.set_viewport((1000, 1000));
        assert!(close(v.zoom(), 0.5));
    }

    #[test]
    fn zoom_is_clamped() {
        let mut v = ViewState::new((100, 100), (100, 100));
        v.zoom_by(1e9);
        assert!(close(v.zoom(), MAX_ZOOM));
        v.zoom_by(1e-12);
        assert!(close(v.zoom(), MIN_ZOOM));
    }

    #[test]
    fn same_size_image_swap_keeps_view() {
        let mut v = ViewState::new((2000, 1000), (500, 500));
        v.zoom_by(3.0);
        let z = v.zoom();
        v.set_image_size((2000, 1000));
        assert!(close(v.zoom(), z));
        v.set_image_size((1000, 1000));
        assert_eq!(v.mode(), ZoomMode::Fit);
    }
}
