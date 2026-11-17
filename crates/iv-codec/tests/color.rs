//! ICC colour management against a known reference: sRGB pure red is Display P3 (234, 51, 35),
//! so a P3-tagged pixel of that value must come out as sRGB (255, 0, 0).

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use iv_codec::default_registry;
use iv_core::{CancelToken, PixelFormat, Source};
use moxcms::ColorProfile;

fn png_with_profile(px: [u8; 3], icc: Option<Vec<u8>>) -> Vec<u8> {
    let data: Vec<u8> = std::iter::repeat_n(px, 4).flatten().collect();
    let mut out = Vec::new();
    let mut enc = PngEncoder::new(&mut out);
    if let Some(icc) = icc {
        enc.set_icc_profile(icc).unwrap();
    }
    enc.write_image(&data, 2, 2, ExtendedColorType::Rgb8).unwrap();
    out
}

fn p3() -> Vec<u8> {
    ColorProfile::new_display_p3().encode().unwrap()
}

fn decode_px(bytes: Vec<u8>) -> ([u8; 4], bool) {
    let d = default_registry().decode(&Source::from_bytes("a.png", bytes), &CancelToken::never()).unwrap();
    assert_eq!(d.image.format(), PixelFormat::Rgba8);
    (d.image.data()[..4].try_into().unwrap(), d.color_managed)
}

fn near(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= tol)
}

#[test]
fn display_p3_is_converted_to_srgb() {
    let (px, managed) = decode_px(png_with_profile([234, 51, 35], Some(p3())));
    assert!(managed);
    assert!(near(px, [255, 0, 0, 255], 4), "P3 (234,51,35) should be sRGB red, got {px:?}");
}

#[test]
fn neutral_grey_stays_neutral() {
    // P3 and sRGB share white point and transfer curve
    let (px, _) = decode_px(png_with_profile([128, 128, 128], Some(p3())));
    assert!(near(px, [128, 128, 128, 255], 2), "{px:?}");
}

#[test]
fn srgb_tagged_and_untagged_pixels_are_unchanged() {
    let srgb = ColorProfile::new_srgb().encode().unwrap();
    let (tagged, managed) = decode_px(png_with_profile([200, 100, 50], Some(srgb)));
    assert!(managed);
    assert!(near(tagged, [200, 100, 50, 255], 1), "{tagged:?}");

    let (untagged, managed) = decode_px(png_with_profile([200, 100, 50], None));
    assert!(!managed);
    assert_eq!(untagged, [200, 100, 50, 255]);
}

#[test]
fn sixteen_bit_pixels_are_converted_too() {
    let v = |c: u8| c as u16 * 257;
    let data: Vec<u16> = std::iter::repeat_n([v(234), v(51), v(35)], 4).flatten().collect();
    let mut out = Vec::new();
    let mut enc = PngEncoder::new(&mut out);
    enc.set_icc_profile(p3()).unwrap();
    enc.write_image(bytemuck::cast_slice(&data), 2, 2, ExtendedColorType::Rgb16).unwrap();
    let d = default_registry().decode(&Source::from_bytes("a.png", out), &CancelToken::never()).unwrap();
    assert_eq!(d.image.format(), PixelFormat::Rgba16);
    let px: &[u16] = bytemuck::cast_slice(d.image.data());
    let px8 = [px[0], px[1], px[2], px[3]].map(|c| (c / 257) as u8);
    assert!(near(px8, [255, 0, 0, 255], 4), "{px8:?}");
}
