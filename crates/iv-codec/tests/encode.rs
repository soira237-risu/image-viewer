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
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
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
        (FormatId::Jxl, vec![]),
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
