//! Every encoder, round-tripped through the decoders: lossless formats must be pixel-exact,
//! lossy ones above a PSNR floor; metadata must survive.

use iv_codec::default_registry;
use iv_codec::encode::{EncodeOptions, Encoders, Metadata, prepare};
use iv_core::{CancelToken, FormatId, ImageBuf, Orientation, PixelFormat, Source};

/// Smooth gradients (compress like photos) with an optional alpha ramp.
fn gradient(w: u32, h: u32, alpha: bool) -> ImageBuf {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let a = if alpha { (x * 255 / (w - 1)) as u8 } else { 255 };
            data.extend_from_slice(&[(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, 128, a]);
        }
    }
    ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap()
}

fn round_trip(format: FormatId, img: &ImageBuf, opts: &[&str], meta: &Metadata) -> (iv_codec::Decoded, Vec<u8>) {
    let encoders = Encoders::default();
    let enc = encoders.for_format(format).unwrap_or_else(|| panic!("no encoder for {format}"));
    let opts = EncodeOptions::parse(enc.options(), &opts.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    let prepared = prepare(img, enc.info(), [255, 255, 255]);
    let bytes = enc.encode(&prepared, &opts, meta).unwrap();
    let name = format!("out.{}", format.canonical_extension());
    let decoded = default_registry().decode(&Source::from_bytes(name, bytes.clone()), &CancelToken::never()).unwrap();
    assert_eq!(decoded.info.format, format, "the output is recognised as {format}");
    (decoded, bytes)
}

fn psnr(a: &ImageBuf, b: &ImageBuf) -> f64 {
    let (a, b) = (a.to_rgba8(), b.to_rgba8());
    let mse: f64 = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .flat_map(|(p, q)| (0..3).map(move |c| (p[c] as f64 - q[c] as f64).powi(2)))
        .sum::<f64>()
        / (a.len() / 4 * 3) as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

#[test]
fn lossless_formats_are_exact() {
    let img = gradient(64, 48, true);
    for (format, opts) in [
        (FormatId::Png, vec![]),
        (FormatId::Webp, vec!["lossless=true"]),
        (FormatId::Jxl, vec!["lossless=true"]),
        // quality 100 is libjxl's lossless, as in cjxl
        (FormatId::Jxl, vec!["quality=100", "effort=3"]),
        (FormatId::Tiff, vec![]),
        (FormatId::Qoi, vec![]),
        (FormatId::Bmp, vec![]),
    ] {
        let (d, _) = round_trip(format, &img, &opts, &Metadata::default());
        assert_eq!(d.image.to_rgba8(), img.to_rgba8(), "{format}");
    }
}

#[test]
fn lossy_formats_stay_above_a_quality_floor() {
    let img = gradient(128, 96, false);
    for (format, opts, floor) in [
        (FormatId::Jpeg, vec!["quality=90"], 38.0),
        (FormatId::Webp, vec!["quality=85"], 35.0),
        (FormatId::Avif, vec!["quality=75", "speed=8"], 35.0),
    ] {
        let (d, _) = round_trip(format, &img, &opts, &Metadata::default());
        let p = psnr(&d.image, &img);
        assert!(p >= floor, "{format}: PSNR {p:.1} dB < {floor}");
    }
}

#[test]
fn quality_option_changes_size() {
    let img = gradient(128, 96, false);
    let (_, small) = round_trip(FormatId::Jpeg, &img, &["quality=20"], &Metadata::default());
    let (_, large) = round_trip(FormatId::Jpeg, &img, &["quality=98"], &Metadata::default());
    assert!(small.len() < large.len(), "{} vs {}", small.len(), large.len());
}

#[test]
fn transparency_survives_where_supported_and_is_flattened_elsewhere() {
    let img = gradient(32, 16, true);
    let (webp, _) = round_trip(FormatId::Webp, &img, &["quality=90"], &Metadata::default());
    assert!(webp.info.has_alpha);
    let left = webp.image.to_rgba8()[3];
    assert!(left < 10, "lossy WebP keeps alpha: {left}");

    let (jpeg, _) = round_trip(FormatId::Jpeg, &img, &["quality=95"], &Metadata::default());
    let px = &jpeg.image.to_rgba8()[..4];
    assert!(px[0] > 240 && px[1] > 240 && px[2] > 240 && px[3] == 255, "transparent pixel became white: {px:?}");
}

#[test]
fn sixteen_bit_survives_png_and_tiff() {
    let data: Vec<u16> = (0..16u16).flat_map(|i| [i * 4000 + 1, 65535 - i * 3000, 12345, 65535]).collect();
    let img = ImageBuf::new(4, 4, PixelFormat::Rgba16, bytemuck::cast_slice(&data).to_vec()).unwrap();
    for format in [FormatId::Png, FormatId::Tiff] {
        let (d, _) = round_trip(format, &img, &[], &Metadata::default());
        assert_eq!(d.image.format(), PixelFormat::Rgba16, "{format}");
        assert_eq!(d.image.data(), img.data(), "{format}");
    }
}

/// Minimal EXIF (TIFF) block with IFD0 orientation.
fn exif_with_orientation(o: u16) -> Vec<u8> {
    let mut t = b"II\x2A\x00".to_vec();
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&1u16.to_le_bytes());
    t.extend_from_slice(&0x0112u16.to_le_bytes());
    t.extend_from_slice(&3u16.to_le_bytes());
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&o.to_le_bytes());
    t.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    t
}

#[test]
fn icc_and_exif_are_embedded() {
    let img = gradient(32, 16, false);
    let icc = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let meta = Metadata { icc: Some(icc.clone()), exif: Some(exif_with_orientation(6)) };
    for (format, opts) in [
        (FormatId::Jpeg, vec![]),
        (FormatId::Png, vec![]),
        (FormatId::Webp, vec!["lossless=true"]),
        (FormatId::Webp, vec!["quality=80"]),
    ] {
        let (d, _) = round_trip(format, &img, &opts, &meta);
        assert_eq!(d.info.icc_profile.as_deref(), Some(icc.as_slice()), "{format} {opts:?}: ICC");
        assert!(d.color_managed, "{format}");
        if format == FormatId::Jpeg {
            assert_eq!(d.info.orientation, Orientation::Rotate90, "JPEG EXIF orientation");
        }
    }
}

/// Photo-like content: smooth gradients with fine deterministic texture, so lossy encoders have
/// detail to trade away (a plain gradient compresses to almost nothing at any quality).
fn photo(w: u32, h: u32) -> ImageBuf {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    let mut seed = 0x2545_f491u32;
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed % 24) as i32 - 12;
            let wave = (((x as f32 * 0.21).sin() + (y as f32 * 0.13).cos()) * 40.0) as i32;
            let c = |base: i32| (base + wave + noise).clamp(0, 255) as u8;
            data.extend_from_slice(&[c((x * 200 / w) as i32), c((y * 200 / h) as i32 + 30), c(110), 255]);
        }
    }
    ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap()
}

#[test]
fn lossy_jpeg_xl_trades_size_for_quality() {
    let img = photo(192, 128);
    let mut last = usize::MAX;
    // floors a little under what libjxl 0.12 gives on this image (34.7, 31.7, 31.0 dB)
    for (quality, floor) in [(90, 32.0), (75, 29.0), (50, 27.0)] {
        let q = format!("quality={quality}");
        let (d, bytes) = round_trip(FormatId::Jxl, &img, &[&q], &Metadata::default());
        let p = psnr(&d.image, &img);
        assert!(p >= floor, "quality {quality}: PSNR {p:.1} dB < {floor}");
        assert!(p.is_finite(), "quality {quality} is lossy");
        assert!(bytes.len() < last, "quality {quality}: {} bytes, not below {last}", bytes.len());
        last = bytes.len();
    }
    // lossless keeps every value and costs more than visually lossless
    let (exact, lossless) = round_trip(FormatId::Jxl, &img, &["lossless=true"], &Metadata::default());
    assert_eq!(exact.image.to_rgba8(), img.to_rgba8());
    let (_, q90) = round_trip(FormatId::Jxl, &img, &["quality=90"], &Metadata::default());
    assert!(lossless.len() > q90.len(), "lossless {} vs quality 90 {}", lossless.len(), q90.len());
}

#[test]
fn jpeg_xl_quality_follows_libjxl_distance() {
    use iv_codec::encode::jxl_distance_from_quality as d;
    assert_eq!(d(100.0), 0.0);
    assert!((d(90.0) - 1.0).abs() < 1e-6);
    assert!((d(75.0) - 2.35).abs() < 1e-6);
    assert!((d(30.0) - 6.4).abs() < 1e-5);
    assert!((d(0.0) - 25.0).abs() < 1e-6);
    // continuous where the two pieces meet, and rising as quality falls
    assert!((d(29.999) - d(30.0)).abs() < 0.01);
    assert!((1..100).all(|q| d(q as f32) > d(q as f32 + 1.0)));
}

#[test]
fn jpeg_xl_effort_is_validated_and_changes_the_encode() {
    let enc = Encoders::default();
    let jxl = enc.for_format(FormatId::Jxl).unwrap();
    assert!(EncodeOptions::parse(jxl.options(), &["effort=0".into()]).is_err());
    assert!(EncodeOptions::parse(jxl.options(), &["effort=10".into()]).is_err());
    let img = photo(96, 64);
    let (_, fast) = round_trip(FormatId::Jxl, &img, &["effort=1", "lossless=true"], &Metadata::default());
    let (_, slow) = round_trip(FormatId::Jxl, &img, &["effort=9", "lossless=true"], &Metadata::default());
    assert!(slow.len() < fast.len(), "effort 9 {} vs effort 1 {}", slow.len(), fast.len());
}

#[test]
fn jpeg_xl_keeps_alpha_lossy_and_lossless() {
    let img = gradient(64, 32, true);
    for opts in [vec!["lossless=true"], vec!["quality=90"], vec!["quality=50"]] {
        let (d, _) = round_trip(FormatId::Jxl, &img, &opts, &Metadata::default());
        assert!(d.info.has_alpha, "{opts:?}");
        let (a, b) = (d.image.to_rgba8(), img.to_rgba8());
        let worst = a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0).map(|(p, q)| p[3].abs_diff(q[3])).max();
        let allowed = if opts[0] == "lossless=true" { 0 } else { 4 };
        assert!(worst.unwrap() <= allowed, "{opts:?}: alpha off by {worst:?}");
    }
}

#[test]
fn jpeg_xl_keeps_sixteen_bits_lossy_and_lossless() {
    // a 16-bit ramp whose low bits matter: 8-bit storage would round them away
    let (w, h) = (32u32, 16u32);
    let data: Vec<u16> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = ((i % w) as u16, (i / w) as u16);
            [x * 2000 + 7, y * 4000 + 3, 30000 + x * 13, 65535 - y * 1000]
        })
        .collect();
    let img = ImageBuf::new(w, h, PixelFormat::Rgba16, bytemuck::cast_slice(&data).to_vec()).unwrap();
    let (exact, _) = round_trip(FormatId::Jxl, &img, &["lossless=true"], &Metadata::default());
    assert_eq!(exact.image.format(), PixelFormat::Rgba16);
    assert_eq!(exact.image.data(), img.data(), "lossless 16-bit is exact");

    let (lossy, _) = round_trip(FormatId::Jxl, &img, &["quality=95"], &Metadata::default());
    assert_eq!(lossy.image.format(), PixelFormat::Rgba16, "lossy keeps the depth");
    assert!(lossy.info.has_alpha);
    let got: &[u16] = bytemuck::cast_slice(lossy.image.data());
    let mse = got.iter().zip(&data).map(|(&a, &b)| (a as f64 - b as f64).powi(2)).sum::<f64>() / data.len() as f64;
    let psnr16 = 10.0 * (65535.0f64 * 65535.0 / mse).log10();
    assert!(psnr16 > 40.0, "16-bit lossy PSNR {psnr16:.1} dB");
    // values that 8-bit storage cannot hold come back (not multiples of 257)
    assert!(got.iter().filter(|v| **v % 257 != 0).count() > got.len() / 2);
}

/// The payload of the first box of `ty` in a JPEG XL container, if it is stored uncompressed.
fn jxl_box<'a>(file: &'a [u8], ty: &[u8; 4]) -> Option<&'a [u8]> {
    let mut at = 0;
    while at + 8 <= file.len() {
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        let end = if size == 0 { file.len() } else { at + size };
        if &file[at + 4..at + 8] == ty {
            return file.get(at + 8..end);
        }
        if size < 8 {
            return None;
        }
        at = end;
    }
    None
}

#[test]
fn jpeg_xl_embeds_icc_and_exif() {
    let img = gradient(32, 16, false);
    let icc = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let exif = exif_with_orientation(6);
    let meta = Metadata { icc: Some(icc.clone()), exif: Some(exif.clone()) };
    let primaries = |p: &[u8]| {
        let p = moxcms::ColorProfile::new_from_slice(p).unwrap();
        [p.red_colorant, p.green_colorant, p.blue_colorant].map(|c| [c.x, c.y, c.z])
    };
    for opts in [vec!["lossless=true"], vec!["quality=80"]] {
        let (d, bytes) = round_trip(FormatId::Jxl, &img, &opts, &meta);
        // libjxl stores a profile it recognises (Display P3) as the colour space's code, and the
        // decoder hands back an equivalent profile of its own: the same primaries, other bytes
        let got = primaries(d.info.icc_profile.as_deref().expect("a colour profile"));
        let want = primaries(&icc);
        for (g, w) in got.iter().flatten().zip(want.iter().flatten()) {
            assert!((g - w).abs() < 2e-3, "{opts:?}: primaries {got:?} vs {want:?}");
        }
        assert!(d.color_managed, "{opts:?}");
        assert!(bytes.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n"), "{opts:?}: EXIF needs the container");
        // the box is the 4-byte TIFF header offset, then the EXIF block
        let boxed = jxl_box(&bytes, b"Exif").expect("Exif box");
        assert_eq!(&boxed[4..], exif.as_slice(), "{opts:?}");
    }
    // a profile with a sampled tone curve (here gamma 2.0 on sRGB primaries) has no code: kept
    // byte for byte, and the lossy encode converts through it (libjxl's colour management)
    let mut sampled = moxcms::ColorProfile::new_srgb();
    let lut: Vec<u16> = (0..256u32).map(|i| ((i as f64 / 255.0).powf(2.0) * 65535.0).round() as u16).collect();
    for trc in [&mut sampled.red_trc, &mut sampled.green_trc, &mut sampled.blue_trc] {
        *trc = Some(moxcms::ToneReprCurve::Lut(lut.clone()));
    }
    let sampled = sampled.encode().unwrap();
    let meta = Metadata { icc: Some(sampled.clone()), exif: None };
    // lossless: the pixels and the profile as they were
    let (exact, bytes) = round_trip(FormatId::Jxl, &img, &["lossless=true"], &meta);
    assert_eq!(exact.info.icc_profile.as_deref(), Some(sampled.as_slice()));
    let stored =
        default_registry().decode_unmanaged(&Source::from_bytes("a.jxl", bytes), &CancelToken::never()).unwrap();
    assert_eq!(stored.image.to_rgba8(), img.to_rgba8());
    // lossy: the file still names the original profile, and jxl-oxide renders the XYB pixels to
    // sRGB; they match the input taken through the gamma 2.0 curve to sRGB
    let (lossy, bytes) = round_trip(FormatId::Jxl, &img, &["quality=95"], &meta);
    let file = jxl_oxide::JxlImage::builder().read(&bytes[..]).unwrap();
    assert_eq!(file.original_icc(), Some(sampled.as_slice()));
    let to_srgb = |v: u8| {
        let l = (v as f64 / 255.0).powi(2);
        let s = if l <= 0.003_130_8 { 12.92 * l } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
        (s * 255.0).round() as u8
    };
    let expected: Vec<u8> =
        img.to_rgba8().iter().enumerate().map(|(i, &v)| if i % 4 == 3 { v } else { to_srgb(v) }).collect();
    let expected = ImageBuf::new(img.width(), img.height(), PixelFormat::Rgba8, expected).unwrap();
    let p = psnr(&lossy.image, &expected);
    assert!(p >= 35.0, "lossy pixels follow the profile: PSNR {p:.1}");
    // without metadata: a bare codestream, sRGB
    let (_, bytes) = round_trip(FormatId::Jxl, &img, &["quality=80"], &Metadata::default());
    assert!(bytes.starts_with(&[0xFF, 0x0A]), "bare codestream");
    // a CMYK profile does not describe RGB pixels: left out instead of failing the encode
    let mut cmyk = icc.clone();
    cmyk[16..20].copy_from_slice(b"CMYK");
    let (_, bytes) = round_trip(FormatId::Jxl, &img, &["quality=80"], &Metadata { icc: Some(cmyk), exif: None });
    assert!(bytes.starts_with(&[0xFF, 0x0A]));
}
