//! End-to-end decoding through the default registry, on images encoded at test time.

use std::io::Cursor;
use std::sync::Arc;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use iv_codec::{Decoded, Decoder, DecoderInfo, Registry, default_registry};
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Orientation, PixelFormat, Result, Source};

/// A small image with a distinct value in every pixel and channel, so any channel swap,
/// flip or stride bug shows up.
fn pattern(w: u32, h: u32, alpha: bool) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        let a = if alpha { ((x * 7 + y * 13) % 256) as u8 } else { 255 };
        Rgba([(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, ((x + y) % 256) as u8, a])
    })
}

fn encode(img: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

fn decode(name: &str, bytes: Vec<u8>) -> Decoded {
    default_registry().decode(&Source::from_bytes(name, bytes), &CancelToken::never()).unwrap()
}

fn assert_exact(decoded: &Decoded, expected: &RgbaImage) {
    assert_eq!(decoded.image.format(), PixelFormat::Rgba8);
    assert_eq!((decoded.image.width(), decoded.image.height()), expected.dimensions());
    assert_eq!(decoded.image.data(), expected.as_raw().as_slice());
}

#[test]
fn lossless_formats_round_trip_exactly() {
    let rgba = pattern(37, 23, true);
    let rgb = pattern(37, 23, false);
    let cases: &[(&str, ImageFormat, &RgbaImage)] = &[
        ("a.png", ImageFormat::Png, &rgba),
        ("a.webp", ImageFormat::WebP, &rgba),
        ("a.qoi", ImageFormat::Qoi, &rgba),
        ("a.tiff", ImageFormat::Tiff, &rgba),
        ("a.tga", ImageFormat::Tga, &rgba),
        ("a.bmp", ImageFormat::Bmp, &rgba),
        ("a.ico", ImageFormat::Ico, &rgba),
        ("a.ppm", ImageFormat::Pnm, &rgb),
    ];
    for (name, format, expected) in cases {
        let img = if *format == ImageFormat::Pnm {
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8((*expected).clone()).to_rgb8())
        } else {
            DynamicImage::ImageRgba8((*expected).clone())
        };
        let d = decode(name, encode(&img, *format));
        assert_exact(&d, expected);
        assert_eq!(d.info.width, 37, "{name}");
        assert_eq!(d.info.frame_count, Some(1), "{name}");
    }
}

#[test]
fn jpeg_uses_zune_and_is_close_to_source() {
    let src = pattern(64, 48, false);
    let bytes = encode(&DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(src.clone()).to_rgb8()), ImageFormat::Jpeg);
    let d = decode("photo.jpg", bytes);

    assert_eq!(d.info.codec, "zune-jpeg");
    assert_eq!(d.info.format, FormatId::Jpeg);
    assert_eq!((d.image.width(), d.image.height()), (64, 48));
    assert!(!d.info.has_alpha);

    // lossy: compare mean absolute error on colour channels; alpha must be opaque
    let mut err = 0u64;
    for (got, want) in d.image.data().chunks_exact(4).zip(src.as_raw().chunks_exact(4)) {
        assert_eq!(got[3], 255);
        err += (0..3).map(|c| (got[c] as i32 - want[c] as i32).unsigned_abs() as u64).sum::<u64>();
    }
    let mae = err as f64 / (64.0 * 48.0 * 3.0);
    assert!(mae < 6.0, "mean abs error {mae}");
}

#[test]
fn sixteen_bit_png_keeps_precision() {
    let img = image::ImageBuffer::<Rgba<u16>, _>::from_fn(5, 3, |x, y| {
        Rgba([x as u16 * 1000 + 1, y as u16 * 3000 + 7, 65535, 40000])
    });
    let d = decode("deep.png", encode(&DynamicImage::ImageRgba16(img.clone()), ImageFormat::Png));
    assert_eq!(d.image.format(), PixelFormat::Rgba16);
    assert_eq!(d.info.bits_per_channel, 16);
    let got: &[u16] = bytemuck::cast_slice(d.image.data());
    assert_eq!(got, img.as_raw().as_slice());
}

#[test]
fn content_wins_over_wrong_extension() {
    let png = encode(&DynamicImage::ImageRgba8(pattern(8, 8, true)), ImageFormat::Png);
    let src = Source::from_bytes("actually-png.jpg", png);
    let reg = default_registry();

    let id = reg.identify(&src).unwrap();
    assert_eq!(id.format, FormatId::Png);
    assert_eq!(id.extension_format, Some(FormatId::Jpeg));
    assert!(id.extension_mismatch());

    let d = reg.decode(&src, &CancelToken::never()).unwrap();
    assert_eq!(d.info.format, FormatId::Png);
}

#[test]
fn tga_is_identified_by_extension() {
    let tga = encode(&DynamicImage::ImageRgba8(pattern(9, 4, true)), ImageFormat::Tga);
    let reg = default_registry();
    let id = reg.identify(&Source::from_bytes("x.tga", tga.clone())).unwrap();
    assert_eq!(id.format, FormatId::Tga);
    assert!(!id.by_content);
    assert!(matches!(reg.identify(&Source::from_bytes("x.bin", tga)), Err(Error::UnsupportedFormat)));
}

#[test]
fn probe_reads_dimensions_without_full_decode() {
    let png = encode(&DynamicImage::ImageRgba8(pattern(300, 200, true)), ImageFormat::Png);
    let info = default_registry().probe(&Source::from_bytes("big.png", png)).unwrap();
    assert_eq!((info.width, info.height), (300, 200));
    assert!(info.has_alpha);
    assert_eq!(info.orientation, Orientation::Normal);
}

#[test]
fn corrupt_data_is_an_error_not_a_panic() {
    let mut png = encode(&DynamicImage::ImageRgba8(pattern(50, 50, true)), ImageFormat::Png);
    png.truncate(png.len() / 2);
    let reg = default_registry();
    assert!(reg.decode(&Source::from_bytes("cut.png", png), &CancelToken::never()).is_err());

    let mut jpg = encode(&DynamicImage::ImageRgb8(image::RgbImage::new(50, 50)), ImageFormat::Jpeg);
    jpg.truncate(20);
    assert!(reg.decode(&Source::from_bytes("cut.jpg", jpg), &CancelToken::never()).is_err());

    assert!(
        reg.decode(&Source::from_bytes("garbage.png", vec![0x89, b'P', b'N', b'G']), &CancelToken::never()).is_err()
    );
}

#[test]
fn cancelled_token_stops_decode() {
    let png = encode(&DynamicImage::ImageRgba8(pattern(16, 16, true)), ImageFormat::Png);
    let cancel = CancelToken::new();
    cancel.cancel();
    let r = default_registry().decode(&Source::from_bytes("a.png", png), &cancel);
    assert!(matches!(r, Err(Error::Cancelled)));
}

#[test]
fn formats_without_a_decoder_report_no_decoder() {
    // a minimal AVIF ftyp box: identified, but nothing is registered for AVIF yet
    let mut avif = 24u32.to_be_bytes().to_vec();
    avif.extend_from_slice(b"ftypavif\0\0\0\0mif1miaf");
    let r = default_registry().decode(&Source::from_bytes("a.avif", avif), &CancelToken::never());
    match r {
        Err(Error::NoDecoder { format, .. }) => assert_eq!(format, FormatId::Avif),
        other => panic!("expected NoDecoder, got {other:?}"),
    }
}

/// Always fails; registered above the real decoder to check that the chain falls through.
struct Broken(DecoderInfo);

impl Decoder for Broken {
    fn info(&self) -> &DecoderInfo {
        &self.0
    }
    fn probe(&self, _: &Source, _: FormatId) -> Result<ImageInfo> {
        Err(Error::decode("broken", "always fails"))
    }
    fn decode(&self, _: &Source, _: FormatId, _: &CancelToken) -> Result<Decoded> {
        Err(Error::decode("broken", "always fails"))
    }
}

#[test]
fn failing_decoder_falls_through_to_next_priority() {
    let mut reg = default_registry();
    reg.register(Arc::new(Broken(DecoderInfo {
        id: "broken",
        name: "broken",
        formats: &[FormatId::Png],
        priority: 1000,
    })));
    assert_eq!(reg.decoders_for(FormatId::Png).next().unwrap().info().id, "broken");

    let png = encode(&DynamicImage::ImageRgba8(pattern(4, 4, true)), ImageFormat::Png);
    let d = reg.decode(&Source::from_bytes("a.png", png), &CancelToken::never()).unwrap();
    assert_eq!(d.info.codec, "image-rs");

    // only the broken decoder for the format: its error is reported
    let mut only_broken = Registry::new();
    only_broken.register(Arc::new(Broken(DecoderInfo {
        id: "broken",
        name: "broken",
        formats: &[FormatId::Png],
        priority: 0,
    })));
    let png = encode(&DynamicImage::ImageRgba8(pattern(4, 4, true)), ImageFormat::Png);
    match only_broken.decode(&Source::from_bytes("a.png", png), &CancelToken::never()) {
        Err(Error::NoDecoder { reason, .. }) => assert!(reason.contains("always fails"), "{reason}"),
        other => panic!("expected NoDecoder, got {other:?}"),
    }
}

#[test]
fn file_sources_decode_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.png");
    let expected = pattern(20, 10, true);
    std::fs::write(&path, encode(&DynamicImage::ImageRgba8(expected.clone()), ImageFormat::Png)).unwrap();
    let d = default_registry().decode(&Source::from_path(&path), &CancelToken::never()).unwrap();
    assert_exact(&d, &expected);
}

/// Inserts an APP1 EXIF segment (IFD0 orientation + IFD1 JPEG thumbnail) after the SOI marker.
fn with_exif(jpeg: &[u8], orientation: u16, thumbnail: &[u8]) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II\x2A\x00");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    // IFD0 @8: one entry, then the offset of IFD1 (@26)
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&26u32.to_le_bytes());
    // IFD1 @26: thumbnail offset and length; thumbnail data follows @56
    let thumb_offset = 56u32;
    tiff.extend_from_slice(&2u16.to_le_bytes());
    for (tag, value) in [(0x0201u16, thumb_offset), (0x0202u16, thumbnail.len() as u32)] {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(tiff.len(), thumb_offset as usize);
    tiff.extend_from_slice(thumbnail);

    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn jpeg_of(w: u32, h: u32) -> Vec<u8> {
    let rgb = DynamicImage::ImageRgba8(pattern(w, h, false)).to_rgb8();
    encode(&DynamicImage::ImageRgb8(rgb), ImageFormat::Jpeg)
}

#[test]
fn jpeg_exif_orientation_and_thumbnail_preview() {
    let full = with_exif(&jpeg_of(120, 80), 6, &jpeg_of(24, 16));
    let src = Source::from_bytes("rotated.jpg", full);
    let reg = default_registry();

    let info = reg.probe(&src).unwrap();
    assert_eq!(info.orientation, Orientation::Rotate90);
    assert_eq!((info.width, info.height), (120, 80), "stored size");
    assert_eq!(info.display_size(), (80, 120));

    let preview = reg.preview(&src, 512, &CancelToken::never()).unwrap().expect("embedded thumbnail");
    assert_eq!((preview.width(), preview.height()), (24, 16));

    let d = reg.decode(&src, &CancelToken::never()).unwrap();
    assert_eq!((d.image.width(), d.image.height()), (120, 80), "pixels stay in stored layout");
    assert_eq!(d.info.orientation, Orientation::Rotate90);
}

#[test]
fn jpeg_without_thumbnail_has_no_preview() {
    let src = Source::from_bytes("plain.jpg", jpeg_of(40, 30));
    assert!(default_registry().preview(&src, 512, &CancelToken::never()).unwrap().is_none());
}

#[test]
fn image_buf_shares_pixels_on_clone() {
    let d = decode("a.png", encode(&DynamicImage::ImageRgba8(pattern(4, 4, true)), ImageFormat::Png));
    let copy: ImageBuf = d.image.clone();
    assert_eq!(copy.data().as_ptr(), d.image.data().as_ptr());
}

#[test]
fn gif_decodes_exactly_when_the_palette_fits() {
    // four colours fit a GIF palette, so the round trip is exact
    let colours = [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]];
    let expected = RgbaImage::from_fn(12, 7, |x, y| Rgba(colours[((x / 3 + y) % 4) as usize]));
    let d = decode("a.gif", encode(&DynamicImage::ImageRgba8(expected.clone()), ImageFormat::Gif));
    assert_exact(&d, &expected);
    assert_eq!(d.info.format, FormatId::Gif);
    assert_eq!(d.info.frame_count, None, "GIF frame count needs a full scan");
}
