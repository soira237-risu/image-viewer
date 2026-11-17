//! Zoom and pan state, in pure math so it can be tested without a GPU.
//!
//! Coordinates: *image space* is the displayed (already oriented) image in pixels, origin at
//! its top-left. *Screen space* is the viewport in physical pixels, origin at its top-left.

/// Smallest and largest zoom (screen pixels per image pixel).
pub const MIN_ZOOM: f64 = 0.01;
pub const MAX_ZOOM: f64 = 64.0;

/// How a fitted image is sized to the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// The whole image, shrunk to fit but never enlarged past 100 %.
    Shrink,
    /// The whole image, shrunk or enlarged to fill the viewport on one axis.
    Enlarge,
    /// The image's width fills the viewport's (shrunk or enlarged); a taller image starts at
    /// its top and scrolls (webtoons).
    Width,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoomMode {
    /// Fitted; refitted when the viewport or the image changes size.
    Fit(Fit),
    /// User-chosen zoom; kept across viewport resizes.
    Manual,
}

/// The zoom each newly shown image starts at (the `zoom_on_open` setting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZoomOnOpen {
    /// [`Fit::Shrink`]: small images at 100 %, large ones shrunk to fit.
    #[default]
    Fit,
    /// [`Fit::Enlarge`]: small images enlarged too.
    FitEnlarge,
    /// [`Fit::Width`]: for long vertical strips.
    FitWidth,
    /// 100 %, centred.
    ActualSize,
}

impl ZoomOnOpen {
    /// The fit that the Fit command (and a refit) uses: this one's, or shrink-to-fit for
    /// [`ZoomOnOpen::ActualSize`].
    pub fn fit(self) -> Fit {
        match self {
            ZoomOnOpen::Fit | ZoomOnOpen::ActualSize => Fit::Shrink,
            ZoomOnOpen::FitEnlarge => Fit::Enlarge,
            ZoomOnOpen::FitWidth => Fit::Width,
        }
    }
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
    /// How a new image (or this one at a new size) starts.
    open: ZoomOnOpen,
}

impl ViewState {
    /// A view of an image of `image_size` (display size) in a viewport, shrunk to fit.
    pub fn new(image_size: (u32, u32), viewport: (u32, u32)) -> Self {
        Self::opened(image_size, viewport, ZoomOnOpen::Fit)
    }

    /// A view of a newly shown image, at the zoom `open` asks for.
    pub fn opened(image_size: (u32, u32), viewport: (u32, u32), open: ZoomOnOpen) -> Self {
        let mut v = Self {
            image: (image_size.0.max(1) as f64, image_size.1.max(1) as f64),
            viewport: (viewport.0.max(1) as f64, viewport.1.max(1) as f64),
            zoom: 1.0,
            center: (0.0, 0.0),
            mode: ZoomMode::Fit(open.fit()),
            open,
        };
        v.reset();
        v
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn mode(&self) -> ZoomMode {
        self.mode
    }

    /// Whether the view is fitted (in any way) rather than at a zoom of the user's.
    pub fn is_fitted(&self) -> bool {
        matches!(self.mode, ZoomMode::Fit(_))
    }

    pub fn image_size(&self) -> (f64, f64) {
        self.image
    }

    /// The zoom `fit` gives the image in this viewport.
    pub fn fit_zoom_for(&self, fit: Fit) -> f64 {
        let (x, y) = (self.viewport.0 / self.image.0, self.viewport.1 / self.image.1);
        let zoom = match fit {
            Fit::Shrink => x.min(y).min(1.0),
            Fit::Enlarge => x.min(y),
            Fit::Width => x,
        };
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// Zoom that makes the whole image visible, capped at 1.0.
    pub fn fit_zoom(&self) -> f64 {
        self.fit_zoom_for(Fit::Shrink)
    }

    /// Fits the image as `fit` says: centred, or (fit width) from its top.
    pub fn fit_with(&mut self, fit: Fit) {
        self.mode = ZoomMode::Fit(fit);
        self.zoom = self.fit_zoom_for(fit);
        let y = match fit {
            // the top row at the top of the viewport (centred when it all fits: see clamp_center)
            Fit::Width => self.viewport.1 / 2.0 / self.zoom,
            Fit::Shrink | Fit::Enlarge => self.image.1 / 2.0,
        };
        self.center = (self.image.0 / 2.0, y);
        self.clamp_center();
    }

    /// The Fit command: the fit of the `zoom_on_open` setting (shrink-to-fit when that is 100 %).
    pub fn fit(&mut self) {
        self.fit_with(self.open.fit());
    }

    /// Back to how a newly shown image starts.
    pub fn reset(&mut self) {
        match self.open {
            ZoomOnOpen::ActualSize => {
                self.mode = ZoomMode::Manual;
                self.zoom = 1.0;
                self.center = (self.image.0 / 2.0, self.image.1 / 2.0);
                self.clamp_center();
            }
            _ => self.fit(),
        }
    }

    /// The `zoom_on_open` setting changed: the view starts over the new way.
    pub fn set_open(&mut self, open: ZoomOnOpen) {
        self.open = open;
        self.reset();
    }

    /// Replaces the image (e.g. a preview being swapped for the full decode). Keeps the view when
    /// the displayed size matches. Otherwise a fitted view fits the new size the same way, and
    /// a zoomed one starts over as a new image would.
    pub fn set_image_size(&mut self, image_size: (u32, u32)) {
        let new = (image_size.0.max(1) as f64, image_size.1.max(1) as f64);
        if new == self.image {
            return;
        }
        self.image = new;
        match self.mode {
            ZoomMode::Fit(fit) => self.fit_with(fit),
            ZoomMode::Manual => self.reset(),
        }
    }

    pub fn set_viewport(&mut self, viewport: (u32, u32)) {
        let new = (viewport.0.max(1) as f64, viewport.1.max(1) as f64);
        // fit width: the image row at the top of the viewport stays there
        let top = self.center.1 - self.viewport.1 / 2.0 / self.zoom;
        self.viewport = new;
        match self.mode {
            ZoomMode::Fit(Fit::Width) => {
                self.zoom = self.fit_zoom_for(Fit::Width);
                self.center = (self.image.0 / 2.0, top + self.viewport.1 / 2.0 / self.zoom);
                self.clamp_center();
            }
            ZoomMode::Fit(fit) => self.fit_with(fit),
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

    /// Moves the image by a screen-space delta (dragging right moves the image right). A view
    /// fitted to the width stays fitted (it scrolls); a whole-image fit becomes a zoom of the
    /// user's.
    pub fn pan_by(&mut self, dx: f64, dy: f64) {
        if matches!(self.mode, ZoomMode::Fit(Fit::Shrink | Fit::Enlarge)) {
            self.mode = ZoomMode::Manual;
        }
        self.center.0 -= dx / self.zoom;
        self.center.1 -= dy / self.zoom;
        self.clamp_center();
    }

    /// The screen rows the image's top edge can be at when the image is taller than the
    /// viewport: from the lowest one [`Self::snap`] allows (the bottom edge on screen, give or
    /// take less than a pixel) up to 0 (the top edge at the top). `None` when it fits.
    fn top_range_y(&self) -> Option<(f64, f64)> {
        let (image, viewport) = (self.image.1, self.viewport.1);
        // as `clamp_center` decides between centring and clamping
        (image > viewport / self.zoom).then(|| ((viewport - image * self.zoom).ceil(), 0.0))
    }

    /// Whether the image reaches past the top or bottom of the viewport, so there is something
    /// to scroll to.
    pub fn scrolls_y(&self) -> bool {
        self.can_scroll(true) || self.can_scroll(false)
    }

    /// Whether the image can scroll further down (`down`) or up: its top edge is not yet at the
    /// limit that way. Compared with the snapped limits, not with the pixels left hidden: at the
    /// bottom up to one pixel of a fractional height stays hidden and cannot be scrolled to.
    pub fn can_scroll(&self, down: bool) -> bool {
        let Some((lowest, highest)) = self.top_range_y() else { return false };
        let y = self.dest_rect().y;
        // the top edge sits on whole pixels: half a pixel tells "at the limit" from "one away"
        if down { y > lowest + 0.5 } else { y < highest - 0.5 }
    }

    /// Scrolls the image so the view moves `dy` screen pixels down the image (negative: up),
    /// when it reaches past the viewport; the zoom and mode stay. Returns whether it moved.
    pub fn scroll_by(&mut self, dy: f64) -> bool {
        if !self.scrolls_y() {
            return false;
        }
        let before = self.center.1;
        self.center.1 += dy / self.zoom;
        self.clamp_center();
        self.center.1 != before
    }

    pub fn screen_to_image(&self, p: (f64, f64)) -> (f64, f64) {
        (
            self.center.0 + (p.0 - self.viewport.0 / 2.0) / self.zoom,
            self.center.1 + (p.1 - self.viewport.1 / 2.0) / self.zoom,
        )
    }

    /// Inverse of [`Self::screen_to_image`].
    pub fn image_to_screen(&self, p: (f64, f64)) -> (f64, f64) {
        (
            (p.0 - self.center.0) * self.zoom + self.viewport.0 / 2.0,
            (p.1 - self.center.1) * self.zoom + self.viewport.1 / 2.0,
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
        self.snap();
    }

    /// Moves the image by less than half a pixel so its top-left corner lands on a whole screen
    /// pixel: at 100 % every image pixel then covers exactly one screen pixel and is drawn as it
    /// is, not blended with its neighbour. An image that covers the viewport on an axis keeps
    /// covering it (no sliver of background at an edge).
    fn snap(&mut self) {
        fn axis(center: f64, image: f64, viewport: f64, zoom: f64) -> f64 {
            let size = image * zoom;
            let origin = viewport / 2.0 - center * zoom;
            let mut snapped = origin.round();
            if size >= viewport {
                snapped = snapped.clamp((viewport - size).ceil(), 0.0);
            }
            (viewport / 2.0 - snapped) / zoom
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
        // the point stays under the cursor, give or take the snap to whole screen pixels
        let moved = v.image_to_screen(before);
        assert!((moved.0 - anchor.0).abs() <= 0.5 && (moved.1 - anchor.1).abs() <= 0.5, "{moved:?}");
        assert_eq!(v.mode(), ZoomMode::Manual);
        let after = v.screen_to_image(anchor);
        let round = v.image_to_screen(after);
        assert!(close(round.0, anchor.0) && close(round.1, anchor.1), "{round:?}");
    }

    fn whole(v: f64) -> bool {
        (v - v.round()).abs() < 1e-9
    }

    #[test]
    fn the_image_lands_on_whole_screen_pixels() {
        // odd and even sizes against odd and even viewports, fitted (100 %: small images)
        for (image, viewport) in [((200, 100), (1001, 601)), ((201, 99), (1000, 600)), ((4001, 3001), (800, 601))] {
            let v = ViewState::new(image, viewport);
            let r = v.dest_rect();
            assert!(whole(r.x) && whole(r.y), "{image:?} in {viewport:?}: {r:?}");
        }
        // 100 % from a fitted large image, around any point, then panned by whole pixels
        let mut v = ViewState::new((4000, 3000), (1001, 777));
        v.actual_size_at((333.3, 517.9));
        let r = v.dest_rect();
        assert!(whole(r.x) && whole(r.y) && close(v.zoom(), 1.0), "{r:?}");
        v.pan_by(-7.0, 3.0);
        let r = v.dest_rect();
        assert!(whole(r.x) && whole(r.y), "{r:?}");
    }

    #[test]
    fn snapping_never_uncovers_an_edge() {
        // at a zoom where the image is a fractional number of screen pixels wide
        let mut v = ViewState::new((3000, 2000), (800, 600));
        v.set_zoom_at(0.3337, (400.0, 300.0));
        for dx in [1e6, -1e6] {
            v.pan_by(dx, dx);
            let r = v.dest_rect();
            assert!(r.x <= 1e-9 && r.x + r.width >= 800.0 - 1e-9, "{r:?}");
            assert!(r.y <= 1e-9 && r.y + r.height >= 600.0 - 1e-9, "{r:?}");
        }
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
        assert_eq!(v.mode(), ZoomMode::Fit(Fit::Shrink));
    }

    fn rect(v: &ViewState) -> (f64, f64, f64, f64) {
        let r = v.dest_rect();
        (r.x, r.y, r.width, r.height)
    }

    #[test]
    fn each_zoom_on_open_sizes_a_new_image_its_way() {
        let vp = (1000, 800);
        let open = |size, z| ViewState::opened(size, vp, z);
        // shrink to fit (the default, as before): small images stay at 100 %
        assert_eq!(rect(&open((200, 100), ZoomOnOpen::Fit)), (400.0, 350.0, 200.0, 100.0));
        assert_eq!(rect(&open((4000, 2000), ZoomOnOpen::Fit)), (0.0, 150.0, 1000.0, 500.0));
        assert_eq!(open((4000, 2000), ZoomOnOpen::Fit).mode(), ZoomMode::Fit(Fit::Shrink));
        // fit both ways: the small image is enlarged until it touches the viewport
        let v = open((200, 100), ZoomOnOpen::FitEnlarge);
        assert!(close(v.zoom(), 5.0));
        assert_eq!(rect(&v), (0.0, 150.0, 1000.0, 500.0));
        assert_eq!(rect(&open((4000, 2000), ZoomOnOpen::FitEnlarge)), (0.0, 150.0, 1000.0, 500.0));
        // fit width: a tall strip fills the width and starts at its top
        let v = open((800, 4000), ZoomOnOpen::FitWidth);
        assert!(close(v.zoom(), 1.25));
        assert_eq!(rect(&v), (0.0, 0.0, 1000.0, 5000.0));
        assert_eq!(v.mode(), ZoomMode::Fit(Fit::Width));
        // ... and a wide, short image is centred vertically
        assert_eq!(rect(&open((4000, 400), ZoomOnOpen::FitWidth)), (0.0, 350.0, 1000.0, 100.0));
        // actual size: 100 %, centred, the user's zoom from then on
        let v = open((4000, 2000), ZoomOnOpen::ActualSize);
        assert_eq!(rect(&v), (-1500.0, -600.0, 4000.0, 2000.0));
        assert_eq!(v.mode(), ZoomMode::Manual);
        assert_eq!(rect(&open((200, 100), ZoomOnOpen::ActualSize)), (400.0, 350.0, 200.0, 100.0));
    }

    #[test]
    fn two_pages_are_fitted_as_one_image() {
        // two 800×1200 pages side by side are 1600×1200 (see `spread_layout`)
        let spread = ViewState::opened((1600, 1200), (1000, 800), ZoomOnOpen::FitWidth);
        assert_eq!(rect(&spread), (0.0, 25.0, 1000.0, 750.0), "short enough: centred");
        // two long strips: the pair fills the width from the top
        let strips = ViewState::opened((1600, 6000), (1000, 800), ZoomOnOpen::FitWidth);
        assert_eq!(rect(&strips), (0.0, 0.0, 1000.0, 3750.0));
        // a page arriving beside the first refits the pair the same way
        let mut v = ViewState::opened((800, 3000), (1000, 800), ZoomOnOpen::FitWidth);
        v.scroll_by(500.0);
        v.set_image_size((1600, 6000));
        assert_eq!(rect(&v), (0.0, 0.0, 1000.0, 3750.0));
        let mut v = ViewState::opened((800, 1200), (1000, 800), ZoomOnOpen::ActualSize);
        v.set_image_size((1600, 1200));
        assert_eq!(rect(&v), (-300.0, -200.0, 1600.0, 1200.0), "100 %, centred again");
        let mut v = ViewState::opened((800, 1200), (1000, 800), ZoomOnOpen::FitEnlarge);
        v.set_image_size((1600, 1200));
        assert_eq!(rect(&v), (0.0, 25.0, 1000.0, 750.0));
    }

    #[test]
    fn fit_width_scrolls_and_stays_fitted() {
        let mut v = ViewState::opened((800, 4000), (1000, 800), ZoomOnOpen::FitWidth);
        assert!(v.scrolls_y() && v.can_scroll(true) && !v.can_scroll(false));
        assert!(v.scroll_by(300.0));
        assert_eq!(rect(&v), (0.0, -300.0, 1000.0, 5000.0));
        assert_eq!(v.mode(), ZoomMode::Fit(Fit::Width));
        assert!(v.can_scroll(false));
        // the screen still maps onto the right image pixels (selection, crop)
        let p = v.screen_to_image((0.0, 0.0));
        assert!(close(p.0, 0.0) && close(p.1, 240.0), "{p:?}");
        // dragging scrolls too, and keeps the view fitted
        v.pan_by(0.0, 100.0);
        assert_eq!(rect(&v), (0.0, -200.0, 1000.0, 5000.0));
        assert_eq!(v.mode(), ZoomMode::Fit(Fit::Width));
        // down to the bottom and no further
        v.scroll_by(1e7);
        assert_eq!(rect(&v), (0.0, -4200.0, 1000.0, 5000.0));
        assert!(!v.can_scroll(true) && v.can_scroll(false));
        assert!(!v.scroll_by(10.0), "nothing below");
        v.scroll_by(-1e7);
        assert_eq!(rect(&v).1, 0.0);
    }

    #[test]
    fn a_fractional_height_scrolls_to_its_end_and_no_further() {
        // 3000×2000 fitted to a 2560 px width is 1706.67 px tall: the snap leaves 0.67 px of it
        // below the viewport at the bottom, which is the end (the wheel then turns the page)
        let mut v = ViewState::opened((3000, 2000), (2560, 1600), ZoomOnOpen::FitWidth);
        assert!(v.scrolls_y() && v.can_scroll(true) && !v.can_scroll(false));
        let mut steps = 0;
        while v.scroll_by(200.0) {
            steps += 1;
            assert!(steps < 10, "never reaches the end");
        }
        assert!(close(rect(&v).1, -106.0), "{:?}", rect(&v));
        assert!(!v.can_scroll(true), "at the end: nothing left to scroll down to");
        assert!(v.can_scroll(false) && v.scrolls_y());
        // a pixel up from the end is not the end
        assert!(v.scroll_by(-1.0));
        assert!(v.can_scroll(true));
        // the same at the top: a pixel down is not the top
        v.scroll_by(-1e7);
        assert!(!v.can_scroll(false));
        assert!(v.scroll_by(1.0) && v.can_scroll(false));
        // less than a pixel taller than the viewport: nothing to scroll either way
        let v = ViewState::opened((3000, 1876), (2560, 1600), ZoomOnOpen::FitWidth);
        assert!(!v.scrolls_y(), "{:?}", rect(&v));
    }

    #[test]
    fn resizing_a_fit_width_view_keeps_the_row_at_the_top() {
        let mut v = ViewState::opened((800, 4000), (1000, 800), ZoomOnOpen::FitWidth);
        v.scroll_by(300.0); // image row 240 at the top
        v.set_viewport((500, 800));
        assert!(close(v.zoom(), 0.625));
        assert_eq!(rect(&v), (0.0, -150.0, 500.0, 2500.0));
        let top = v.screen_to_image((0.0, 0.0));
        assert!(close(top.1, 240.0), "{top:?}");
        assert_eq!(v.mode(), ZoomMode::Fit(Fit::Width));
    }

    #[test]
    fn the_fit_command_uses_the_configured_fit() {
        let cases = [
            (ZoomOnOpen::Fit, Fit::Shrink),
            (ZoomOnOpen::FitEnlarge, Fit::Enlarge),
            (ZoomOnOpen::FitWidth, Fit::Width),
            // 100 % on open: the Fit command still shrinks the image to fit
            (ZoomOnOpen::ActualSize, Fit::Shrink),
        ];
        for (open, fit) in cases {
            let mut v = ViewState::opened((200, 1000), (1000, 800), open);
            v.zoom_by(3.0);
            v.fit();
            assert_eq!(v.mode(), ZoomMode::Fit(fit), "{open:?}");
            let expected = match fit {
                Fit::Shrink => 0.8,
                Fit::Enlarge => 0.8,
                Fit::Width => 5.0,
            };
            assert!(close(v.zoom(), expected), "{open:?}: {}", v.zoom());
        }
        // a small image: shrink keeps 100 %, enlarge fills the height
        let mut v = ViewState::opened((100, 100), (1000, 800), ZoomOnOpen::FitEnlarge);
        v.actual_size_at((0.0, 0.0));
        v.fit();
        assert!(close(v.zoom(), 8.0));
    }

    #[test]
    fn scrolling_a_whole_image_fit_changes_nothing() {
        let mut v = ViewState::opened((4000, 2000), (1000, 800), ZoomOnOpen::Fit);
        assert!(!v.scrolls_y() && !v.can_scroll(true) && !v.can_scroll(false));
        assert!(!v.scroll_by(100.0));
        // still fitted: a resize refits it (a pan would have made it the user's zoom)
        assert_eq!(v.mode(), ZoomMode::Fit(Fit::Shrink));
        // zoomed in by the user, the arrow keys scroll it like a drag
        v.set_zoom_at(1.0, (500.0, 400.0));
        assert!(v.scroll_by(100.0));
        assert_eq!(v.mode(), ZoomMode::Manual);
    }

    #[test]
    fn changing_zoom_on_open_restarts_the_view() {
        let mut v = ViewState::opened((800, 4000), (1000, 800), ZoomOnOpen::Fit);
        assert!(close(v.zoom(), 0.2));
        v.set_open(ZoomOnOpen::FitWidth);
        assert_eq!(rect(&v), (0.0, 0.0, 1000.0, 5000.0));
        v.set_open(ZoomOnOpen::ActualSize);
        assert!(close(v.zoom(), 1.0) && v.mode() == ZoomMode::Manual);
        assert!(!v.is_fitted());
    }
}
