//! Turns an [`ImageBuf`] into RGBA8 pixels a GPU texture can hold.

use std::borrow::Cow;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use iv_core::{ImageBuf, PixelFormat};

/// RGBA8 pixels ready for upload, possibly reduced to fit the GPU's texture size limit.
pub struct TexturePixels<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba8: Cow<'a, [u8]>,
}

/// Linear → sRGB transfer, for float (scene-linear) sources.
fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

fn to_rgba8(img: &ImageBuf) -> Cow<'_, [u8]> {
    match img.format() {
        PixelFormat::Rgba8 => Cow::Borrowed(img.data()),
        PixelFormat::Rgba16 => {
            let src: &[u16] = bytemuck::cast_slice(img.data());
            Cow::Owned(src.iter().map(|&v| ((v as u32 + 128) / 257) as u8).collect())
        }
        PixelFormat::RgbaF32 => {
            // HDR tone mapping is a later milestone; for now clip to SDR. Alpha is linear coverage.
            let src: &[f32] = bytemuck::cast_slice(img.data());
            Cow::Owned(
                src.chunks_exact(4)
                    .flat_map(|p| {
                        let enc = |c: f32| (linear_to_srgb(c.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
                        [enc(p[0]), enc(p[1]), enc(p[2]), (p[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8]
                    })
                    .collect(),
            )
        }
    }
}

/// Converts to RGBA8 and, if either side exceeds `max_side`, shrinks (Lanczos3, alpha-aware) so
/// both sides fit. Large images get proper tiling in a later milestone; until then they are
/// shown reduced rather than refused.
pub fn prepare(img: &ImageBuf, max_side: u32) -> TexturePixels<'_> {
    let rgba8 = to_rgba8(img);
    let (w, h) = (img.width(), img.height());
    if w <= max_side && h <= max_side {
        return TexturePixels { width: w, height: h, rgba8 };
    }

    let scale = max_side as f64 / w.max(h) as f64;
    let (nw, nh) = (
        ((w as f64 * scale).round() as u32).clamp(1, max_side),
        ((h as f64 * scale).round() as u32).clamp(1, max_side),
    );
    let src = ImageRef::new(w, h, &rgba8, PixelType::U8x4).expect("RGBA8 buffer matches its dimensions");
    let mut dst = Image::new(nw, nh, PixelType::U8x4);
    Resizer::new()
        .resize(&src, &mut dst, &ResizeOptions::new().use_alpha(true))
        .expect("same pixel type on both sides");
    TexturePixels { width: nw, height: nh, rgba8: Cow::Owned(dst.into_vec()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba8_is_borrowed_not_copied() {
        let img = ImageBuf::new(2, 1, PixelFormat::Rgba8, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let p = prepare(&img, 16);
        assert!(matches!(p.rgba8, Cow::Borrowed(_)));
        assert_eq!(&*p.rgba8, &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn sixteen_bit_rounds_to_nearest() {
        let px: [u16; 4] = [0, 65535, 257 * 100 + 128, 32896];
        let img = ImageBuf::new(1, 1, PixelFormat::Rgba16, bytemuck::cast_slice(&px).to_vec()).unwrap();
        assert_eq!(&*prepare(&img, 16).rgba8, &[0, 255, 100, 128]);
    }

    #[test]
    fn float_is_srgb_encoded_and_clipped() {
        let px: [f32; 4] = [0.0, 1.0, 0.214_041_14, 2.0]; // 0.214 linear ≈ 128 sRGB
        let img = ImageBuf::new(1, 1, PixelFormat::RgbaF32, bytemuck::cast_slice(&px).to_vec()).unwrap();
        let out = prepare(&img, 16);
        assert_eq!(out.rgba8[0], 0);
        assert_eq!(out.rgba8[1], 255);
        assert!((out.rgba8[2] as i32 - 128).abs() <= 1, "{}", out.rgba8[2]);
        assert_eq!(out.rgba8[3], 255);
    }

    #[test]
    fn oversized_images_are_reduced_to_the_limit_keeping_aspect() {
        let img = ImageBuf::new(400, 100, PixelFormat::Rgba8, vec![200; 400 * 100 * 4]).unwrap();
        let p = prepare(&img, 100);
        assert_eq!((p.width, p.height), (100, 25));
        assert_eq!(p.rgba8.len(), 100 * 25 * 4);
        assert!(p.rgba8.iter().all(|&b| b == 200), "a flat image stays flat");
    }
}
