//! CPU renderer: draws pages of an image into a BGRA frame that GDI can blit.
//!
//! Only the part of a page that is on screen is touched: that region of the stored pixels is
//! cut out with a sub-pixel crop and resampled straight to its on-screen size (SIMD,
//! multi-threaded `fast_image_resize`), then written to the frame through the page's
//! orientation (a D4 transform, so it is an index remap), with tone adjustments, the
//! transparency checkerboard and the selection dimming applied per pixel. The image itself is
//! never copied, rotated or converted as a whole, so drawing costs memory on the order of the
//! window size, not the image size.

use std::sync::OnceLock;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use iv_core::{ImageBuf, PixelFormat};
use iv_ops::Adjust;
use rayon::prelude::*;

use crate::view::ScreenRect;

/// Checkerboard behind transparent pixels: cell size and the two greys (sRGB).
const CHECKER_CELL: usize = 8;
const CHECKER: [[u8; 3]; 2] = [[118, 118, 118], [149, 149, 149]];
/// Outside a selection, light is scaled by this factor (in linear light).
const SELECTION_DIM: f64 = 0.35;

/// Window-sized pixels, one `0x00RRGGBB` word per pixel (a top-down 32-bit DIB).
#[derive(Default)]
pub struct Frame {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        let mut f = Self::default();
        f.resize(width, height);
        f
    }

    /// Resizes the frame; contents are undefined until the next [`render`].
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.pixels = vec![0; width as usize * height as usize];
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// The sRGB colour at (x, y).
    pub fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let p = self.pixels[(y * self.width + x) as usize];
        [(p >> 16) as u8, (p >> 8) as u8, p as u8]
    }
}

/// An image ready to draw: RGBA8 stored pixels (sharing the decoded buffer when it already is
/// RGBA8).
#[derive(Clone)]
pub struct Picture {
    image: ImageBuf,
    has_alpha: bool,
}

impl Picture {
    pub fn new(img: &ImageBuf, has_alpha: bool) -> Self {
        let image = match img.format() {
            PixelFormat::Rgba8 => img.clone(),
            _ => ImageBuf::new(img.width(), img.height(), PixelFormat::Rgba8, img.to_rgba8().into_owned())
                .expect("RGBA8 conversion keeps the dimensions"),
        };
        Self { image, has_alpha }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.image.width(), self.image.height())
    }
}

/// One image placed on screen.
pub struct Page<'a> {
    pub picture: &'a Picture,
    /// Shown UV (0..1 over the page) → stored UV: `[a b c; d e f]` with a D4 linear part
    /// (`iv_ops::Geometry::display_to_stored_uv`).
    pub uv: [[f64; 3]; 2],
    /// Tone adjustments, in order (the same function saving applies).
    pub adjusts: &'a [Adjust],
    /// Where the whole page lands, in frame pixels (may extend past the frame).
    pub dest: ScreenRect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub background: [u8; 3],
    /// At or above this many screen pixels per image pixel, pixels are drawn as sharp squares.
    pub nearest_above_zoom: f64,
}

impl Default for Style {
    fn default() -> Self {
        Self { background: [26, 26, 28], nearest_above_zoom: 3.0 }
    }
}

fn pack([r, g, b]: [u8; 3]) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// sRGB value → the same value with its light scaled by [`SELECTION_DIM`].
fn dim_table() -> &'static [u8; 256] {
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let decode = |c: f64| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        let encode = |c: f64| if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
        std::array::from_fn(|i| (encode(decode(i as f64 / 255.0) * SELECTION_DIM) * 255.0).round() as u8)
    })
}

/// Fills `frame` with the background and draws `pages` over it. Outside `selection`, page
/// pixels are dimmed.
pub fn render(frame: &mut Frame, pages: &[Page<'_>], selection: Option<ScreenRect>, style: &Style) {
    frame.pixels.fill(pack(style.background));
    for page in pages {
        draw_page(frame, page, selection, style);
    }
}

/// Integer pixel span whose centres fall inside `[start, start + len)`, clipped to `0..limit`.
fn pixel_span(start: f64, len: f64, limit: u32) -> (usize, usize) {
    let lo = (start - 0.5).ceil().clamp(0.0, limit as f64) as usize;
    let hi = (start + len - 0.5).ceil().clamp(0.0, limit as f64) as usize;
    (lo, hi.max(lo))
}

fn draw_page(frame: &mut Frame, page: &Page<'_>, selection: Option<ScreenRect>, style: &Style) {
    let d = page.dest;
    if !(d.width > 0.0 && d.height > 0.0) {
        return;
    }
    let (x0, x1) = pixel_span(d.x, d.width, frame.width);
    let (y0, y1) = pixel_span(d.y, d.height, frame.height);
    let (dw, dh) = (x1 - x0, y1 - y0);
    if dw == 0 || dh == 0 {
        return;
    }

    // the visible part of the page, in page UV, mapped to a stored-pixel rectangle
    let u = ((x0 as f64 - d.x) / d.width, (x1 as f64 - d.x) / d.width);
    let v = ((y0 as f64 - d.y) / d.height, (y1 as f64 - d.y) / d.height);
    let m = page.uv;
    let map = |u: f64, v: f64| (m[0][0] * u + m[0][1] * v + m[0][2], m[1][0] * u + m[1][1] * v + m[1][2]);
    let (a, b) = (map(u.0, v.0), map(u.1, v.1));
    let (sw, sh) = page.picture.size();
    let (sw_f, sh_f) = (sw as f64, sh as f64);
    let s0 = (a.0.min(b.0) * sw_f).clamp(0.0, sw_f);
    let s1 = (a.0.max(b.0) * sw_f).clamp(s0, sw_f);
    let t0 = (a.1.min(b.1) * sh_f).clamp(0.0, sh_f);
    let t1 = (a.1.max(b.1) * sh_f).clamp(t0, sh_f);
    if s1 - s0 <= 0.0 || t1 - t0 <= 0.0 {
        return;
    }

    // with a 90° turn, screen columns run along stored rows
    let swapped = m[0][0].abs() < m[0][1].abs();
    let (ow, oh) = if swapped { (dh, dw) } else { (dw, dh) };
    let scale = (ow as f64 / (s1 - s0)).max(oh as f64 / (t1 - t0));
    let alg = if scale >= style.nearest_above_zoom {
        ResizeAlg::Nearest
    } else {
        ResizeAlg::Convolution(FilterType::Bilinear)
    };
    let src = ImageRef::new(sw, sh, page.picture.image.data(), PixelType::U8x4).expect("RGBA8 buffer matches its size");
    let mut out = Image::new(ow as u32, oh as u32, PixelType::U8x4);
    let opts = ResizeOptions::new().resize_alg(alg).crop(s0, t0, s1 - s0, t1 - t0).use_alpha(page.picture.has_alpha);
    Resizer::new().resize(&src, &mut out, &opts).expect("same pixel type on both sides");
    let out = out.buffer();

    // stored-region index of screen pixel (x0 + i, y0 + j) is base(j) + i * step
    let (ow_i, oh_i) = (ow as isize, oh as isize);
    let (a_pos, b_pos, d_pos, e_pos) = (m[0][0] > 0.0, m[0][1] > 0.0, m[1][0] > 0.0, m[1][1] > 0.0);
    let row_start = move |j: isize| -> (isize, isize) {
        if swapped {
            let si = if b_pos { j } else { ow_i - 1 - j };
            if d_pos { (si, ow_i) } else { ((oh_i - 1) * ow_i + si, -ow_i) }
        } else {
            let sj = if e_pos { j } else { oh_i - 1 - j };
            if a_pos { (sj * ow_i, 1) } else { (sj * ow_i + ow_i - 1, -1) }
        }
    };

    let has_alpha = page.picture.has_alpha;
    let adjusts = page.adjusts;
    let dim = selection.filter(|s| s.width > 0.0 && s.height > 0.0).map(|s| (s, dim_table()));
    let width = frame.width as usize;
    frame.pixels[y0 * width..y1 * width].par_chunks_mut(width).enumerate().for_each(|(j, row)| {
        let y = y0 + j;
        let (base, step) = row_start(j as isize);
        for i in 0..dw {
            let x = x0 + i;
            let k = (base + i as isize * step) as usize * 4;
            let px = &out[k..k + 4];
            let mut rgb = [px[0], px[1], px[2]];
            if !adjusts.is_empty() {
                // the same arithmetic as `iv_ops::apply::adjust`, so the preview is the result
                let c = adjusts.iter().fold(rgb.map(|c| c as f32 / 255.0), |c, a| a.apply(c));
                rgb = c.map(|c| (c * 255.0).round() as u8);
            }
            if has_alpha && px[3] < 255 {
                let bg = CHECKER[(x / CHECKER_CELL + y / CHECKER_CELL) & 1];
                let a = px[3] as u32;
                rgb = std::array::from_fn(|c| ((rgb[c] as u32 * a + bg[c] as u32 * (255 - a) + 127) / 255) as u8);
            }
            if let Some((s, table)) = dim {
                let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
                if !(cx >= s.x && cx < s.x + s.width && cy >= s.y && cy < s.y + s.height) {
                    rgb = rgb.map(|c| table[c as usize]);
                }
            }
            row[x] = pack(rgb);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use iv_core::Orientation;
    use iv_ops::{Geometry, Op, Rect};

    /// w×h RGBA8 image whose pixel (x, y) is [x·10, y·10, 77, 255].
    fn coords(w: u32, h: u32) -> ImageBuf {
        let data = (0..h).flat_map(|y| (0..w).flat_map(move |x| [x as u8 * 10, y as u8 * 10, 77, 255])).collect();
        ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap()
    }

    fn rgb_at(img: &ImageBuf, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * img.width() + x) * 4) as usize;
        [img.data()[i], img.data()[i + 1], img.data()[i + 2]]
    }

    /// Draws `img` with `g` at 100 % at (ox, oy) in a frame of `frame` size.
    fn draw_at_100(img: &ImageBuf, g: &Geometry, frame: (u32, u32), origin: (f64, f64)) -> Frame {
        let pic = Picture::new(img, false);
        let mut f = Frame::new(frame.0, frame.1);
        let dest = ScreenRect { x: origin.0, y: origin.1, width: g.size.0 as f64, height: g.size.1 as f64 };
        let page = Page { picture: &pic, uv: g.display_to_stored_uv(), adjusts: &g.adjusts, dest };
        render(&mut f, &[page], None, &Style::default());
        f
    }

    /// At 100 %, the preview must be pixel-for-pixel what saving produces.
    fn assert_matches_saved(img: &ImageBuf, ops: &[Op], exif: Orientation) {
        let g = Geometry::new((img.width(), img.height()), exif, ops);
        let saved = iv_ops::apply::render(img, &g).unwrap();
        let f = draw_at_100(img, &g, g.size, (0.0, 0.0));
        for y in 0..g.size.1 {
            for x in 0..g.size.0 {
                assert_eq!(f.rgb(x, y), rgb_at(&saved, x, y), "({x}, {y}) with {ops:?} / {exif:?}");
            }
        }
    }

    #[test]
    fn every_orientation_matches_the_saved_pixels() {
        let img = coords(5, 3);
        for o in Orientation::ALL {
            assert_matches_saved(&img, &[], o);
            assert_matches_saved(&img, &[Op::Orient(o)], Orientation::Normal);
        }
    }

    #[test]
    fn crops_and_turns_match_the_saved_pixels() {
        let img = coords(9, 7);
        let crop = Op::Crop(Rect { x: 2, y: 1, width: 4, height: 5 });
        assert_matches_saved(&img, &[crop], Orientation::Normal);
        assert_matches_saved(&img, &[Op::rotate_cw(), crop, Op::flip_h()], Orientation::Rotate90);
        assert_matches_saved(
            &img,
            &[crop, Op::rotate_ccw(), Op::Crop(Rect { x: 1, y: 1, width: 2, height: 2 })],
            Orientation::Normal,
        );
    }

    #[test]
    fn adjustments_match_the_saved_pixels() {
        let img = coords(6, 4);
        let adjust = Op::Adjust(Adjust { brightness: 0.1, contrast: 0.3, gamma: 1.7, saturation: -0.4 });
        assert_matches_saved(&img, &[adjust], Orientation::Normal);
        assert_matches_saved(
            &img,
            &[Op::rotate_cw(), adjust, Op::Adjust(Adjust { saturation: 0.8, ..Default::default() })],
            Orientation::Normal,
        );
    }

    #[test]
    fn a_page_partly_off_screen_shows_the_matching_part() {
        let img = coords(20, 10);
        let g = Geometry::new((20, 10), Orientation::Normal, &[]);
        let f = draw_at_100(&img, &g, (8, 4), (-7.0, -3.0));
        for y in 0..4 {
            for x in 0..8 {
                assert_eq!(f.rgb(x, y), rgb_at(&img, x + 7, y + 3), "({x}, {y})");
            }
        }
    }

    #[test]
    fn background_fills_around_the_image_and_flat_images_stay_flat_when_shrunk() {
        let img = ImageBuf::new(400, 200, PixelFormat::Rgba8, [200, 100, 50, 255].repeat(400 * 200)).unwrap();
        let pic = Picture::new(&img, false);
        let mut f = Frame::new(100, 100);
        let page = Page {
            picture: &pic,
            uv: Geometry::new((400, 200), Orientation::Normal, &[]).display_to_stored_uv(),
            adjusts: &[],
            dest: ScreenRect { x: 0.0, y: 25.0, width: 100.0, height: 50.0 },
        };
        let style = Style { background: [1, 2, 3], ..Style::default() };
        render(&mut f, &[page], None, &style);
        assert_eq!(f.rgb(50, 10), [1, 2, 3]);
        assert_eq!(f.rgb(50, 90), [1, 2, 3]);
        for (x, y) in [(0, 25), (50, 50), (99, 74)] {
            assert_eq!(f.rgb(x, y), [200, 100, 50], "({x}, {y})");
        }
    }

    #[test]
    fn transparency_shows_the_checkerboard_and_selection_dims_outside() {
        // left half transparent, right half opaque white
        let data = (0..4)
            .flat_map(|_| (0..4).flat_map(|x| if x < 2 { [255, 0, 0, 0] } else { [255, 255, 255, 255] }))
            .collect();
        let img = ImageBuf::new(4, 4, PixelFormat::Rgba8, data).unwrap();
        let pic = Picture::new(&img, true);
        let mut f = Frame::new(64, 64);
        let page = Page {
            picture: &pic,
            uv: Geometry::new((4, 4), Orientation::Normal, &[]).display_to_stored_uv(),
            adjusts: &[],
            dest: ScreenRect { x: 0.0, y: 0.0, width: 64.0, height: 64.0 },
        };
        let sel = ScreenRect { x: 48.0, y: 0.0, width: 16.0, height: 16.0 };
        render(&mut f, &[page], Some(sel), &Style { nearest_above_zoom: 2.0, ..Style::default() });
        let dim = |c: [u8; 3]| c.map(|v| dim_table()[v as usize]);
        assert_eq!(f.rgb(1, 1), dim(CHECKER[0]), "transparent: checker, dimmed outside the selection");
        assert_eq!(f.rgb(9, 1), dim(CHECKER[1]), "next checker cell");
        assert_eq!(f.rgb(50, 5), [255, 255, 255], "inside the selection: untouched");
        assert_eq!(f.rgb(50, 40), dim([255, 255, 255]), "opaque, outside the selection");
        assert!(dim_table()[255] < 170 && dim_table()[0] == 0);
    }

    #[test]
    fn high_zoom_draws_sharp_pixels() {
        let img = coords(2, 1);
        let pic = Picture::new(&img, false);
        let mut f = Frame::new(40, 20);
        let page = Page {
            picture: &pic,
            uv: Geometry::new((2, 1), Orientation::Normal, &[]).display_to_stored_uv(),
            adjusts: &[],
            dest: ScreenRect { x: 0.0, y: 0.0, width: 40.0, height: 20.0 },
        };
        render(&mut f, &[page], None, &Style::default());
        assert_eq!(f.rgb(19, 10), rgb_at(&img, 0, 0));
        assert_eq!(f.rgb(20, 10), rgb_at(&img, 1, 0));
    }

    #[test]
    fn sixteen_bit_images_are_drawn() {
        let px: Vec<u16> = [65535u16, 0, 32896, 65535].repeat(4);
        let img = ImageBuf::new(2, 2, PixelFormat::Rgba16, bytemuck::cast_slice(&px).to_vec()).unwrap();
        let g = Geometry::new((2, 2), Orientation::Normal, &[]);
        let f = draw_at_100(&img, &g, (2, 2), (0.0, 0.0));
        assert_eq!(f.rgb(1, 1), [255, 0, 128]);
    }
}
