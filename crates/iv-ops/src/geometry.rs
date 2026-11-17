//! Any sequence of edits reduces to: crop a region of the stored pixels, transform it by one
//! orientation, scale it to one size, then tone-adjust. The viewer previews that with a single
//! GPU texture-coordinate transform; saving applies it to pixels once.

use iv_core::Orientation;

use crate::op::{Adjust, Op, Rect};

#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    /// Stored image size.
    pub stored: (u32, u32),
    /// Region of the stored pixels that survives cropping: x, y, width, height (fractional
    /// when crops were made on a resized image).
    pub crop: [f64; 4],
    /// Transform from the cropped stored region to what is shown (EXIF orientation included).
    pub orientation: Orientation,
    /// Final size in pixels.
    pub size: (u32, u32),
    /// Tone adjustments, in order.
    pub adjusts: Vec<Adjust>,
}

fn clamp_rect(r: Rect, size: (u32, u32)) -> Rect {
    let x = r.x.min(size.0.saturating_sub(1));
    let y = r.y.min(size.1.saturating_sub(1));
    let width = r.width.clamp(1, size.0 - x);
    let height = r.height.clamp(1, size.1 - y);
    Rect { x, y, width, height }
}

impl Geometry {
    /// Reduces `ops`, applied to a stored image of `stored` size shown with `exif` orientation.
    pub fn new(stored: (u32, u32), exif: Orientation, ops: &[Op]) -> Self {
        let stored = (stored.0.max(1), stored.1.max(1));
        let mut g = Geometry {
            stored,
            crop: [0.0, 0.0, stored.0 as f64, stored.1 as f64],
            orientation: exif,
            size: if exif.swaps_dimensions() { (stored.1, stored.0) } else { stored },
            adjusts: Vec::new(),
        };
        for op in ops {
            match *op {
                Op::Orient(t) => {
                    g.orientation = g.orientation.then(t);
                    if t.swaps_dimensions() {
                        g.size = (g.size.1, g.size.0);
                    }
                }
                Op::Crop(r) => {
                    let r = clamp_rect(r, g.size);
                    let (w, h) = (g.size.0 as f64, g.size.1 as f64);
                    let m = g.orientation.display_to_stored_uv();
                    let map =
                        |u: f64, v: f64| (m[0][0] * u + m[0][1] * v + m[0][2], m[1][0] * u + m[1][1] * v + m[1][2]);
                    let a = map(r.x as f64 / w, r.y as f64 / h);
                    let b = map((r.x + r.width) as f64 / w, (r.y + r.height) as f64 / h);
                    let (s0, s1) = (a.0.min(b.0), a.0.max(b.0));
                    let (t0, t1) = (a.1.min(b.1), a.1.max(b.1));
                    let [cx, cy, cw, ch] = g.crop;
                    g.crop = [cx + s0 * cw, cy + t0 * ch, (s1 - s0) * cw, (t1 - t0) * ch];
                    g.size = (r.width, r.height);
                }
                Op::Resize { width, height } => g.size = (width.max(1), height.max(1)),
                Op::Adjust(a) => {
                    if !a.is_identity() {
                        g.adjusts.push(a)
                    }
                }
            }
        }
        g
    }

    /// The crop rounded to whole stored pixels (at least 1×1, inside the image).
    pub fn crop_pixels(&self) -> Rect {
        let [x, y, w, h] = self.crop;
        let x0 = (x.round() as u32).min(self.stored.0 - 1);
        let y0 = (y.round() as u32).min(self.stored.1 - 1);
        let x1 = ((x + w).round() as u32).clamp(x0 + 1, self.stored.0);
        let y1 = ((y + h).round() as u32).clamp(y0 + 1, self.stored.1);
        Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
    }

    pub fn is_cropped(&self) -> bool {
        self.crop_pixels() != Rect { x: 0, y: 0, width: self.stored.0, height: self.stored.1 }
    }

    /// Size after crop and orientation, before any resize.
    pub fn unscaled_size(&self) -> (u32, u32) {
        let c = self.crop_pixels();
        if self.orientation.swaps_dimensions() { (c.height, c.width) } else { (c.width, c.height) }
    }

    /// Affine map from shown UV (0..1 over the edited image) to stored-texture UV, for drawing
    /// the edit on the GPU without touching pixels: `stored = [a b c; d e f] · [u v 1]`.
    pub fn display_to_stored_uv(&self) -> [[f64; 3]; 2] {
        let m = self.orientation.display_to_stored_uv();
        let [cx, cy, cw, ch] = self.crop;
        let (sw, sh) = (self.stored.0 as f64, self.stored.1 as f64);
        let (ox, oy, kx, ky) = (cx / sw, cy / sh, cw / sw, ch / sh);
        [[kx * m[0][0], kx * m[0][1], kx * m[0][2] + ox], [ky * m[1][0], ky * m[1][1], ky * m[1][2] + oy]]
    }

    /// Only rotations/flips (no crop, resize or adjustment) relative to the stored image.
    pub fn is_orientation_only(&self) -> bool {
        !self.is_cropped() && self.size == self.unscaled_size() && self.adjusts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotations_compose_with_exif() {
        let g = Geometry::new((400, 300), Orientation::Rotate90, &[Op::rotate_cw()]);
        assert_eq!(g.orientation, Orientation::Rotate180);
        assert_eq!(g.size, (400, 300));
        assert!(g.is_orientation_only());
        let back = Geometry::new((400, 300), Orientation::Rotate90, &[Op::rotate_ccw()]);
        assert_eq!(back.orientation, Orientation::Normal);
    }

    #[test]
    fn crop_after_rotation_maps_back_to_stored_pixels() {
        // stored 400x300; rotated 90° clockwise it shows 300x400. Cropping the shown top-left
        // 100x50 corner is the stored image's bottom-left: x 0..50, y 200..300.
        let g = Geometry::new(
            (400, 300),
            Orientation::Normal,
            &[Op::rotate_cw(), Op::Crop(Rect { x: 0, y: 0, width: 100, height: 50 })],
        );
        assert_eq!(g.crop_pixels(), Rect { x: 0, y: 200, width: 50, height: 100 });
        assert_eq!(g.size, (100, 50));
        assert_eq!(g.unscaled_size(), (100, 50));
        assert!(!g.is_orientation_only());
    }

    #[test]
    fn crops_nest_and_resizes_scale_later_crops() {
        let ops = [
            Op::Crop(Rect { x: 100, y: 100, width: 200, height: 100 }),
            Op::Resize { width: 100, height: 50 },
            // in the resized 100x50 image, the right half
            Op::Crop(Rect { x: 50, y: 0, width: 50, height: 50 }),
        ];
        let g = Geometry::new((400, 300), Orientation::Normal, &ops);
        assert_eq!(g.crop_pixels(), Rect { x: 200, y: 100, width: 100, height: 100 });
        assert_eq!(g.size, (50, 50));
    }

    #[test]
    fn out_of_range_crops_are_clamped() {
        let g = Geometry::new((10, 10), Orientation::Normal, &[Op::Crop(Rect { x: 8, y: 8, width: 50, height: 50 })]);
        assert_eq!(g.crop_pixels(), Rect { x: 8, y: 8, width: 2, height: 2 });
    }

    #[test]
    fn uv_transform_points_at_the_cropped_region() {
        let g =
            Geometry::new((400, 200), Orientation::Normal, &[Op::Crop(Rect { x: 200, y: 0, width: 200, height: 100 })]);
        let m = g.display_to_stored_uv();
        let at = |u: f64, v: f64| (m[0][0] * u + m[0][1] * v + m[0][2], m[1][0] * u + m[1][1] * v + m[1][2]);
        assert_eq!(at(0.0, 0.0), (0.5, 0.0));
        assert_eq!(at(1.0, 1.0), (1.0, 0.5));
    }
}
