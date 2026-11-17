//! Colour management: pixels decoded with an embedded RGB ICC profile (Display P3, Adobe RGB,
//! ProPhoto …) are converted to sRGB, so a wide-gamut photo does not look washed out on an sRGB
//! display. Done once after decoding, so the cache holds display-ready pixels.

use iv_core::{ImageBuf, PixelFormat};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};

use crate::decoder::Decoded;

/// Why a profile was not applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Skipped {
    NoProfile,
    /// Grey or CMYK profiles describe data the decoders have already expanded to RGB.
    NotRgb,
    /// Float (HDR) pixels are handled by tone mapping, not ICC.
    FloatPixels,
    Unreadable(String),
}

/// 5 × 5 × 5 RGB samples across the cube, opaque.
fn probe_colours() -> Vec<u8> {
    let levels = [0u8, 64, 128, 192, 255];
    let mut v = Vec::with_capacity(125 * 4);
    for r in levels {
        for g in levels {
            for b in levels {
                v.extend_from_slice(&[r, g, b, 255]);
            }
        }
    }
    v
}

/// Whether `src → dst` leaves every probe colour within ±1: an sRGB-tagged image (the common
/// case for cameras) then skips the full-image pass (13 ms at 9 MP).
fn is_identity(src: &ColorProfile, dst: &ColorProfile) -> bool {
    let Ok(t) = src.create_transform_8bit(Layout::Rgba, dst, Layout::Rgba, TransformOptions::default()) else {
        return false;
    };
    let input = probe_colours();
    let mut out = vec![0u8; input.len()];
    t.transform(&input, &mut out).is_ok() && input.iter().zip(&out).all(|(a, b)| (*a as i32 - *b as i32).abs() <= 1)
}

fn convert(img: &ImageBuf, src: &ColorProfile, dst: &ColorProfile) -> Result<ImageBuf, String> {
    let opts = TransformOptions::default();
    match img.format() {
        PixelFormat::Rgba8 => {
            let t = src.create_transform_8bit(Layout::Rgba, dst, Layout::Rgba, opts).map_err(|e| e.to_string())?;
            let mut out = vec![0u8; img.data().len()];
            t.transform(img.data(), &mut out).map_err(|e| e.to_string())?;
            ImageBuf::new(img.width(), img.height(), PixelFormat::Rgba8, out).map_err(|e| e.to_string())
        }
        PixelFormat::Rgba16 => {
            let t = src.create_transform_16bit(Layout::Rgba, dst, Layout::Rgba, opts).map_err(|e| e.to_string())?;
            let input: &[u16] = bytemuck::cast_slice(img.data());
            let mut result = Ok(());
            let out = ImageBuf::with_samples::<u16>(img.width(), img.height(), PixelFormat::Rgba16, |out| {
                result = t.transform(input, out).map_err(|e| e.to_string());
            })
            .map_err(|e| e.to_string())?;
            result.map(|()| out)
        }
        PixelFormat::RgbaF32 => Err("float".into()),
    }
}

/// The profile `icc` and sRGB, when pixels of `format` described by `icc` need converting;
/// `None` when they are sRGB already.
fn conversion(icc: &[u8], format: PixelFormat) -> Result<Option<(ColorProfile, ColorProfile)>, Skipped> {
    if format == PixelFormat::RgbaF32 {
        return Err(Skipped::FloatPixels);
    }
    let src = ColorProfile::new_from_slice(icc).map_err(|e| Skipped::Unreadable(e.to_string()))?;
    if src.color_space != DataColorSpace::Rgb {
        return Err(Skipped::NotRgb);
    }
    let dst = ColorProfile::new_srgb();
    Ok((!is_identity(&src, &dst)).then_some((src, dst)))
}

/// `img`, described by the ICC profile `icc`, converted to sRGB; `None` when it is sRGB already.
/// For saving to a format that cannot hold the profile.
pub fn image_to_srgb(img: &ImageBuf, icc: &[u8]) -> Result<Option<ImageBuf>, Skipped> {
    let Some((src, dst)) = conversion(icc, img.format())? else { return Ok(None) };
    convert(img, &src, &dst).map(Some).map_err(Skipped::Unreadable)
}

/// Converts `decoded` (all frames) from its embedded ICC profile to sRGB in place.
pub fn to_srgb(decoded: &mut Decoded) -> Result<(), Skipped> {
    let Some(icc) = decoded.info.icc_profile.clone() else { return Err(Skipped::NoProfile) };
    // already sRGB: the pixels are display-ready as decoded
    let Some((src, dst)) = conversion(&icc, decoded.image.format())? else { return Ok(()) };
    decoded.image = convert(&decoded.image, &src, &dst).map_err(Skipped::Unreadable)?;
    if let Some(anim) = decoded.animation.as_mut() {
        for (i, frame) in anim.frames.iter_mut().enumerate() {
            // the first frame is the still image; keep them sharing pixels
            frame.image = if i == 0 {
                decoded.image.clone()
            } else {
                convert(&frame.image, &src, &dst).map_err(Skipped::Unreadable)?
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_detection() {
        let srgb = ColorProfile::new_srgb();
        assert!(is_identity(&srgb, &srgb));
        assert!(!is_identity(&ColorProfile::new_display_p3(), &srgb));
        assert!(!is_identity(&ColorProfile::new_adobe_rgb(), &srgb));
    }

    #[test]
    fn srgb_tagged_images_keep_their_pixel_buffer() {
        let img = ImageBuf::new(1, 1, PixelFormat::Rgba8, vec![10, 20, 30, 255]).unwrap();
        let info = iv_core::ImageInfo {
            format: iv_core::FormatId::Png,
            codec: "t",
            width: 1,
            height: 1,
            frame_count: Some(1),
            has_alpha: false,
            bits_per_channel: 8,
            orientation: iv_core::Orientation::Normal,
            icc_profile: Some(ColorProfile::new_srgb().encode().unwrap().into()),
            exif: None,
        };
        let mut d = Decoded::still(info, img.clone());
        assert_eq!(to_srgb(&mut d), Ok(()));
        assert_eq!(d.image.data().as_ptr(), img.data().as_ptr(), "no copy, no conversion");
    }
}
