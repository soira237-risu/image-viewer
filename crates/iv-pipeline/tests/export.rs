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
    let rgb = |d: &[u8]| d.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect::<Vec<_>>();
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
