//! CPU renderer: draws pages of an image into a BGRA frame that GDI can blit.
//!
//! Only the part of a page that is on screen is touched: that region of the stored pixels (plus
//! the few pixels the filter reaches) is cut out with a sub-pixel crop and resampled straight to
//! its on-screen size (SIMD, multi-threaded `fast_image_resize`), then written to the frame
//! through the page's orientation (a D4 transform, so it is an index remap), with tone
//! adjustments (a lookup table), the transparency checkerboard and the selection dimming applied
//! per pixel. The image itself is never copied, rotated or converted as a whole, so drawing
//! costs memory and time on the order of the window size, not the image size:
//!
//! - An image shown at half its size or less is resampled from a box-filtered copy at 1/2,
//!   1/4 … of its size (a mip level), so fitting a 50-megapixel photo reads a few megapixels,
//!   not fifty. A picture keeps only the level it is being drawn from (a quarter of its memory
//!   at most, 1/16 when shown at a quarter of its size or less, none above half its size) and
//!   builds it on first use.
//! - Alpha is premultiplied only over the region being resampled.
//! - While the frame is told an overlay may change ([`Frame::keep_resampled`]: a selection is
//!   being made, the edit panel is open), the resampled region is kept, and a redraw that only
//!   changes what is applied after resampling (selection, tone sliders, background) reuses it.
//!   Otherwise it and the resampler's scratch are freed after each redraw.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use fast_image_resize::images::{CroppedImage, Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use iv_core::{ImageBuf, PixelFormat};
use iv_ops::{Adjust, ToneTable};
use rayon::prelude::*;

use crate::view::ScreenRect;

/// Checkerboard behind transparent pixels: cell size and the two greys (sRGB).
const CHECKER_CELL: usize = 8;
const CHECKER: [[u8; 3]; 2] = [[118, 118, 118], [149, 149, 149]];
/// Outside a selection, light is scaled by this factor (in linear light).
const SELECTION_DIM: f64 = 0.35;
/// Smallest mip level is 1/2^MAX_LEVEL of the picture.
const MAX_LEVEL: u32 = 8;

/// Window-sized pixels, one `0x00RRGGBB` word per pixel (a top-down 32-bit DIB), and, while
/// asked to, what the last redraw resampled, for the next one to reuse.
#[derive(Default)]
pub struct Frame {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
    resizer: Resizer,
    /// Keep resampled regions (and the resampler's scratch) between redraws.
    keep: bool,
    /// Resampled regions of the pages drawn last, at most one per page.
    resampled: Vec<Resampled>,
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

    /// Whether the next redraws are likely to change only what is drawn after resampling (a
    /// selection being dragged, tone sliders, settings): then the resampled regions (up to the
    /// window's size in memory) are kept for them to reuse. Off, they are freed after each
    /// redraw, so an idle viewer holds no more than the frame.
    pub fn keep_resampled(&mut self, keep: bool) {
        self.keep = keep;
        if !keep {
            self.release();
        }
    }

    fn release(&mut self) {
        self.resampled = Vec::new();
        self.resizer.reset_internal_buffers();
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
/// RGBA8, which is what the pipeline delivers).
#[derive(Clone)]
pub struct Picture {
    image: ImageBuf,
    has_alpha: bool,
    /// Tells pictures apart for the resampling cache; clones share it (and the pixels).
    id: u64,
    /// The mip level last drawn from, `(k, the picture box-filtered to 1/2^k of its size)`,
    /// shared by clones.
    level: Arc<Mutex<Option<(u32, ImageBuf)>>>,
}

impl Picture {
    pub fn new(img: &ImageBuf, has_alpha: bool) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let image = match img.format() {
            PixelFormat::Rgba8 => img.clone(),
            _ => ImageBuf::new(img.width(), img.height(), PixelFormat::Rgba8, img.to_rgba8().into_owned())
                .expect("RGBA8 conversion keeps the dimensions"),
        };
        Self { image, has_alpha, id: NEXT_ID.fetch_add(1, Ordering::Relaxed), level: Default::default() }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.image.width(), self.image.height())
    }

    /// The picture at 1/2^k of its size (k = 0: the picture itself). Built from the level kept
    /// when that one is larger, else from the picture; then kept instead of it. Drawing the
    /// picture itself frees the kept level.
    fn level(&self, k: u32) -> ImageBuf {
        let mut kept = self.level.lock().unwrap_or_else(|e| e.into_inner());
        if k == 0 {
            *kept = None;
            return self.image.clone();
        }
        let (mut at, mut img) = match kept.take() {
            Some((at, img)) if at <= k => (at, img),
            _ => (0, self.image.clone()),
        };
        while at < k {
            img = half(&img, self.has_alpha);
            at += 1;
        }
        *kept = Some((k, img.clone()));
        img
    }
}

/// `img` box-filtered to half its size (rounded up; an odd last row or column averages fewer
/// pixels). With alpha, colours are weighted by it, so transparent pixels do not darken edges.
fn half(img: &ImageBuf, alpha: bool) -> ImageBuf {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (hw, hh) = (w.div_ceil(2), h.div_ceil(2));
    let src = img.data();
    let mut out = vec![0u8; hw * hh * 4];
    out.par_chunks_mut(hw * 4).enumerate().for_each(|(y, row)| {
        let rows = [2 * y, (2 * y + 1).min(h - 1)];
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let cols = [2 * x, (2 * x + 1).min(w - 1)];
            let mut sum = [0u32; 4];
            let mut weight = 0u32;
            for sy in rows {
                for sx in cols {
                    let p = &src[(sy * w + sx) * 4..][..4];
                    let a = if alpha { p[3] as u32 } else { 1 };
                    for c in 0..3 {
                        sum[c] += p[c] as u32 * a;
                    }
                    sum[3] += p[3] as u32;
                    weight += a;
                }
            }
            let rgb = |c: usize| sum[c].checked_add(weight / 2).and_then(|s| s.checked_div(weight)).unwrap_or(0) as u8;
            px.copy_from_slice(&[rgb(0), rgb(1), rgb(2), ((sum[3] + 2) / 4) as u8]);
        }
    });
    ImageBuf::new(hw as u32, hh as u32, PixelFormat::Rgba8, out).expect("size matches")
}

/// Which mip level to resample from at `scale` screen pixels per stored pixel: the smallest one
/// that still has at least as many pixels as the screen region (so it is only ever shrunk).
fn level_for(scale: f64, (w, h): (u32, u32)) -> u32 {
    let mut k = 0;
    while k < MAX_LEVEL && scale * f64::from(1u32 << (k + 1)) <= 1.0 && (w >> (k + 1)) >= 1 && (h >> (k + 1)) >= 1 {
        k += 1;
    }
    k
}

/// The integer span of a source of `size` pixels that resampling `[c0, c1)` to `out` pixels
/// reads: the region plus the bilinear filter's reach (one source pixel, widened by the
/// reduction factor), clipped to the source. Returns (start, length).
fn source_window(c0: f64, c1: f64, out: usize, size: u32) -> (u32, u32) {
    let reach = ((c1 - c0) / out as f64).max(1.0).ceil() as i64 + 1;
    let lo = (c0.floor() as i64 - reach).clamp(0, size as i64 - 1);
    let hi = (c1.ceil() as i64 + reach).clamp(lo + 1, size as i64);
    (lo as u32, (hi - lo) as u32)
}

/// `v`, or the whole number it is within rounding noise of (view arithmetic lands 100 % views
/// on whole pixels up to such noise; an exact crop is copied instead of interpolated).
fn snap(v: f64) -> f64 {
    let r = v.round();
    if (v - r).abs() < 1e-6 { r } else { v }
}

/// One page's resampled region and what it was resampled from.
struct Resampled {
    key: ResampleKey,
    /// `size.0 × size.1` RGBA8 pixels.
    pixels: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq)]
struct ResampleKey {
    picture: u64,
    level: u32,
    /// Crop in the level's pixels, as bits (compared exactly).
    crop: [u64; 4],
    size: (usize, usize),
    nearest: bool,
    alpha: bool,
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
    let mut previous = std::mem::take(&mut frame.resampled);
    for page in pages {
        if let Some(r) = draw_page(frame, &mut previous, page, selection, style) {
            frame.resampled.push(r);
        }
    }
    if !frame.keep {
        frame.release();
    }
}

/// Integer pixel span whose centres fall inside `[start, start + len)`, clipped to `0..limit`.
fn pixel_span(start: f64, len: f64, limit: u32) -> (usize, usize) {
    let lo = (start - 0.5).ceil().clamp(0.0, limit as f64) as usize;
    let hi = (start + len - 0.5).ceil().clamp(0.0, limit as f64) as usize;
    (lo, hi.max(lo))
}

/// Draws one page; returns its resampled region for the next redraw.
fn draw_page(
    frame: &mut Frame,
    previous: &mut Vec<Resampled>,
    page: &Page<'_>,
    selection: Option<ScreenRect>,
    style: &Style,
) -> Option<Resampled> {
    let d = page.dest;
    if !(d.width > 0.0 && d.height > 0.0) {
        return None;
    }
    let (x0, x1) = pixel_span(d.x, d.width, frame.width);
    let (y0, y1) = pixel_span(d.y, d.height, frame.height);
    let (dw, dh) = (x1 - x0, y1 - y0);
    if dw == 0 || dh == 0 {
        return None;
    }

    // the visible part of the page, in page UV, mapped to a stored-pixel rectangle
    let u = ((x0 as f64 - d.x) / d.width, (x1 as f64 - d.x) / d.width);
    let v = ((y0 as f64 - d.y) / d.height, (y1 as f64 - d.y) / d.height);
    let m = page.uv;
    let map = |u: f64, v: f64| (m[0][0] * u + m[0][1] * v + m[0][2], m[1][0] * u + m[1][1] * v + m[1][2]);
    let (a, b) = (map(u.0, v.0), map(u.1, v.1));
    let (sw, sh) = page.picture.size();
    let (sw_f, sh_f) = (sw as f64, sh as f64);
    let s0 = snap(a.0.min(b.0) * sw_f).clamp(0.0, sw_f);
    let s1 = snap(a.0.max(b.0) * sw_f).clamp(s0, sw_f);
    let t0 = snap(a.1.min(b.1) * sh_f).clamp(0.0, sh_f);
    let t1 = snap(a.1.max(b.1) * sh_f).clamp(t0, sh_f);
    if s1 - s0 <= 0.0 || t1 - t0 <= 0.0 {
        return None;
    }

    // with a 90° turn, screen columns run along stored rows
    let swapped = m[0][0].abs() < m[0][1].abs();
    let (ow, oh) = if swapped { (dh, dw) } else { (dw, dh) };
    let scale = (ow as f64 / (s1 - s0)).max(oh as f64 / (t1 - t0));
    let nearest = scale >= style.nearest_above_zoom;
    let level = if nearest { 0 } else { level_for(scale, (sw, sh)) };
    let div = f64::from(1u32 << level);
    let crop = [s0 / div, t0 / div, s1 / div, t1 / div];
    let key = ResampleKey {
        picture: page.picture.id,
        level,
        crop: crop.map(f64::to_bits),
        size: (ow, oh),
        nearest,
        alpha: page.picture.has_alpha,
    };

    let resampled = match previous.iter().position(|r| r.key == key) {
        // only what is applied after resampling changed (selection, tones, background)
        Some(i) => previous.swap_remove(i),
        None => {
            // reuse an old region's allocation for the new one
            let mut pixels = match previous.pop() {
                Some(r) => r.pixels,
                // zeroed by the OS as pages are touched, not written twice
                None => vec![0u8; ow * oh * 4],
            };
            pixels.resize(ow * oh * 4, 0);
            resample(&mut frame.resizer, &page.picture.level(level), crop, (ow, oh), nearest, key.alpha, &mut pixels);
            // scratch larger than the region it made (a large enlargement, a wide reduction) is
            // not kept for the next redraws
            if frame.resizer.size_of_internal_buffers() > 2 * pixels.len() {
                frame.resizer.reset_internal_buffers();
            }
            Resampled { key, pixels }
        }
    };
    let out = &resampled.pixels;

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
    // the same table saving maps pixels through, so the preview is the result
    let tones = ToneTable::new(page.adjusts, 256);
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
            if let Some(t) = &tones {
                rgb = t.map8(rgb);
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
    Some(resampled)
}

/// Resamples `[x0, y0, x1, y1]` of `src` to `size` into `out` (RGBA8), reading only the part of
/// `src` the filter reaches: premultiplying alpha and the filter's scratch buffers then cost the
/// visible region, not the whole image.
fn resample(
    resizer: &mut Resizer,
    src: &ImageBuf,
    [x0, y0, x1, y1]: [f64; 4],
    (ow, oh): (usize, usize),
    nearest: bool,
    alpha: bool,
    out: &mut [u8],
) {
    let (wx, ww) = source_window(x0, x1, ow, src.width());
    let (wy, wh) = source_window(y0, y1, oh, src.height());
    let full =
        ImageRef::new(src.width(), src.height(), src.data(), PixelType::U8x4).expect("RGBA8 buffer matches its size");
    let window = CroppedImage::new(&full, wx, wy, ww, wh).expect("window inside the image");
    let mut dst = Image::from_slice_u8(ow as u32, oh as u32, out, PixelType::U8x4).expect("buffer of the output size");
    let alg = if nearest { ResizeAlg::Nearest } else { ResizeAlg::Convolution(FilterType::Bilinear) };
    let opts =
        ResizeOptions::new().resize_alg(alg).crop(x0 - wx as f64, y0 - wy as f64, x1 - x0, y1 - y0).use_alpha(alpha);
    resizer.resize(&window, &mut dst, &opts).expect("same pixel type on both sides");
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
        let tone_only = Op::Adjust(Adjust { brightness: -0.1, contrast: 0.4, gamma: 0.8, saturation: 0.0 });
        assert_matches_saved(&img, &[tone_only, Op::flip_v()], Orientation::Normal);
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

    /// A translucent w×h image with a different colour at every pixel.
    fn noisy_alpha(w: u32, h: u32) -> ImageBuf {
        let mut seed = 0x9e37_79b9_u32;
        let data = (0..w * h * 4)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap()
    }

    /// What `fast_image_resize` gives for the crop over the whole image (the old way: the
    /// whole source premultiplied).
    fn resample_whole(img: &ImageBuf, crop: [f64; 4], size: (usize, usize), nearest: bool) -> Vec<u8> {
        let src = ImageRef::new(img.width(), img.height(), img.data(), PixelType::U8x4).unwrap();
        let mut out = Image::new(size.0 as u32, size.1 as u32, PixelType::U8x4);
        let alg = if nearest { ResizeAlg::Nearest } else { ResizeAlg::Convolution(FilterType::Bilinear) };
        let opts = ResizeOptions::new()
            .resize_alg(alg)
            .crop(crop[0], crop[1], crop[2] - crop[0], crop[3] - crop[1])
            .use_alpha(true);
        Resizer::new().resize(&src, &mut out, &opts).unwrap();
        out.into_vec()
    }

    #[test]
    fn resampling_a_window_gives_what_resampling_the_whole_image_gives() {
        let img = noisy_alpha(97, 61);
        let mut resizer = Resizer::new();
        let cases: &[([f64; 4], (usize, usize), bool)] = &[
            ([10.3, 7.6, 30.3, 27.6], (40, 40), false),    // 2x, fractional offset
            ([0.0, 0.0, 97.0, 61.0], (40, 25), false),     // shrunk whole
            ([41.25, 11.5, 83.75, 52.0], (17, 16), false), // shrunk part
            ([60.5, 40.5, 96.9, 60.9], (100, 56), false),  // enlarged at the far edge
            ([3.0, 4.0, 13.0, 9.0], (10, 5), false),       // 1:1, whole pixels: a copy
            ([5.1, 5.1, 9.1, 8.1], (32, 24), true),        // sharp pixels
        ];
        for &(crop, size, nearest) in cases {
            let mut out = vec![0u8; size.0 * size.1 * 4];
            resample(&mut resizer, &img, crop, size, nearest, true, &mut out);
            assert!(out == resample_whole(&img, crop, size, nearest), "{crop:?} → {size:?}");
        }
    }

    #[test]
    fn zooming_into_a_transparent_image_premultiplies_only_the_visible_part() {
        let img = ImageBuf::new(1500, 1000, PixelFormat::Rgba8, [9, 8, 7, 100].repeat(1500 * 1000)).unwrap();
        let mut resizer = Resizer::new();
        let mut out = vec![0u8; 80 * 60 * 4];
        resample(&mut resizer, &img, [700.5, 400.5, 740.5, 430.5], (80, 60), false, true, &mut out);
        let scratch = resizer.size_of_internal_buffers();
        assert!(scratch < 64 * 1024, "{scratch} bytes of scratch for a 40x30 region of a 6 MB image");
        assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 100));
    }

    #[test]
    fn source_windows_cover_the_filter_and_stay_inside() {
        // 2x enlargement: one pixel of reach on each side
        assert_eq!(source_window(10.3, 30.3, 40, 100), (8, 25));
        // 4x reduction: the reach grows with it
        assert_eq!(source_window(40.0, 80.0, 10, 100), (35, 50));
        // clipped at both ends
        assert_eq!(source_window(0.0, 100.0, 50, 100), (0, 100));
        assert_eq!(source_window(99.5, 100.0, 3, 100), (97, 3));
    }

    #[test]
    fn levels_are_picked_so_they_are_only_ever_shrunk() {
        let big = (8000, 6000);
        assert_eq!(level_for(1.0, big), 0);
        assert_eq!(level_for(0.6, big), 0);
        assert_eq!(level_for(0.5, big), 1);
        assert_eq!(level_for(0.24, big), 2);
        assert_eq!(level_for(0.01, (40, 30)), 4, "not past one pixel");
    }

    #[test]
    fn half_size_levels_average_and_respect_alpha() {
        // 3x1: an odd column at the end
        let data = vec![10, 20, 30, 255, 30, 40, 50, 255, 200, 0, 0, 255];
        let img = ImageBuf::new(3, 1, PixelFormat::Rgba8, data).unwrap();
        let h = half(&img, false);
        assert_eq!((h.width(), h.height()), (2, 1));
        assert_eq!(&h.data()[..4], &[20, 30, 40, 255]);
        assert_eq!(&h.data()[4..], &[200, 0, 0, 255]);
        // a transparent pixel's colour does not bleed in
        let data = vec![255, 255, 255, 255, 0, 0, 0, 0];
        let h = half(&ImageBuf::new(2, 1, PixelFormat::Rgba8, data).unwrap(), true);
        assert_eq!(h.data(), &[255, 255, 255, 128]);
    }

    #[test]
    fn a_large_image_shrunk_from_a_level_looks_like_shrinking_it_directly() {
        let img = coords(24, 16); // smooth gradients
        let pic = Picture::new(&img, false);
        let g = Geometry::new((24, 16), Orientation::Normal, &[]);
        let mut f = Frame::new(6, 4);
        let page = Page {
            picture: &pic,
            uv: g.display_to_stored_uv(),
            adjusts: &[],
            dest: ScreenRect { x: 0.0, y: 0.0, width: 6.0, height: 4.0 },
        };
        render(&mut f, &[page], None, &Style::default());
        assert_eq!(pic.level.lock().unwrap().as_ref().map(|l| l.0), Some(2), "drawn from the quarter-size level");
        let direct = resample_whole(&img, [0.0, 0.0, 24.0, 16.0], (6, 4), false);
        for y in 0..4 {
            for x in 0..6 {
                let i = (y * 6 + x) * 4;
                let got = f.rgb(x as u32, y as u32);
                // (the gradient climbs 40 per screen pixel) a box and a triangle filter only part
                // at the image edge, where the triangle reaches further in
                let edge = x == 0 || y == 0 || x == 5 || y == 3;
                let tolerance = if edge { 5 } else { 1 };
                for c in 0..3 {
                    assert!(
                        (got[c] as i32 - direct[i + c] as i32).abs() <= tolerance,
                        "({x}, {y}): {got:?} vs {:?}",
                        &direct[i..i + 3]
                    );
                }
            }
        }
        // only the level in use is kept: drawn at full size, none
        let page = Page {
            picture: &pic,
            uv: g.display_to_stored_uv(),
            adjusts: &[],
            dest: ScreenRect { x: 0.0, y: 0.0, width: 24.0, height: 16.0 },
        };
        render(&mut Frame::new(24, 16), &[page], None, &Style::default());
        assert!(pic.level.lock().unwrap().is_none());
    }

    #[test]
    fn redraws_that_change_only_the_overlay_reuse_the_resampled_region() {
        let img = noisy_alpha(50, 40);
        let pic = Picture::new(&img, true);
        let g = Geometry::new((50, 40), Orientation::Normal, &[]);
        let mut f = Frame::new(30, 30);
        let draw = |f: &mut Frame, sel: Option<ScreenRect>, adjusts: &[Adjust]| {
            let dest = ScreenRect { x: 0.5, y: 2.0, width: 25.0, height: 20.0 };
            let page = Page { picture: &pic, uv: g.display_to_stored_uv(), adjusts, dest };
            render(f, &[page], sel, &Style::default());
        };
        draw(&mut f, None, &[]);
        assert!(f.resampled.is_empty(), "not kept unless asked: an idle viewer holds only the frame");
        f.keep_resampled(true);
        draw(&mut f, None, &[]);
        let first = f.resampled[0].pixels.as_ptr();
        let sel = ScreenRect { x: 3.0, y: 3.0, width: 9.0, height: 9.0 };
        draw(&mut f, Some(sel), &[Adjust { brightness: 0.2, ..Default::default() }]);
        assert_eq!(f.resampled.len(), 1);
        assert_eq!(f.resampled[0].pixels.as_ptr(), first, "not resampled again");
        // and the overlay result is what a fresh frame draws
        let mut fresh = Frame::new(30, 30);
        draw(&mut fresh, Some(sel), &[Adjust { brightness: 0.2, ..Default::default() }]);
        assert_eq!(f.pixels(), fresh.pixels());
        // another picture is resampled (into the same allocation)
        let other = Picture::new(&img, true);
        let page = Page {
            picture: &other,
            uv: g.display_to_stored_uv(),
            adjusts: &[],
            dest: ScreenRect { x: 0.5, y: 2.0, width: 25.0, height: 20.0 },
        };
        render(&mut f, &[page], None, &Style::default());
        assert!(f.resampled[0].key.picture == other.id);
        f.keep_resampled(false);
        assert!(f.resampled.is_empty() && f.resizer.size_of_internal_buffers() == 0, "freed when no longer useful");
    }
}
