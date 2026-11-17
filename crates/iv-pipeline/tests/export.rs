//! Saving and converting through the export path, on real files.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageEncoder, ImageFormat, Rgba, RgbaImage};
use iv_codec::encode::{EncodeOptions, Encoders};
use iv_codec::{default_registry, exif_edit};
use iv_core::{CancelToken, FormatId, Orientation, Source};
use iv_ops::{Op, Rect};
use iv_pipeline::export::{ExportOutcome, ExportRequest, export, fix_extension, write_atomic};

/// 6x4 image, every pixel distinct.
fn pattern() -> RgbaImage {
    RgbaImage::from_fn(6, 4, |x, y| Rgba([(x * 40) as u8, (y * 60) as u8, 100, 255]))
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn encode(img: &RgbaImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    let d = if format == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(img.clone()).to_rgb8())
    } else {
        DynamicImage::ImageRgba8(img.clone())
    };
    d.write_to(&mut out, format).unwrap();
    out.into_inner()
}

fn request(source: &Path, dest: &Path, format: FormatId, ops: Vec<Op>) -> ExportRequest {
    ExportRequest {
        source: source.to_path_buf(),
        ops,
        format,
        options: EncodeOptions::default(),
        dest: dest.to_path_buf(),
        background: [255, 255, 255],
        keep_metadata: true,
    }
}

fn run(req: &ExportRequest) -> ExportOutcome {
    export(&default_registry(), &Encoders::default(), req).unwrap()
}

/// Everything from the first start-of-scan marker on: the compressed image data.
fn scan_data(jpeg: &[u8]) -> &[u8] {
    let i = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    &jpeg[i..]
}

#[test]
fn rotating_a_jpeg_is_lossless() {
    let dir = tempfile::tempdir().unwrap();
    let original =
        encode(&RgbaImage::from_fn(64, 32, |x, y| Rgba([x as u8 * 4, y as u8 * 8, 77, 255])), ImageFormat::Jpeg);
    let src = write(dir.path(), "photo.jpg", &original);
    let dest = dir.path().join("rotated.jpg");

    assert_eq!(run(&request(&src, &dest, FormatId::Jpeg, vec![Op::rotate_cw()])), ExportOutcome::LosslessOrientation);
    let out = std::fs::read(&dest).unwrap();
    assert_eq!(scan_data(&out), scan_data(&original), "compressed data copied byte for byte");

    let reg = default_registry();
    let info = reg.probe(&Source::from_path(&dest)).unwrap();
    assert_eq!(info.orientation, Orientation::Rotate90);
    assert_eq!(info.display_size(), (32, 64));
    let a = reg.decode(&Source::from_path(&src), &CancelToken::never()).unwrap();
    let b = reg.decode(&Source::from_path(&dest), &CancelToken::never()).unwrap();
    assert_eq!(a.image.data(), b.image.data(), "same stored pixels, only the orientation differs");
}

/// `jpeg` with a location in its EXIF, XMP, an ICC profile, a comment and data after its end.
fn with_private_metadata(jpeg: &[u8]) -> Vec<u8> {
    let segment = |marker: u8, body: &[u8]| {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(body);
        s
    };
    // little-endian TIFF: orientation 1 and a GPS IFD holding a map datum
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend_from_slice(&2u16.to_le_bytes());
    tiff.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
    tiff.extend_from_slice(&[0x25, 0x88, 4, 0, 1, 0, 0, 0, 38, 0, 0, 0]);
    tiff.extend_from_slice(&[0; 4]);
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&[0x12, 0, 2, 0, 11, 0, 0, 0, 56, 0, 0, 0]);
    tiff.extend_from_slice(&[0; 4]);
    tiff.extend_from_slice(b"SECRET-GPS\0");
    let exif = [b"Exif\0\0".as_slice(), &tiff].concat();
    // after SOI and the encoder's JFIF segment
    assert_eq!(jpeg[2..4], [0xFF, 0xE0]);
    let at = 4 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
    [
        &jpeg[..at],
        &segment(0xE1, &exif),
        &segment(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>SECRET-XMP"),
        &segment(0xE2, b"ICC_PROFILE\0\x01\x01SECRET-ICC"),
        &segment(0xFE, b"SECRET-COMMENT"),
        &jpeg[at..],
        b"SECRET-APPENDED",
    ]
    .concat()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn rotating_a_jpeg_without_metadata_strips_it_losslessly() {
    let dir = tempfile::tempdir().unwrap();
    let plain =
        encode(&RgbaImage::from_fn(64, 32, |x, y| Rgba([x as u8 * 4, y as u8 * 8, 77, 255])), ImageFormat::Jpeg);
    let original = with_private_metadata(&plain);
    let src = write(dir.path(), "located.jpg", &original);
    let info = default_registry().probe(&Source::from_path(&src)).unwrap();
    assert!(contains(info.exif.as_deref().unwrap(), b"SECRET-GPS"), "the test file has a location");

    // `iv convert --to jpg --no-metadata`, with and without a rotation
    for (ops, orientation) in [(vec![Op::rotate_cw()], Orientation::Rotate90), (vec![], Orientation::Normal)] {
        let dest = dir.path().join("shared.jpg");
        let mut req = request(&src, &dest, FormatId::Jpeg, ops);
        req.keep_metadata = false;
        assert_eq!(run(&req), ExportOutcome::LosslessOrientation);
        let out = std::fs::read(&dest).unwrap();
        assert!(!contains(&out, b"SECRET"), "metadata left in the output");
        assert_eq!(scan_data(&out), scan_data(&plain), "compressed data copied byte for byte");
        let info = default_registry().probe(&Source::from_path(&dest)).unwrap();
        assert_eq!(info.orientation, orientation);
        assert!(info.icc_profile.is_none());
    }

    // keeping metadata keeps it
    let dest = dir.path().join("kept.jpg");
    assert_eq!(run(&request(&src, &dest, FormatId::Jpeg, vec![Op::rotate_cw()])), ExportOutcome::LosslessOrientation);
    assert!(contains(&std::fs::read(&dest).unwrap(), b"SECRET-GPS"));
}

#[test]
fn rotation_composes_with_existing_exif_orientation() {
    let dir = tempfile::tempdir().unwrap();
    let tagged = exif_edit::jpeg_with_orientation(&encode(&pattern(), ImageFormat::Jpeg), 6).unwrap();
    let src = write(dir.path(), "tagged.jpg", &tagged);
    let dest = dir.path().join("upright.jpg");
    // shown rotated 90° clockwise; the user turns it back
    run(&request(&src, &dest, FormatId::Jpeg, vec![Op::rotate_ccw()]));
    let info = default_registry().probe(&Source::from_path(&dest)).unwrap();
    assert_eq!(info.orientation, Orientation::Normal);
}

#[test]
fn crop_and_rotate_are_baked_into_converted_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "p.png", &encode(&pattern(), ImageFormat::Png));
    let dest = dir.path().join("out.png");
    // rotate the 6x4 image to 4x6, keep its top-left 2x3
    let ops = vec![Op::rotate_cw(), Op::Crop(Rect { x: 0, y: 0, width: 2, height: 3 })];
    assert!(matches!(run(&request(&src, &dest, FormatId::Png, ops)), ExportOutcome::Encoded { dropped_frames: 0, .. }));
    let d = default_registry().decode(&Source::from_path(&dest), &CancelToken::never()).unwrap();
    assert_eq!((d.image.width(), d.image.height()), (2, 3));
    assert_eq!(d.info.orientation, Orientation::Normal);
    // top-left of the rotated image is the stored bottom-left pixel (0, 3)
    assert_eq!(&d.image.data()[..4], pattern().get_pixel(0, 3).0.as_slice());
}

#[test]
fn conversion_keeps_pixels_and_the_icc_profile() {
    let dir = tempfile::tempdir().unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let mut png = Vec::new();
    let mut enc = image::codecs::png::PngEncoder::new(&mut png);
    enc.set_icc_profile(p3.clone()).unwrap();
    enc.write_image(pattern().as_raw(), 6, 4, image::ExtendedColorType::Rgba8).unwrap();
    let src = write(dir.path(), "wide.png", &png);
    let dest = dir.path().join("wide.webp");

    let mut req = request(&src, &dest, FormatId::Webp, vec![]);
    req.options = EncodeOptions::parse(
        Encoders::default().for_format(FormatId::Webp).unwrap().options(),
        &["lossless=true".into()],
    )
    .unwrap();
    run(&req);
    let reg = default_registry();
    let out = reg.decode_unmanaged(&Source::from_path(&dest), &CancelToken::never()).unwrap();
    assert_eq!(out.info.format, FormatId::Webp);
    assert_eq!(out.info.icc_profile.as_deref(), Some(p3.as_slice()), "profile carried over");
    // saved in its own colour space, not converted to sRGB on the way
    let rgb = |d: &[u8]| d.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]).collect::<Vec<_>>();
    assert_eq!(rgb(out.image.data()), rgb(pattern().as_raw()));
}

#[test]
fn exif_orientation_is_reset_when_pixels_are_rotated() {
    let dir = tempfile::tempdir().unwrap();
    let tagged = exif_edit::jpeg_with_orientation(&encode(&pattern(), ImageFormat::Jpeg), 6).unwrap();
    let src = write(dir.path(), "tagged.jpg", &tagged);
    let dest = dir.path().join("cropped.jpg");
    // a crop forces re-encoding, so the EXIF rotation has to be baked in
    run(&request(&src, &dest, FormatId::Jpeg, vec![Op::Crop(Rect { x: 0, y: 0, width: 4, height: 5 })]));
    let info = default_registry().probe(&Source::from_path(&dest)).unwrap();
    assert_eq!(info.orientation, Orientation::Normal, "orientation tag rewritten to 1");
    assert_eq!((info.width, info.height), (4, 5), "stored pixels are already upright");
}

#[test]
fn saving_over_the_source_works() {
    let dir = tempfile::tempdir().unwrap();
    let src = write(dir.path(), "same.png", &encode(&pattern(), ImageFormat::Png));
    run(&request(&src, &src, FormatId::Png, vec![Op::flip_h()]));
    let d = default_registry().decode(&Source::from_path(&src), &CancelToken::never()).unwrap();
    assert_eq!(&d.image.data()[..4], pattern().get_pixel(5, 0).0.as_slice());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn a_failed_write_leaves_the_original_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let dest = write(dir.path(), "keep.png", b"original bytes");
    let r = write_atomic(&dest, |f| {
        f.write_all(b"half of the new")?;
        Err(std::io::Error::other("disk full"))
    });
    assert!(r.is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"original bytes");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file left behind");
}

#[test]
fn overlapping_writes_to_one_file_never_share_a_temporary_file() {
    // a second save of the same file starts while the first is still writing (a save running
    // on a thread, or two batch inputs that map to one output)
    let dir = tempfile::tempdir().unwrap();
    let dest = write(dir.path(), "same.png", b"original");
    let outer = write_atomic(&dest, |f| {
        f.write_all(b"first save, ")?;
        let inner = write_atomic(&dest, |g| g.write_all(b"second save"));
        assert!(inner.is_ok(), "{inner:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"second save");
        f.write_all(b"complete")
    });
    assert!(outer.is_ok(), "{outer:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"first save, complete", "one whole result, not a mix");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(names, ["same.png"], "no temporary file left behind");

    // threads racing on one destination: every write succeeds and the file is one of them
    let contents: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 64 * 1024]).collect();
    std::thread::scope(|s| {
        for c in &contents {
            let dest = &dest;
            s.spawn(move || write_atomic(dest, |f| f.write_all(c)).unwrap());
        }
    });
    let result = std::fs::read(&dest).unwrap();
    assert!(contents.contains(&result), "the file is exactly one of the writes");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn saving_a_misnamed_file_in_its_content_format_keeps_alpha() {
    // a PNG with transparency named .jpg, saved over itself as what it really is
    let dir = tempfile::tempdir().unwrap();
    let mut img = pattern();
    img.put_pixel(0, 0, Rgba([1, 2, 3, 0]));
    let src = write(dir.path(), "really-png.jpg", &encode(&img, ImageFormat::Png));
    run(&request(&src, &src, FormatId::Png, vec![Op::flip_h()]));
    let reg = default_registry();
    let id = reg.identify(&Source::from_path(&src)).unwrap();
    assert_eq!(id.format, FormatId::Png, "still PNG bytes under the old name");
    let d = reg.decode(&Source::from_path(&src), &CancelToken::never()).unwrap();
    assert!(d.info.has_alpha);
    // flipped: the transparent pixel is now top right
    assert_eq!(d.image.data()[(5 * 4) + 3], 0, "transparency kept");
}

#[test]
fn animated_sources_report_dropped_frames() {
    let dir = tempfile::tempdir().unwrap();
    let mut gif = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
        for c in [10u8, 200] {
            enc.encode_frame(image::Frame::new(RgbaImage::from_pixel(4, 4, Rgba([c, c, c, 255])))).unwrap();
        }
    }
    let src = write(dir.path(), "anim.gif", &gif);
    let outcome = run(&request(&src, &dir.path().join("still.png"), FormatId::Png, vec![]));
    assert!(matches!(outcome, ExportOutcome::Encoded { dropped_frames: 1, .. }), "{outcome:?}");
}

#[test]
fn wrong_extensions_are_fixed_without_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let reg = default_registry();
    let png_named_jpg = write(dir.path(), "really-png.jpg", &encode(&pattern(), ImageFormat::Png));
    let fixed = fix_extension(&reg, &png_named_jpg).unwrap().unwrap();
    assert_eq!(fixed.file_name().unwrap(), "really-png.png");
    assert!(fixed.exists() && !png_named_jpg.exists());
    assert_eq!(fix_extension(&reg, &fixed).unwrap(), None, "already right");

    let clash = write(dir.path(), "clash.jpg", &encode(&pattern(), ImageFormat::Png));
    write(dir.path(), "clash.png", b"someone else's file");
    assert!(fix_extension(&reg, &clash).is_err());
    assert_eq!(std::fs::read(dir.path().join("clash.png")).unwrap(), b"someone else's file");
}

#[test]
fn archive_pages_decode_and_can_be_saved_as_files() {
    use std::fs::File;
    let dir = tempfile::tempdir().unwrap();
    let cbz = dir.path().join("book.cbz");
    {
        let mut w = zip::ZipWriter::new(File::create(&cbz).unwrap());
        for (name, px) in [("p10.png", pattern()), ("p2.png", pattern())] {
            w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(&encode(&px, ImageFormat::Png)).unwrap();
        }
        w.finish().unwrap();
    }
    let pages = iv_pipeline::archive::list_images(&cbz).unwrap();
    assert_eq!(
        pages.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>(),
        ["p2.png", "p10.png"]
    );

    let folder = iv_pipeline::FolderList::for_file(&pages[1]).unwrap();
    assert_eq!(folder.len(), 2);
    assert_eq!(folder.position(&pages[1]), Some(1));

    let dest = dir.path().join("page.webp");
    run(&request(&pages[0], &dest, FormatId::Webp, vec![Op::rotate_cw()]));
    let d = default_registry().decode(&Source::from_path(&dest), &CancelToken::never()).unwrap();
    assert_eq!((d.image.width(), d.image.height()), (4, 6));
    assert!(fix_extension(&default_registry(), &pages[0]).is_err(), "archive pages cannot be renamed");
}

// --- JPEG XL: lossless JPEG recompression, the rebuilt original, and the fallbacks ---

/// A photo-like JPEG (gradients with fine texture) at quality 90.
fn photo_jpeg(w: u32, h: u32) -> Vec<u8> {
    let mut seed = 0x9e37_79b9u32;
    let img = RgbaImage::from_fn(w, h, |x, y| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let n = (seed % 20) as i32 - 10;
        let wave = (((x as f32 * 0.17).sin() + (y as f32 * 0.11).cos()) * 35.0) as i32;
        let c = |b: i32| (b + wave + n).clamp(0, 255) as u8;
        Rgba([c((x * 220 / w) as i32), c((y * 180 / h) as i32 + 20), c(120), 255])
    });
    let mut out = Vec::new();
    let rgb = DynamicImage::ImageRgba8(img).to_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
    let mut s = vec![0xFF, marker];
    s.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    s.extend_from_slice(body);
    s
}

/// `jpeg` with EXIF (orientation 1 and a location), XMP, a real ICC profile and a comment, all
/// carrying markers a test can look for.
fn with_real_metadata(jpeg: &[u8], icc: &[u8]) -> Vec<u8> {
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend_from_slice(&2u16.to_le_bytes());
    tiff.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
    tiff.extend_from_slice(&[0x25, 0x88, 4, 0, 1, 0, 0, 0, 38, 0, 0, 0]);
    tiff.extend_from_slice(&[0; 4]);
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&[0x12, 0, 2, 0, 11, 0, 0, 0, 56, 0, 0, 0]);
    tiff.extend_from_slice(&[0; 4]);
    tiff.extend_from_slice(b"SECRET-GPS\0");
    let icc_body = [b"ICC_PROFILE\0\x01\x01".as_slice(), icc].concat();
    assert_eq!(jpeg[2..4], [0xFF, 0xE0]);
    let at = 4 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
    [
        &jpeg[..at],
        &segment(0xE1, &[b"Exif\0\0".as_slice(), &tiff].concat()),
        &segment(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>SECRET-XMP</x:xmpmeta>"),
        &segment(0xE2, &icc_body),
        &segment(0xFE, b"SECRET-COMMENT"),
        &jpeg[at..],
    ]
    .concat()
}

fn jxl_request(source: &Path, dest: &Path, ops: Vec<Op>, opts: &[&str]) -> ExportRequest {
    let mut req = request(source, dest, FormatId::Jxl, ops);
    let enc = Encoders::default();
    let options = enc.for_format(FormatId::Jxl).unwrap().options();
    req.options = EncodeOptions::parse(options, &opts.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    req
}

/// Mean squared error over the colour channels, as PSNR in dB.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let (sum, n) = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .flat_map(|(p, q)| (0..3).map(move |c| (p[c] as f64 - q[c] as f64).powi(2)))
        .fold((0.0, 0usize), |(s, n), d| (s + d, n + 1));
    let mse = sum / n as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

fn decode_stored(path: &Path) -> iv_codec::Decoded {
    default_registry().decode_unmanaged(&Source::from_path(path), &CancelToken::never()).unwrap()
}

/// The stored pixels of `d` turned the way they are shown.
fn upright(d: &iv_codec::Decoded) -> Vec<u8> {
    let g = iv_ops::Geometry::new((d.image.width(), d.image.height()), d.info.orientation, &[]);
    iv_ops::apply::render(&d.image, &g).unwrap().to_rgba8().into_owned()
}

#[test]
fn jpeg_to_jxl_is_recompressed_losslessly_and_rebuilds_bit_for_bit() {
    let dir = tempfile::tempdir().unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let original = with_real_metadata(&photo_jpeg(256, 160), &p3);
    let src = write(dir.path(), "photo.jpg", &original);

    // `iv convert --to jxl`, defaults
    let jxl = dir.path().join("photo.jxl");
    let outcome = run(&jxl_request(&src, &jxl, vec![], &[]));
    let out = std::fs::read(&jxl).unwrap();
    assert_eq!(outcome, ExportOutcome::JpegRecompressed { bytes: out.len() });
    assert!(out.len() < original.len(), "{} bytes from a {}-byte JPEG", out.len(), original.len());

    // the same picture: both decoders run the same DCT coefficients, only their rounding differs
    let (a, b) = (decode_stored(&src), decode_stored(&jxl));
    assert_eq!(b.info.format, FormatId::Jxl);
    assert_eq!((b.image.width(), b.image.height()), (256, 160));
    let p = psnr(&a.image.to_rgba8(), &b.image.to_rgba8());
    assert!(p > 45.0, "decoded JPEG and JPEG XL differ: PSNR {p:.1} dB");

    // metadata kept: the colour profile (stored by libjxl as Display P3's code) ...
    let icc = b.info.icc_profile.as_deref().expect("colour profile kept");
    let red = |p: &[u8]| moxcms::ColorProfile::new_from_slice(p).unwrap().red_colorant;
    assert!((red(icc).x - red(&p3).x).abs() < 2e-3, "Display P3 primaries");

    // ... and everything else: the original file comes back byte for byte
    let back = dir.path().join("back.jpg");
    let outcome = run(&request(&jxl, &back, FormatId::Jpeg, vec![]));
    assert_eq!(outcome, ExportOutcome::JpegRebuilt { bytes: original.len() });
    assert_eq!(std::fs::read(&back).unwrap(), original, "JPEG -> JPEG XL -> JPEG gives the same bytes");
}

#[test]
fn jpeg_to_jxl_without_metadata_keeps_none_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let plain = photo_jpeg(128, 96);
    let original = with_real_metadata(&plain, &p3);
    let src = write(dir.path(), "located.jpg", &original);
    let jxl = dir.path().join("shared.jxl");
    let mut req = jxl_request(&src, &jxl, vec![], &[]);
    req.keep_metadata = false;
    assert!(matches!(run(&req), ExportOutcome::JpegRecompressed { .. }));
    let out = std::fs::read(&jxl).unwrap();
    assert!(!contains(&out, b"SECRET"), "metadata left in the JPEG XL");
    assert!(!contains(&out, b"Exif") && !contains(&out, b"xml "), "no metadata boxes");

    // what it holds is exactly the JPEG without its metadata
    let back = dir.path().join("back.jpg");
    assert!(matches!(run(&request(&jxl, &back, FormatId::Jpeg, vec![])), ExportOutcome::JpegRebuilt { .. }));
    let rebuilt = std::fs::read(&back).unwrap();
    assert_eq!(rebuilt, exif_edit::jpeg_without_metadata(&original).unwrap());
    assert!(!contains(&rebuilt, b"SECRET"));
    assert_eq!(scan_data(&rebuilt), scan_data(&plain));
    assert!(default_registry().probe(&Source::from_path(&back)).unwrap().icc_profile.is_none());
}

#[test]
fn rotating_a_jpeg_into_jxl_stays_lossless() {
    let dir = tempfile::tempdir().unwrap();
    let original = photo_jpeg(96, 64);
    let src = write(dir.path(), "wide.jpg", &original);
    let jxl = dir.path().join("tall.jxl");
    assert!(matches!(
        run(&jxl_request(&src, &jxl, vec![Op::rotate_cw()], &[])),
        ExportOutcome::JpegRecompressed { .. }
    ));

    // shown turned: the JPEG XL orientation, taken from the EXIF of the JPEG libjxl was given
    let d = decode_stored(&jxl);
    assert_eq!((d.image.width(), d.image.height()), (64, 96));
    let mut turned = decode_stored(&src);
    turned.info.orientation = Orientation::Rotate90;
    let p = psnr(&upright(&turned), &d.image.to_rgba8());
    assert!(p > 45.0, "PSNR {p:.1} dB");

    // the rebuilt JPEG is the original data with the orientation tag
    let back = dir.path().join("back.jpg");
    assert!(matches!(run(&request(&jxl, &back, FormatId::Jpeg, vec![])), ExportOutcome::JpegRebuilt { .. }));
    let rebuilt = std::fs::read(&back).unwrap();
    assert_eq!(scan_data(&rebuilt), scan_data(&original));
    assert_eq!(default_registry().probe(&Source::from_path(&back)).unwrap().orientation, Orientation::Rotate90);

    // and a JPEG XL turned back on the way to JPEG: still the original data, upright
    let upright_jpeg = dir.path().join("upright.jpg");
    assert!(matches!(
        run(&request(&jxl, &upright_jpeg, FormatId::Jpeg, vec![Op::rotate_ccw()])),
        ExportOutcome::JpegRebuilt { .. }
    ));
    let info = default_registry().probe(&Source::from_path(&upright_jpeg)).unwrap();
    assert_eq!(info.orientation, Orientation::Normal);
    assert_eq!(scan_data(&std::fs::read(&upright_jpeg).unwrap()), scan_data(&original));
}

/// `jpeg` with one EXIF segment per entry of `tags`, each holding only an orientation tag of
/// that (TIFF type, value).
fn with_orientation_segments(jpeg: &[u8], tags: &[(u16, u16)]) -> Vec<u8> {
    let at = 4 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
    let mut out = jpeg[..at].to_vec();
    for &(kind, value) in tags {
        let mut tiff = b"II*\0\x08\0\0\0\x01\0\x12\x01".to_vec();
        tiff.extend_from_slice(&kind.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&(value as u32).to_le_bytes());
        tiff.extend_from_slice(&[0; 4]);
        out.extend(segment(0xE1, &[b"Exif\0\0".as_slice(), &tiff].concat()));
    }
    out.extend_from_slice(&jpeg[at..]);
    out
}

#[test]
fn a_jpeg_turned_by_its_exif_is_recompressed_shown_the_same_way() {
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    let dir = tempfile::tempdir().unwrap();
    let plain = photo_jpeg(96, 64);
    // what the viewer shows, read the way it reads it; libjxl reads only the first EXIF segment
    // and only a SHORT tag, so each of these has to come out of the JPEG XL the same way
    // (name, the EXIF segments as (TIFF type, value), how the viewer shows the file)
    type Case = (&'static str, &'static [(u16, u16)], Orientation);
    let cases: [Case; 4] = [
        ("short", &[(SHORT, 6)], Orientation::Rotate90),
        ("long", &[(LONG, 6)], Orientation::Rotate90),
        ("first-disagrees", &[(SHORT, 6), (SHORT, 1)], Orientation::Normal),
        ("last-disagrees", &[(SHORT, 1), (LONG, 8)], Orientation::Rotate270),
    ];
    for (name, tags, shown) in cases {
        let original = with_orientation_segments(&plain, tags);
        let src = write(dir.path(), &format!("{name}.jpg"), &original);
        assert_eq!(default_registry().probe(&Source::from_path(&src)).unwrap().orientation, shown, "{name}");
        let jxl = dir.path().join(format!("{name}.jxl"));
        assert!(matches!(run(&jxl_request(&src, &jxl, vec![], &[])), ExportOutcome::JpegRecompressed { .. }), "{name}");
        assert_eq!(iv_codec::jxl_orientation(&std::fs::read(&jxl).unwrap()).unwrap(), shown, "{name}");
        let d = decode_stored(&jxl);
        let size = if shown == Orientation::Normal { (96, 64) } else { (64, 96) };
        assert_eq!((d.image.width(), d.image.height()), size, "{name}");
        let p = psnr(&upright(&decode_stored(&src)), &d.image.to_rgba8());
        assert!(p > 45.0, "{name}: PSNR {p:.1} dB");

        // back to JPEG: the same image data, shown the same way
        let back = dir.path().join(format!("{name}-back.jpg"));
        assert!(matches!(run(&request(&jxl, &back, FormatId::Jpeg, vec![])), ExportOutcome::JpegRebuilt { .. }));
        let rebuilt = std::fs::read(&back).unwrap();
        assert_eq!(scan_data(&rebuilt), scan_data(&plain), "{name}");
        assert_eq!(default_registry().probe(&Source::from_path(&back)).unwrap().orientation, shown, "{name}");
        if name == "short" {
            assert_eq!(rebuilt, original, "a tag libjxl reads as it is: nothing rewritten");
        }
    }
}

#[test]
fn turning_a_recompressed_jpeg_xl_keeps_it_recompressed() {
    let dir = tempfile::tempdir().unwrap();
    let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
    let plain = photo_jpeg(96, 64);
    let original = with_real_metadata(&plain, &p3);
    let src = write(dir.path(), "photo.jpg", &original);
    let jxl = dir.path().join("photo.jxl");
    assert!(matches!(run(&jxl_request(&src, &jxl, vec![], &[])), ExportOutcome::JpegRecompressed { .. }));
    let size = std::fs::read(&jxl).unwrap().len();

    // `iv convert a.jxl --to jxl --rotate cw` with the defaults (quality 90), and the viewer's
    // Save (lossless=true): the JPEG inside is turned and recompressed, not decoded
    for (name, opts) in [("default", &[][..]), ("save", &["lossless=true"][..])] {
        let turned = dir.path().join(format!("turned-{name}.jxl"));
        let outcome = run(&jxl_request(&jxl, &turned, vec![Op::rotate_cw()], opts));
        let out = std::fs::read(&turned).unwrap();
        assert_eq!(outcome, ExportOutcome::JpegRecompressed { bytes: out.len() }, "{name}");
        assert!(out.len().abs_diff(size) < 64, "{name}: {size} -> {} bytes", out.len());
        assert_eq!(iv_codec::jxl_orientation(&out).unwrap(), Orientation::Rotate90, "{name}");
        let d = decode_stored(&turned);
        assert_eq!((d.image.width(), d.image.height()), (64, 96), "{name}");

        // still the original JPEG inside, metadata and all, now turned in its EXIF
        let back = dir.path().join(format!("back-{name}.jpg"));
        assert!(matches!(run(&request(&turned, &back, FormatId::Jpeg, vec![])), ExportOutcome::JpegRebuilt { .. }));
        let rebuilt = std::fs::read(&back).unwrap();
        assert_eq!(scan_data(&rebuilt), scan_data(&plain), "{name}");
        assert!(contains(&rebuilt, b"SECRET-GPS") && contains(&rebuilt, b"SECRET-XMP"), "{name}");
        assert_eq!(default_registry().probe(&Source::from_path(&back)).unwrap().orientation, Orientation::Rotate90);
    }

    // turned back: the original file again, byte for byte
    let turned = dir.path().join("turned-default.jxl");
    let again = dir.path().join("again.jxl");
    assert!(matches!(
        run(&jxl_request(&turned, &again, vec![Op::rotate_ccw()], &[])),
        ExportOutcome::JpegRecompressed { .. }
    ));
    let back = dir.path().join("again.jpg");
    assert!(matches!(run(&request(&again, &back, FormatId::Jpeg, vec![])), ExportOutcome::JpegRebuilt { .. }));
    assert_eq!(std::fs::read(&back).unwrap(), original);

    // without metadata: none of it, and still recompressed
    let bare = dir.path().join("bare.jxl");
    let mut req = jxl_request(&jxl, &bare, vec![Op::rotate_cw()], &[]);
    req.keep_metadata = false;
    assert!(matches!(run(&req), ExportOutcome::JpegRecompressed { .. }));
    assert!(!contains(&std::fs::read(&bare).unwrap(), b"SECRET"));

    // asked for a pixel encode: pixels, and the EXIF read from the JPEG XL's box goes with them
    let pixels = dir.path().join("pixels.jxl");
    let req = jxl_request(&jxl, &pixels, vec![Op::rotate_cw()], &["lossless_jpeg=false", "quality=80"]);
    assert!(matches!(run(&req), ExportOutcome::Encoded { .. }));
    assert!(contains(&std::fs::read(&pixels).unwrap(), b"SECRET-GPS"), "EXIF kept through a pixel encode");
}

/// A baseline 8×8 CMYK JPEG (Adobe segment, four components), all blocks flat: a JPEG libjxl
/// cannot recompress.
fn cmyk_jpeg() -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    j.extend(segment(0xEE, &[b"Adobe".as_slice(), &[0, 100, 0, 0, 0, 0, 0]].concat()));
    j.extend(segment(0xDB, &[[0u8].as_slice(), &[1; 64]].concat()));
    let mut sof = vec![8, 0, 8, 0, 8, 4];
    for id in 1..=4 {
        sof.extend_from_slice(&[id, 0x11, 0]);
    }
    j.extend(segment(0xC0, &sof));
    let one_code = |class: u8| [[class].as_slice(), &[1], &[0; 15], &[0]].concat();
    j.extend(segment(0xC4, &one_code(0x00)));
    j.extend(segment(0xC4, &one_code(0x10)));
    j.extend(segment(0xDA, &[4, 1, 0, 2, 0, 3, 0, 4, 0, 0, 63, 0]));
    // each block: DC difference 0 ("0"), end of block ("0"): four blocks fill one byte
    j.extend_from_slice(&[0x00, 0xFF, 0xD9]);
    j
}

#[test]
fn jxl_falls_back_to_a_pixel_encode() {
    let dir = tempfile::tempdir().unwrap();
    let reg = default_registry();
    let jpeg = write(dir.path(), "photo.jpg", &photo_jpeg(64, 48));

    // a crop: pixels
    let out = dir.path().join("crop.jxl");
    let crop = Op::Crop(Rect { x: 8, y: 8, width: 20, height: 10 });
    assert!(matches!(run(&jxl_request(&jpeg, &out, vec![crop], &[])), ExportOutcome::Encoded { .. }));
    let d = reg.decode(&Source::from_path(&out), &CancelToken::never()).unwrap();
    assert_eq!((d.image.width(), d.image.height()), (20, 10));

    // asked for a pixel encode of a JPEG
    let out = dir.path().join("reencoded.jxl");
    let req = jxl_request(&jpeg, &out, vec![], &["lossless_jpeg=false", "quality=60"]);
    assert!(matches!(run(&req), ExportOutcome::Encoded { .. }));
    let back = dir.path().join("back.jpg");
    // no reconstruction data in it: going back to JPEG encodes pixels as well
    assert!(matches!(run(&request(&out, &back, FormatId::Jpeg, vec![])), ExportOutcome::Encoded { .. }));

    // not a JPEG
    let png = write(dir.path(), "p.png", &encode(&pattern(), ImageFormat::Png));
    let out = dir.path().join("p.jxl");
    assert!(matches!(run(&jxl_request(&png, &out, vec![], &["lossless=true"])), ExportOutcome::Encoded { .. }));
    let d = reg.decode(&Source::from_path(&out), &CancelToken::never()).unwrap();
    assert_eq!(d.image.to_rgba8().as_ref(), pattern().as_raw().as_slice(), "lossless pixels");

    // a JPEG libjxl refuses (CMYK): pixels instead of an error
    let cmyk = write(dir.path(), "cmyk.jpg", &cmyk_jpeg());
    let reference = reg.decode(&Source::from_path(&cmyk), &CancelToken::never()).unwrap();
    let encoders = Encoders::default();
    let jxl = encoders.for_format(FormatId::Jxl).unwrap();
    let refused = jxl.recompress_jpeg(&cmyk_jpeg(), &EncodeOptions::default()).expect("JPEG XL can recompress JPEGs");
    assert!(refused.is_err(), "libjxl does not take CMYK");
    let out = dir.path().join("cmyk.jxl");
    assert!(matches!(run(&jxl_request(&cmyk, &out, vec![], &[])), ExportOutcome::Encoded { .. }));
    let d = reg.decode(&Source::from_path(&out), &CancelToken::never()).unwrap();
    assert_eq!((d.image.width(), d.image.height()), (reference.image.width(), reference.image.height()));

    // a rotation of a JPEG whose EXIF has no orientation tag to rewrite: pixels, turned
    let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
    tiff.extend_from_slice(&[0x0F, 0x01, 2, 0, 2, 0, 0, 0, b'x', 0, 0, 0, 0, 0, 0, 0]);
    let plain = photo_jpeg(64, 48);
    let at = 4 + u16::from_be_bytes([plain[4], plain[5]]) as usize;
    let tagged = [&plain[..at], &segment(0xE1, &[b"Exif\0\0".as_slice(), &tiff].concat()), &plain[at..]].concat();
    let src = write(dir.path(), "make-only.jpg", &tagged);
    let out = dir.path().join("make-only.jxl");
    assert!(matches!(run(&jxl_request(&src, &out, vec![], &[])), ExportOutcome::JpegRecompressed { .. }));
    assert!(matches!(run(&jxl_request(&src, &out, vec![Op::rotate_cw()], &[])), ExportOutcome::Encoded { .. }));
    let d = reg.decode(&Source::from_path(&out), &CancelToken::never()).unwrap();
    assert_eq!((d.image.width(), d.image.height()), (48, 64));
}
