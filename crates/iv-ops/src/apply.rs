//! Applies a [`Geometry`] to pixels: crop, orient, resize, adjust.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use iv_core::{Error, ImageBuf, Orientation, PixelFormat, Result};

use crate::geometry::Geometry;
use crate::op::{Adjust, Rect};

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

/// Bakes `o` into the pixels: the result, shown without transform, looks like `img` shown with `o`.
pub fn orient(img: &ImageBuf, o: Orientation) -> Result<ImageBuf> {
    if o == Orientation::Normal {
        return Ok(img.clone());
    }
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (ow, oh) = if o.swaps_dimensions() { (h, w) } else { (w, h) };
    let bpp = img.format().bytes_per_pixel();
    let m = o.display_to_stored_uv();
    let src = img.data();
    let mut out = vec![0u8; ow * oh * bpp];
    for y in 0..oh {
        let v = (y as f64 + 0.5) / oh as f64;
        for x in 0..ow {
            let u = (x as f64 + 0.5) / ow as f64;
            let sx = ((m[0][0] * u + m[0][1] * v + m[0][2]) * w as f64) as usize;
            let sy = ((m[1][0] * u + m[1][1] * v + m[1][2]) * h as f64) as usize;
            let s = (sy.min(h - 1) * w + sx.min(w - 1)) * bpp;
            let d = (y * ow + x) * bpp;
            out[d..d + bpp].copy_from_slice(&src[s..s + bpp]);
        }
    }
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

/// Applies tone adjustments in order to every pixel.
pub fn adjust(img: &ImageBuf, adjusts: &[Adjust]) -> Result<ImageBuf> {
    if adjusts.is_empty() {
        return Ok(img.clone());
    }
    let run = |rgb: [f32; 3]| adjusts.iter().fold(rgb, |c, a| a.apply(c));
    let (w, h, f) = (img.width(), img.height(), img.format());
    let data = match f {
        PixelFormat::Rgba8 => img
            .data()
            .chunks_exact(4)
            .flat_map(|p| {
                let [r, g, b] = run([p[0], p[1], p[2]].map(|c| c as f32 / 255.0)).map(|c| (c * 255.0).round() as u8);
                [r, g, b, p[3]]
            })
            .collect(),
        PixelFormat::Rgba16 => {
            let px: &[u16] = bytemuck::cast_slice(img.data());
            let out: Vec<u16> = px
                .chunks_exact(4)
                .flat_map(|p| {
                    let [r, g, b] =
                        run([p[0], p[1], p[2]].map(|c| c as f32 / 65535.0)).map(|c| (c * 65535.0).round() as u16);
                    [r, g, b, p[3]]
                })
                .collect();
            bytemuck::cast_slice(&out).to_vec()
        }
        PixelFormat::RgbaF32 => {
            let px: &[f32] = bytemuck::cast_slice(img.data());
            let out: Vec<f32> = px
                .chunks_exact(4)
                .flat_map(|p| {
                    let [r, g, b] = run([p[0], p[1], p[2]]);
                    [r, g, b, p[3]]
                })
                .collect();
            bytemuck::cast_slice(&out).to_vec()
        }
    };
    ImageBuf::new(w, h, f, data)
}

/// The edited image: `img` (stored pixels) with `g` applied.
pub fn render(img: &ImageBuf, g: &Geometry) -> Result<ImageBuf> {
    if (img.width(), img.height()) != g.stored {
        return Err(Error::decode("edit", "geometry was computed for a different image size"));
    }
    let cropped = crop(img, g.crop_pixels())?;
    let oriented = orient(&cropped, g.orientation)?;
    let sized = resize(&oriented, g.size.0, g.size.1)?;
    adjust(&sized, &g.adjusts)
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
        assert!(out.data().chunks_exact(4).all(|p| p == [10, 200, 30, 255]), "{:?}", &out.data()[..4]);

        // alpha-aware resampling premultiplies in 8 bits, so translucent colour may round by 1
        let translucent = ImageBuf::new(8, 8, PixelFormat::Rgba8, [10u8, 200, 30, 128].repeat(64)).unwrap();
        let out = resize(&translucent, 3, 5).unwrap();
        for p in out.data().chunks_exact(4) {
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

    #[test]
    fn mismatched_geometry_is_rejected() {
        let g = Geometry::new((10, 10), Orientation::Normal, &[]);
        assert!(render(&coords(4, 3), &g).is_err());
    }
}
