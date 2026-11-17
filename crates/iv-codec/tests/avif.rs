//! AVIF decoding paths and the Windows Imaging Component decoder.
//!
//! Fixtures in `testdata/avif` were made with ffmpeg (libaom) from the synthetic `alpha.png`
//! produced by the `fixtures` example: 400x200, red top half, blue bottom half, alpha ramping
//! 0 → 255 left to right. `opaque.avif` drops the alpha.
//!
//! AVIFs with container rotation, mirroring and colour profiles are made at test time: the AVIF
//! encoder's output with `irot`/`imir`/`colr` boxes added (`support/avif_props.rs`). The same
//! files were checked by hand against ffmpeg 9.0.2, which turned all twelve the same way.

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

#[path = "support/avif_props.rs"]
mod avif_props;

use avif_props::{colr_icc, colr_nclx, imir, irot, with_properties};
use iv_codec::encode::{EncodeOptions, Encoders, Metadata, prepare};
use iv_core::Orientation;

const TL: [u8; 3] = [200, 60, 40];
const TR: [u8; 3] = [60, 180, 70];
const BL: [u8; 3] = [50, 70, 190];
const BR: [u8; 3] = [230, 230, 230];

/// 48x32 in four flat quadrants of distinct colours, so every one of the eight orientations
/// looks different. Mid-gamut colours, so a Display P3 profile visibly changes them.
fn quadrants_avif() -> Vec<u8> {
    let (w, h) = (48u32, 32u32);
    let mut data = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let c = match (x < w / 2, y < h / 2) {
                (true, true) => TL,
                (false, true) => TR,
                (true, false) => BL,
                (false, false) => BR,
            };
            data.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    let img = ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap();
    let encoders = Encoders::default();
    let enc = encoders.for_format(FormatId::Avif).unwrap();
    let opts = EncodeOptions::parse(enc.options(), &["quality=95".into(), "speed=10".into()]).unwrap();
    enc.encode(&prepare(&img, enc.info(), [0; 3]), &opts, &Metadata::default()).unwrap()
}

fn avif_source(bytes: Vec<u8>) -> Source {
    Source::from_bytes("photo.avif", bytes)
}

type Pixels = (Vec<[u8; 4]>, (u32, u32));

fn pixels(img: &ImageBuf) -> Pixels {
    (img.to_rgba8().as_chunks::<4>().0.to_vec(), (img.width(), img.height()))
}

/// The image as the viewer shows it: each displayed pixel centre mapped to a stored pixel
/// through `Orientation::display_to_stored_uv`, the transform the renderer and `iv-ops` use.
fn shown(stored: &Pixels, o: Orientation) -> Pixels {
    let (px, (w, h)) = stored;
    let (sw, sh) = if o.swaps_dimensions() { (*h, *w) } else { (*w, *h) };
    let m = o.display_to_stored_uv();
    let mut out = Vec::new();
    for y in 0..sh {
        for x in 0..sw {
            let (u, v) = ((x as f64 + 0.5) / sw as f64, (y as f64 + 0.5) / sh as f64);
            let s = m[0][0] * u + m[0][1] * v + m[0][2];
            let t = m[1][0] * u + m[1][1] * v + m[1][2];
            let (sx, sy) = ((s * *w as f64) as u32, (t * *h as f64) as u32);
            out.push(px[(sy * w + sx) as usize]);
        }
    }
    (out, (sw, sh))
}

/// What HEIF prescribes, written out on pixel indices: turn `angle` quarter turns
/// anticlockwise, then mirror (axis 0: top and bottom exchanged, 1: left and right).
fn heif_upright(stored: &Pixels, angle: u8, axis: Option<u8>) -> Pixels {
    let (mut px, (mut w, mut h)) = stored.clone();
    for _ in 0..angle {
        // anticlockwise: the top-right corner comes to the top left; new (x, y) = old (w-1-y, x)
        let (nw, nh) = (h, w);
        px = (0..nh)
            .flat_map(|y| (0..nw).map(move |x| (x, y)))
            .map(|(x, y)| px[(x * w + (w - 1 - y)) as usize])
            .collect();
        (w, h) = (nw, nh);
    }
    let at = |x: u32, y: u32| (y * w + x) as usize;
    let grid = || (0..h).flat_map(|y| (0..w).map(move |x| (x, y)));
    px = match axis {
        None => px,
        Some(0) => grid().map(|(x, y)| px[at(x, h - 1 - y)]).collect(),
        Some(_) => grid().map(|(x, y)| px[at(w - 1 - x, y)]).collect(),
    };
    (px, (w, h))
}

/// Which quadrant colour each quarter of `img` shows: top left, top right, bottom left, bottom
/// right.
fn corners(img: &Pixels) -> [[u8; 3]; 4] {
    let (px, (w, h)) = img;
    let nearest = |p: [u8; 4]| {
        let d = |c: [u8; 3]| (0..3).map(|i| (p[i] as i32 - c[i] as i32).abs()).max().unwrap();
        let best = [TL, TR, BL, BR].into_iter().min_by_key(|c| d(*c)).unwrap();
        assert!(d(best) <= 12, "{p:?} is none of the quadrant colours");
        best
    };
    [(w / 4, h / 4), (w * 3 / 4, h / 4), (w / 4, h * 3 / 4), (w * 3 / 4, h * 3 / 4)]
        .map(|(x, y)| nearest(px[(y * w + x) as usize]))
}

/// The four rotations, each alone and followed by either mirror.
fn every_transform() -> impl Iterator<Item = (u8, Option<u8>)> {
    (0..4).flat_map(|angle| [None, Some(0), Some(1)].map(move |axis| (angle, axis)))
}

#[test]
fn container_rotation_and_mirror_show_the_image_as_heif_prescribes() {
    let base = quadrants_avif();
    let reg = default_registry();
    let plain = reg.decode(&avif_source(base.clone()), &CancelToken::never()).unwrap();
    assert_eq!(plain.info.orientation, Orientation::Normal);
    let stored = pixels(&plain.image);
    assert_eq!(corners(&stored), [TL, TR, BL, BR]);

    let mut seen = std::collections::HashSet::new();
    for (angle, axis) in every_transform() {
        let mut props = vec![irot(angle)];
        props.extend(axis.map(imir));
        let file = with_properties(&base, &props);
        let probe = reg.probe(&avif_source(file.clone())).unwrap();
        let d = reg.decode(&avif_source(file), &CancelToken::never()).unwrap();
        let case = format!("irot {angle} imir {axis:?}");
        assert_eq!(d.info.codec, "rav1d", "{case}");

        // the pixels and size stay as stored; the orientation says how to show them
        assert!(pixels(&d.image) == stored, "{case}: stored pixels");
        assert_eq!((probe.width, probe.height), (48, 32), "{case}");
        assert_eq!((d.info.width, d.info.height), (48, 32), "{case}");
        assert_eq!(probe.orientation, d.info.orientation, "{case}: probe and decode agree");
        let upright = if angle % 2 == 1 { (32, 48) } else { (48, 32) };
        assert_eq!(probe.display_size(), upright, "{case}");
        assert_eq!(d.info.display_size(), upright, "{case}");

        assert!(shown(&stored, d.info.orientation) == heif_upright(&stored, angle, axis), "{case}: shown upright");
        seen.insert(d.info.orientation);
    }
    assert_eq!(seen.len(), 8, "all eight orientations occur: {seen:?}");
}

/// The same told by the corners: a quarter turn anticlockwise brings the top-right corner to
/// the top left.
#[test]
fn a_quarter_turn_anticlockwise_brings_the_top_right_corner_to_the_top_left() {
    let base = quadrants_avif();
    let reg = default_registry();
    for (props, want) in [
        (vec![irot(1)], [TR, BR, TL, BL]),
        (vec![irot(1), imir(0)], [TL, BL, TR, BR]),
        (vec![irot(1), imir(1)], [BR, TR, BL, TL]),
        (vec![irot(3)], [BL, TL, BR, TR]),
        (vec![imir(0)], [BL, BR, TL, TR]),
    ] {
        let d = reg.decode(&avif_source(with_properties(&base, &props)), &CancelToken::never()).unwrap();
        assert_eq!(corners(&shown(&pixels(&d.image), d.info.orientation)), want, "{:?}", d.info.orientation);
    }
}

#[test]
fn an_embedded_icc_profile_is_passed_on_and_applied() {
    let base = quadrants_avif();
    let reg = default_registry();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    assert_eq!(reg.probe(&avif_source(base.clone())).unwrap().icc_profile, None, "the encoder writes no profile");

    let nclx_only = with_properties(&base, &[colr_nclx()]);
    assert_eq!(reg.probe(&avif_source(nclx_only)).unwrap().icc_profile, None, "code points are not a profile");

    // as libavif writes it: the profile, code points beside it, then the rotation
    let file = with_properties(&base, &[colr_icc(&p3), colr_nclx(), irot(3)]);
    let probe = reg.probe(&avif_source(file.clone())).unwrap();
    assert_eq!(probe.icc_profile.as_deref(), Some(p3.as_slice()));
    assert_eq!(probe.orientation, Orientation::Rotate90, "the profile does not hide the rotation");

    let raw = reg.decode_unmanaged(&avif_source(file.clone()), &CancelToken::never()).unwrap();
    assert_eq!(raw.info.icc_profile.as_deref(), Some(p3.as_slice()));
    let managed = reg.decode(&avif_source(file), &CancelToken::never()).unwrap();
    assert!(managed.color_managed);

    // the profile's own conversion of the stored pixel, in the top-left quadrant
    let t = moxcms::ColorProfile::new_from_slice(&p3)
        .unwrap()
        .create_transform_8bit(
            moxcms::Layout::Rgba,
            &moxcms::ColorProfile::new_srgb(),
            moxcms::Layout::Rgba,
            moxcms::TransformOptions::default(),
        )
        .unwrap();
    let before = rgba8_at(&raw.image, 12, 8);
    let mut want = [0u8; 4];
    t.transform(&before, &mut want).unwrap();
    let got = rgba8_at(&managed.image, 12, 8);
    assert!(got.iter().zip(want).all(|(a, b)| (*a as i32 - b as i32).abs() <= 1), "{got:?} vs {want:?}");
    assert!(got.iter().zip(before).any(|(a, b)| (*a as i32 - b as i32).abs() >= 8), "P3 changes {before:?}: {got:?}");
}

/// The fallback decoder: the Windows AV1 codec applies `irot`/`imir` itself and reports the
/// upright size with no orientation. An independent reading of the same boxes, so it also checks
/// the fixtures. Skipped where the AV1 codec is not installed.
#[cfg(windows)]
#[test]
fn the_wic_fallback_shows_oriented_avif_the_same_way() {
    use iv_codec::{Decoder, WicDecoder};
    let base = quadrants_avif();
    let wic = WicDecoder::fallback();
    if let Err(e) = wic.decode(&avif_source(base.clone()), FormatId::Avif, &CancelToken::never()) {
        eprintln!("skipped: WIC does not decode AVIF here ({e})");
        return;
    }
    // WIC decodes these BT.601 colours with BT.709 coefficients, so only the nearest colour counts
    let loose = |img: &Pixels| {
        let (px, (w, h)) = img;
        [(w / 4, h / 4), (w * 3 / 4, h / 4), (w / 4, h * 3 / 4), (w * 3 / 4, h * 3 / 4)].map(|(x, y)| {
            let p = px[(y * w + x) as usize];
            let d = |c: [u8; 3]| (0..3).map(|i| (p[i] as i32 - c[i] as i32).abs()).sum::<i32>();
            [TL, TR, BL, BR].into_iter().min_by_key(|c| d(*c)).unwrap()
        })
    };
    for (angle, axis) in every_transform() {
        let mut props = vec![colr_nclx(), irot(angle)];
        props.extend(axis.map(imir));
        let file = with_properties(&base, &props);
        let ours = default_registry().decode(&avif_source(file.clone()), &CancelToken::never()).unwrap();
        let theirs = wic.decode(&avif_source(file), FormatId::Avif, &CancelToken::never()).unwrap();
        let case = format!("irot {angle} imir {axis:?}");
        assert_eq!(theirs.info.orientation, Orientation::Normal, "{case}");
        assert_eq!((theirs.info.width, theirs.info.height), ours.info.display_size(), "{case}");
        let shown_ours = shown(&pixels(&ours.image), ours.info.orientation);
        assert_eq!(corners(&shown_ours), loose(&pixels(&theirs.image)), "{case}");
    }
}
