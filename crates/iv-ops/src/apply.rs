//! Applies a [`Geometry`] to pixels: crop, orient, resize, adjust.
//!
//! Every step works on whole rows in parallel and keeps at most two full images alive at once
//! (the one it reads and the one it writes); adjusting reuses the buffer when it is not shared.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use iv_core::{Error, ImageBuf, Orientation, PixelFormat, Result};
use rayon::prelude::*;

use crate::geometry::Geometry;
use crate::op::{Adjust, Rect, ToneTable};

fn crop(img: &ImageBuf, r: Rect) -> Result<ImageBuf> {
    if r == (Rect { x: 0, y: 0, width: img.width(), height: img.height() }) {
        return Ok(img.clone());
    }
    let bpp = img.format().bytes_per_pixel();
    let row = r.width as usize * bpp;
    let mut out = Vec::with_capacity(row * r.height as usize);
    for y in r.y..r.y + r.height {
        let start = y as usize * img.stride() + r.x as usize * bpp;
        out.extend_from_slice(&img.data()[start..start + row]);
    }
    ImageBuf::new(r.width, r.height, img.format(), out)
}

/// Where output row `y` of `img` shown with `o` comes from: the stored index of the row's first
/// pixel and the index step to the next pixel, for a stored `w` × `h` image. Orientations are D4
/// transforms (`display_to_stored_uv` has entries 0 and ±1), so this is exact integer
/// arithmetic.
fn row_source(o: Orientation, (w, h): (usize, usize), y: usize) -> (isize, isize) {
    let m = o.display_to_stored_uv();
    // a stored coordinate is the output x or the output y, possibly mirrored:
    // (its value at the row's first pixel, its change per pixel)
    let pick = |along_x: f64, along_y: f64, n: usize| -> (isize, isize) {
        if along_x > 0.5 {
            (0, 1)
        } else if along_x < -0.5 {
            (n as isize - 1, -1)
        } else if along_y > 0.5 {
            (y as isize, 0)
        } else {
            (n as isize - 1 - y as isize, 0)
        }
    };
    let (sx0, sx_step) = pick(m[0][0], m[0][1], w);
    let (sy0, sy_step) = pick(m[1][0], m[1][1], h);
    (sy0 * w as isize + sx0, sy_step * w as isize + sx_step)
}

/// Bakes `o` into the pixels: the result, shown without transform, looks like `img` shown with `o`.
pub fn orient(img: &ImageBuf, o: Orientation) -> Result<ImageBuf> {
    if o == Orientation::Normal {
        return Ok(img.clone());
    }
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (ow, oh) = if o.swaps_dimensions() { (h, w) } else { (w, h) };
    let bpp = img.format().bytes_per_pixel();
    let src = img.data();
    let mut out = vec![0u8; ow * oh * bpp];
    out.par_chunks_mut(ow * bpp).enumerate().for_each(|(y, row)| {
        let (base, step) = row_source(o, (w, h), y);
        if step == 1 {
            let s = base as usize * bpp;
            row.copy_from_slice(&src[s..s + row.len()]);
            return;
        }
        for (x, px) in row.chunks_exact_mut(bpp).enumerate() {
            let s = (base + x as isize * step) as usize * bpp;
            px.copy_from_slice(&src[s..s + bpp]);
        }
    });
    ImageBuf::new(ow as u32, oh as u32, img.format(), out)
}

/// High-quality (Lanczos3, alpha-aware) resampling.
pub fn resize(img: &ImageBuf, width: u32, height: u32) -> Result<ImageBuf> {
    if (width, height) == (img.width(), img.height()) {
        return Ok(img.clone());
    }
    ImageBuf::byte_len(width, height, img.format())?;
    let pixel_type = match img.format() {
        PixelFormat::Rgba8 => PixelType::U8x4,
        PixelFormat::Rgba16 => PixelType::U16x4,
        PixelFormat::RgbaF32 => PixelType::F32x4,
    };
    let src =
        ImageRef::new(img.width(), img.height(), img.data(), pixel_type).map_err(|e| Error::decode("resize", e))?;
    let mut dst = Image::new(width, height, pixel_type);
    Resizer::new()
        .resize(&src, &mut dst, &ResizeOptions::new().use_alpha(true))
        .map_err(|e| Error::decode("resize", e))?;
    ImageBuf::new(width, height, img.format(), dst.into_vec())
}

/// Pixels per parallel work item when mapping tones.
const ROWS_OF: usize = 16 * 1024;

/// Maps `src` pixels to `out` (both RGBA samples of one type) through `map`, in parallel.
fn map_pixels<T: Copy + Send + Sync>(src: &[T], out: &mut [T], map: impl Fn([T; 3]) -> [T; 3] + Sync) {
    out.par_chunks_mut(ROWS_OF * 4).zip(src.par_chunks(ROWS_OF * 4)).for_each(|(o, s)| {
        for (o, s) in o.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
            let [r, g, b] = map([s[0], s[1], s[2]]);
            o.copy_from_slice(&[r, g, b, s[3]]);
        }
    });
}

/// Applies tone adjustments in order to every pixel.
pub fn adjust(img: &ImageBuf, adjusts: &[Adjust]) -> Result<ImageBuf> {
    adjust_owned(img.clone(), adjusts)
}

/// [`adjust`], writing over `img`'s own buffer when nothing else shares it.
fn adjust_owned(img: ImageBuf, adjusts: &[Adjust]) -> Result<ImageBuf> {
    if adjusts.is_empty() {
        return Ok(img);
    }
    let (w, h, f) = (img.width(), img.height(), img.format());
    match f {
        PixelFormat::Rgba8 => {
            let table = ToneTable::new(adjusts, 256).expect("not empty");
            match img.try_into_vec() {
                Ok(mut data) => {
                    data.par_chunks_mut(ROWS_OF * 4).for_each(|chunk| {
                        for p in chunk.as_chunks_mut::<4>().0 {
                            let [r, g, b] = table.map8([p[0], p[1], p[2]]);
                            p[..3].copy_from_slice(&[r, g, b]);
                        }
                    });
                    ImageBuf::new(w, h, f, data)
                }
                Err(img) => ImageBuf::with_samples::<u8>(w, h, f, |out| map_pixels(img.data(), out, |c| table.map8(c))),
            }
        }
        PixelFormat::Rgba16 => {
            let table = ToneTable::new(adjusts, 65536).expect("not empty");
            ImageBuf::with_samples::<u16>(w, h, f, |out| {
                map_pixels(bytemuck::cast_slice(img.data()), out, |c| table.map(c))
            })
        }
        PixelFormat::RgbaF32 => {
            let run = |rgb: [f32; 3]| adjusts.iter().fold(rgb, |c, a| a.apply(c));
            ImageBuf::with_samples::<f32>(w, h, f, |out| map_pixels(bytemuck::cast_slice(img.data()), out, run))
        }
    }
}

/// The edited image: `img` (stored pixels) with `g` applied. Each step's input is freed as soon
/// as its output exists.
pub fn render(img: &ImageBuf, g: &Geometry) -> Result<ImageBuf> {
    if (img.width(), img.height()) != g.stored {
        return Err(Error::decode("edit", "geometry was computed for a different image size"));
    }
    let mut out = crop(img, g.crop_pixels())?;
    out = orient(&out, g.orientation)?;
    out = resize(&out, g.size.0, g.size.1)?;
    adjust_owned(out, &g.adjusts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Op;

    /// w×h image whose pixel (x, y) is [x, y, 0, 255].
    fn coords(w: u32, h: u32) -> ImageBuf {
        let data = (0..h).flat_map(|y| (0..w).flat_map(move |x| [x as u8, y as u8, 0, 255])).collect();
        ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap()
    }

    fn px(img: &ImageBuf, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width() + x) * 4) as usize;
        img.data()[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn rotate_cw_moves_bottom_left_to_top_left() {
        let r = orient(&coords(4, 3), Orientation::Rotate90).unwrap();
        assert_eq!((r.width(), r.height()), (3, 4));
        assert_eq!(px(&r, 0, 0), [0, 2, 0, 255], "old bottom-left");
        assert_eq!(px(&r, 2, 0), [0, 0, 0, 255], "old top-left goes to top-right");
        assert_eq!(px(&r, 2, 3), [3, 0, 0, 255], "old top-right goes to bottom-right");
    }

    #[test]
    fn orientation_round_trips_are_exact() {
        let img = coords(5, 3);
        for o in Orientation::ALL {
            let back = orient(&orient(&img, o).unwrap(), o.inverse()).unwrap();
            assert_eq!(back.data(), img.data(), "{o:?}");
        }
        let four = (0..4).try_fold(img.clone(), |acc, _| orient(&acc, Orientation::Rotate90)).unwrap();
        assert_eq!(four.data(), img.data());
    }

    /// The integer remap lands every pixel where the orientation's UV map says it comes from.
    #[test]
    fn orient_matches_the_uv_map_for_every_orientation() {
        let (w, h) = (7u32, 4u32);
        let img = coords(w, h);
        for o in Orientation::ALL {
            let out = orient(&img, o).unwrap();
            let (ow, oh) = (out.width(), out.height());
            let m = o.display_to_stored_uv();
            for y in 0..oh {
                for x in 0..ow {
                    let (u, v) = ((x as f64 + 0.5) / ow as f64, (y as f64 + 0.5) / oh as f64);
                    let sx = ((m[0][0] * u + m[0][1] * v + m[0][2]) * w as f64) as u32;
                    let sy = ((m[1][0] * u + m[1][1] * v + m[1][2]) * h as f64) as u32;
                    assert_eq!(px(&out, x, y), px(&img, sx, sy), "{o:?} ({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn render_crops_after_rotation() {
        let img = coords(4, 3);
        let g = Geometry::new(
            (4, 3),
            Orientation::Normal,
            &[Op::rotate_cw(), Op::Crop(Rect { x: 0, y: 0, width: 1, height: 2 })],
        );
        let out = render(&img, &g).unwrap();
        assert_eq!((out.width(), out.height()), (1, 2));
        // the rotated image's left column, top two pixels: stored (0,2) then (1,2)
        assert_eq!(px(&out, 0, 0), [0, 2, 0, 255]);
        assert_eq!(px(&out, 0, 1), [1, 2, 0, 255]);
    }

    #[test]
    fn resize_keeps_flat_colour_and_alpha() {
        let opaque = ImageBuf::new(8, 8, PixelFormat::Rgba8, [10u8, 200, 30, 255].repeat(64)).unwrap();
        let out = resize(&opaque, 3, 5).unwrap();
        assert_eq!((out.width(), out.height()), (3, 5));
        assert!(out.data().as_chunks::<4>().0.iter().all(|p| *p == [10, 200, 30, 255]), "{:?}", &out.data()[..4]);

        // alpha-aware resampling premultiplies in 8 bits, so translucent colour may round by 1
        let translucent = ImageBuf::new(8, 8, PixelFormat::Rgba8, [10u8, 200, 30, 128].repeat(64)).unwrap();
        let out = resize(&translucent, 3, 5).unwrap();
        for p in out.data().as_chunks::<4>().0 {
            assert_eq!(p[3], 128);
            assert!(p.iter().zip([10u8, 200, 30]).all(|(a, b)| (*a as i32 - b as i32).abs() <= 1), "{p:?}");
        }
    }

    #[test]
    fn adjust_leaves_alpha_and_works_on_16_bit() {
        let img = ImageBuf::new(1, 1, PixelFormat::Rgba16, bytemuck::cast_slice(&[0u16, 32768, 65535, 1234]).to_vec())
            .unwrap();
        let out = adjust(&img, &[Adjust { saturation: -1.0, ..Default::default() }]).unwrap();
        let p: &[u16] = bytemuck::cast_slice(out.data());
        assert!(p[0] == p[1] && p[1] == p[2], "{p:?}");
        assert_eq!(p[3], 1234);
    }

    /// Adjusting is the per-pixel fold of `Adjust::apply`, whether it writes a new buffer or
    /// over an unshared one, for every pixel format.
    #[test]
    fn adjust_is_the_per_pixel_arithmetic() {
        let chain = [Adjust { brightness: 0.05, contrast: 0.3, gamma: 1.4, saturation: 0.5 }];
        let fold = |rgb: [f32; 3]| chain.iter().fold(rgb, |c, a| a.apply(c));
        let n = 70_000u32; // more than one parallel work item
        let data: Vec<u8> = (0..n).flat_map(|i| [(i % 251) as u8, (i % 256) as u8, (i / 300) as u8, 99]).collect();
        let img = ImageBuf::new(n, 1, PixelFormat::Rgba8, data).unwrap();
        let shared = adjust(&img, &chain).unwrap();
        let owned =
            adjust_owned(ImageBuf::new(n, 1, PixelFormat::Rgba8, img.data().to_vec()).unwrap(), &chain).unwrap();
        assert_eq!(shared.data(), owned.data());
        for (o, s) in shared.data().as_chunks::<4>().0.iter().zip(img.data().as_chunks::<4>().0) {
            let want = fold([s[0], s[1], s[2]].map(|c| c as f32 / 255.0)).map(|c| (c * 255.0).round() as u8);
            assert_eq!(*o, [want[0], want[1], want[2], 99]);
        }

        let deep: Vec<u16> = (0..n * 4).map(|i| (i * 7919 % 65536) as u16).collect();
        let img16 = ImageBuf::new(n, 1, PixelFormat::Rgba16, bytemuck::cast_slice(&deep).to_vec()).unwrap();
        let out16 = adjust(&img16, &chain).unwrap();
        for (o, s) in
            bytemuck::cast_slice::<u8, u16>(out16.data()).as_chunks::<4>().0.iter().zip(deep.as_chunks::<4>().0)
        {
            let want = fold([s[0], s[1], s[2]].map(|c| c as f32 / 65535.0)).map(|c| (c * 65535.0).round() as u16);
            assert_eq!(*o, [want[0], want[1], want[2], s[3]]);
        }

        let float: Vec<f32> = (0..400).map(|i| i as f32 / 397.0).collect();
        let imgf = ImageBuf::new(100, 1, PixelFormat::RgbaF32, bytemuck::cast_slice(&float).to_vec()).unwrap();
        let outf = adjust(&imgf, &chain).unwrap();
        for (o, s) in
            bytemuck::cast_slice::<u8, f32>(outf.data()).as_chunks::<4>().0.iter().zip(float.as_chunks::<4>().0)
        {
            let want = fold([s[0], s[1], s[2]]);
            assert_eq!(*o, [want[0], want[1], want[2], s[3]]);
        }
    }

    #[test]
    fn mismatched_geometry_is_rejected() {
        let g = Geometry::new((10, 10), Orientation::Normal, &[]);
        assert!(render(&coords(4, 3), &g).is_err());
    }
}
