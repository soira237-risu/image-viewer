//! AVIF decoding paths and the Windows Imaging Component decoder.
//!
//! Fixtures in `testdata/avif` were made with ffmpeg (libaom) from the synthetic `alpha.png`
//! produced by the `fixtures` example: 400x200, red top half, blue bottom half, alpha ramping
//! 0 → 255 left to right. `opaque.avif` drops the alpha.

use std::path::{Path, PathBuf};

use iv_codec::default_registry;
use iv_core::{CancelToken, FormatId, ImageBuf, PixelFormat, Source};

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
}

/// RGBA8 of the pixel at (x, y), whatever the decoded precision.
fn rgba8_at(img: &ImageBuf, x: u32, y: u32) -> [u8; 4] {
    let i = (y * img.width() + x) as usize * 4;
    match img.format() {
        PixelFormat::Rgba8 => img.data()[i..i + 4].try_into().unwrap(),
        PixelFormat::Rgba16 => {
            let px: &[u16] = bytemuck::cast_slice(img.data());
            [0, 1, 2, 3].map(|c| (px[i + c] >> 8) as u8)
        }
        PixelFormat::RgbaF32 => unreachable!("AVIF output is integer"),
    }
}

fn decode(name: &str) -> iv_codec::Decoded {
    default_registry().decode(&Source::from_path(testdata(name)), &CancelToken::never()).unwrap()
}

/// Red/blue halves; with `alpha`, transparent on the left and opaque on the right.
fn assert_halves(img: &ImageBuf, alpha: bool) {
    assert_eq!((img.width(), img.height()), (400, 200));
    let left = rgba8_at(img, 2, 50);
    let top = rgba8_at(img, 397, 50);
    let bottom = rgba8_at(img, 397, 150);
    if alpha {
        assert!(left[3] < 20, "transparent at the left edge: {left:?}");
    } else {
        assert_eq!(left[3], 255, "opaque everywhere: {left:?}");
    }
    assert!(top[3] > 235 && top[0] > 180 && top[1] < 80 && top[2] < 80, "opaque red: {top:?}");
    assert!(bottom[3] > 235 && bottom[2] > 180 && bottom[0] < 80, "opaque blue: {bottom:?}");
}

#[test]
fn avif_with_alpha_keeps_its_alpha() {
    let d = decode("avif/alpha.avif");
    assert_eq!(d.info.format, FormatId::Avif);
    assert!(d.info.has_alpha);
    assert_eq!(d.info.codec, "rav1d");
    assert_halves(&d.image, true);
}

#[test]
fn ten_bit_avif_with_alpha_has_full_range_colour() {
    let d = decode("avif/alpha10.avif");
    assert_eq!(d.image.format(), PixelFormat::Rgba16);
    assert_halves(&d.image, true);
}

#[test]
fn opaque_avif_decodes() {
    let d = decode("avif/opaque.avif");
    assert!(!d.info.has_alpha);
    assert_eq!(d.info.codec, "rav1d");
    assert_halves(&d.image, false);
}

/// `testdata/heic/halves.heic` was written by the Windows HEIF encoder (`heic_fixture` example)
/// from the same 400x200 red/blue image; the encoder drops alpha.
///
/// HEIF in WIC needs two optional codecs: the HEIF container decoder and the HEVC video decoder it
/// loads. Without the first WIC reports WINCODEC_ERR_COMPONENTNOTFOUND; with the container but no
/// HEVC codec (the GitHub Windows runner) it reports WINCODEC_ERR_COMPONENTINITIALIZEFAILURE.
/// The WIC decoder turns both into `Error::CodecMissing`, and then the test is skipped; any other
/// failure fails.
#[cfg(windows)]
#[test]
fn heic_decodes_through_wic() {
    let src = Source::from_path(testdata("heic/halves.heic"));
    let d = match default_registry().decode(&src, &CancelToken::never()) {
        Ok(d) => d,
        Err(e @ iv_core::Error::CodecMissing { format: FormatId::Heif }) => {
            eprintln!("skipped: WIC HEIF/HEVC codecs not installed ({e})");
            return;
        }
        Err(e) => panic!("HEIF decode failed: {e}"),
    };
    assert_eq!(d.info.format, FormatId::Heif);
    assert_eq!(d.info.codec, "wic");
    let img = &d.image;
    assert_eq!((img.width(), img.height()), (400, 200));
    let top = rgba8_at(img, 200, 50);
    let bottom = rgba8_at(img, 200, 150);
    assert!(top[0] > 180 && top[1] < 80 && top[2] < 80 && top[3] == 255, "red: {top:?}");
    assert!(bottom[2] > 180 && bottom[0] < 80 && bottom[3] == 255, "blue: {bottom:?}");
}

/// `bt601-patches.avif` (written by ravif, BT.601 matrix, full range) holds red, green and
/// (200, 100, 50) patches, 16 px wide each. ffmpeg decodes them to (254,0,0), (0,255,1),
/// (200,100,50); the Windows AV1 codec applies BT.709 instead and gives (255,25,0) for red.
#[test]
fn bt601_avif_colours_are_right() {
    let d = decode("avif/bt601-patches.avif");
    assert_eq!(d.info.codec, "rav1d");
    let expect = [[254u8, 0, 0], [0, 255, 1], [200, 100, 50]];
    for (k, want) in expect.iter().enumerate() {
        let got = rgba8_at(&d.image, k as u32 * 16 + 8, 8);
        assert!(
            got.iter().zip(want).all(|(a, b)| (*a as i32 - *b as i32).abs() <= 2),
            "patch {k}: {got:?} vs {want:?}"
        );
    }
}
