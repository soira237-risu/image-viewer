//! Thumbnails as Windows Explorer shows them: small, upright, 8-bit, made from a file the shell
//! hands over as a stream. The COM side lives in `crates/iv-thumb`; this part has no window and
//! no COM, so it is tested here.

use std::io::{Read, Seek};

use iv_codec::Registry;
use iv_core::{CancelToken, Error, ImageBuf, ImageInfo, PixelFormat, Result, Source};

/// The class id of the viewer's thumbnail provider (`crates/iv-thumb`), which registering in
/// the settings window writes: `{d8db828f-95c2-42e9-aeb2-8024b33517f1}`.
pub const PROVIDER_CLSID: u128 = 0xd8db828f_95c2_42e9_aeb2_8024b33517f1;

/// The provider's class id as the registry writes it, with braces.
pub fn provider_clsid_text() -> String {
    let v = PROVIDER_CLSID;
    format!(
        "{{{:08x}-{:04x}-{:04x}-{:04x}-{:012x}}}",
        v >> 96,
        (v >> 80) & 0xffff,
        (v >> 64) & 0xffff,
        (v >> 48) & 0xffff,
        v & 0xffff_ffff_ffff
    )
}

/// Largest file read for a thumbnail when it is not an archive (an archive is read in place).
pub const MAX_IMAGE_BYTES: u64 = crate::archive::MAX_ENTRY_BYTES;

/// The size of an image of `width`×`height` made to fit in a square of `side`, keeping its
/// shape; never enlarged, never zero.
pub fn fit(width: u32, height: u32, side: u32) -> (u32, u32) {
    let side = side.max(1);
    let longest = width.max(height);
    if longest <= side {
        return (width.max(1), height.max(1));
    }
    let scale = side as f64 / longest as f64;
    let scaled = |v: u32| ((v as f64 * scale).round() as u32).clamp(1, side);
    (scaled(width), scaled(height))
}

/// Whether a decoder's quick preview can stand in for the image in a thumbnail of longest side
/// `want`: at least that large, and the image's shape (an embedded EXIF thumbnail can be a
/// letterboxed 160×120 of a 3:2 photo, or already turned upright).
fn preview_fits(preview: &ImageBuf, info: &ImageInfo, want: u32) -> bool {
    let (pw, ph) = (preview.width() as f64, preview.height() as f64);
    let (w, h) = (info.width as f64, info.height as f64);
    preview.width().max(preview.height()) >= want && ((pw / ph) / (w / h) - 1.0).abs() < 0.02
}

/// The thumbnail of `src`: upright (EXIF orientation applied), 8-bit RGBA with straight alpha,
/// its longest side `side` or the image's own when that is smaller. Uses a decoder's quick
/// preview when it is large enough, the full decode (colour-managed to sRGB) otherwise.
pub fn thumbnail(registry: &Registry, src: &Source, side: u32, cancel: &CancelToken) -> Result<ImageBuf> {
    let info = registry.probe(src)?;
    let want = side.max(1).min(info.width.max(info.height));
    let preview = match registry.preview(src, want, cancel) {
        Ok(Some(p)) if preview_fits(&p, &info, want) => Some(p),
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        _ => None,
    };
    let stored = match preview {
        Some(p) => p,
        None => registry.decode(src, cancel)?.image,
    };
    cancel.check()?;
    // shrink first, then turn: turning the small image is the cheap order
    let (w, h) = fit(stored.width(), stored.height(), side);
    let small = iv_ops::apply::resize(&stored, w, h)?;
    let upright = iv_ops::apply::orient(&small, info.orientation)?;
    if upright.format() == PixelFormat::Rgba8 {
        return Ok(upright);
    }
    let rgba = upright.to_rgba8().into_owned();
    ImageBuf::new(upright.width(), upright.height(), PixelFormat::Rgba8, rgba)
}

/// The thumbnail of a file the shell hands over as `reader`, named `name` (its extension helps
/// tell formats without a signature apart; it may be empty). An archive shows its first page.
pub fn thumbnail_of<R: Read + Seek>(
    registry: &Registry,
    mut reader: R,
    name: &str,
    side: u32,
    cancel: &CancelToken,
) -> Result<ImageBuf> {
    let mut head = Vec::with_capacity(8);
    (&mut reader).take(8).read_to_end(&mut head)?;
    reader.seek(std::io::SeekFrom::Start(0))?;
    let src = if crate::archive::is_archive_content(&head) {
        crate::archive::cover(reader)?.ok_or(Error::UnsupportedFormat)?
    } else {
        let mut bytes = Vec::new();
        reader.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(Error::decode("thumbnail", format!("over the {MAX_IMAGE_BYTES}-byte limit")));
        }
        Source::from_bytes(name, bytes)
    };
    cancel.check()?;
    thumbnail(registry, &src, side, cancel)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn png(img: image::DynamicImage) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// Left half red, right half blue, `w`×`h`, opaque.
    fn halves(w: u32, h: u32) -> image::RgbaImage {
        image::RgbaImage::from_fn(w, h, |x, _| {
            if x < w / 2 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) }
        })
    }

    fn px(img: &ImageBuf, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * img.width() as usize + x as usize) * 4;
        img.data()[i..i + 4].try_into().unwrap()
    }

    fn of(bytes: Vec<u8>, name: &str, side: u32) -> Result<ImageBuf> {
        thumbnail_of(&iv_codec::default_registry(), Cursor::new(bytes), name, side, &CancelToken::never())
    }

    #[test]
    fn fitting_keeps_the_shape_and_never_enlarges() {
        assert_eq!(fit(4000, 3000, 256), (256, 192));
        assert_eq!(fit(3000, 4000, 256), (192, 256));
        assert_eq!(fit(100, 50, 256), (100, 50));
        assert_eq!(fit(10_000, 1, 256), (256, 1), "a sliver keeps one pixel");
        assert_eq!(fit(256, 256, 256), (256, 256));
        assert_eq!(fit(5, 5, 0), (1, 1));
        assert_eq!(provider_clsid_text(), "{d8db828f-95c2-42e9-aeb2-8024b33517f1}");
    }

    #[test]
    fn a_large_image_shrinks_to_the_side_asked_for() {
        let t = of(png(halves(400, 200).into()), "a.png", 100).unwrap();
        assert_eq!((t.width(), t.height(), t.format()), (100, 50, PixelFormat::Rgba8));
        assert_eq!(px(&t, 5, 25), [255, 0, 0, 255]);
        assert_eq!(px(&t, 94, 25), [0, 0, 255, 255]);
        // a small one stays as it is
        let t = of(png(halves(40, 20).into()), "a.png", 256).unwrap();
        assert_eq!((t.width(), t.height()), (40, 20));
    }

    #[test]
    fn alpha_and_deep_colour_come_out_as_8_bit_rgba() {
        let clear = image::RgbaImage::from_fn(64, 64, |x, _| image::Rgba([0, 200, 0, if x < 32 { 0 } else { 255 }]));
        let t = of(png(clear.into()), "a.png", 32).unwrap();
        assert_eq!(px(&t, 2, 16)[3], 0, "transparent stays transparent");
        assert_eq!(px(&t, 29, 16), [0, 200, 0, 255]);

        let deep =
            image::ImageBuffer::<image::Rgba<u16>, _>::from_fn(20, 10, |_, _| image::Rgba([65535, 0, 32896, 65535]));
        let t = of(png(image::DynamicImage::ImageRgba16(deep)), "deep.png", 256).unwrap();
        assert_eq!(t.format(), PixelFormat::Rgba8);
        assert_eq!(px(&t, 3, 3), [255, 0, 128, 255]);
    }

    #[test]
    fn exif_orientation_is_applied() {
        // stored 60 wide and 30 high, red left; orientation 6 shows it turned 90° clockwise
        let mut jpeg = Cursor::new(Vec::new());
        image::DynamicImage::from(halves(60, 30)).to_rgb8().write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
        let turned = iv_codec::exif_edit::jpeg_with_orientation(jpeg.get_ref(), 6).unwrap();
        let t = of(turned, "photo.jpg", 256).unwrap();
        assert_eq!((t.width(), t.height()), (30, 60), "upright");
        let [r, _, b, _] = px(&t, 15, 5);
        assert!(r > 200 && b < 60, "red on top after turning clockwise: {r} {b}");
        let [r, _, b, _] = px(&t, 15, 54);
        assert!(b > 200 && r < 60, "blue below: {r} {b}");
    }

    #[test]
    fn a_format_known_only_by_its_extension_needs_the_name() {
        // TGA has no signature: the name the shell gives decides it
        let mut tga = Cursor::new(Vec::new());
        image::DynamicImage::from(halves(8, 4)).write_to(&mut tga, image::ImageFormat::Tga).unwrap();
        let t = of(tga.get_ref().clone(), "x.tga", 256).unwrap();
        assert_eq!((t.width(), t.height()), (8, 4));
        assert!(of(tga.into_inner(), "", 256).is_err());
    }

    #[test]
    fn an_archive_shows_its_first_page() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, img) in [("10.png", halves(16, 16)), ("2.png", halves(30, 10))] {
            zip.start_file(name, opts).unwrap();
            std::io::Write::write_all(&mut zip, &png(img.into())).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let t = of(bytes, "comic.cbz", 256).unwrap();
        assert_eq!((t.width(), t.height()), (30, 10), "2.png comes before 10.png");

        let mut empty = zip::ZipWriter::new(Cursor::new(Vec::new()));
        empty.start_file("readme.txt", opts).unwrap();
        assert!(matches!(of(empty.finish().unwrap().into_inner(), "x.cbz", 256), Err(Error::UnsupportedFormat)));
    }

    #[test]
    fn broken_files_fail_without_panicking() {
        let mut bytes = png(halves(64, 64).into());
        bytes.truncate(bytes.len() / 2);
        assert!(of(bytes, "cut.png", 256).is_err());
        assert!(of(b"not an image".to_vec(), "x.png", 256).is_err());
        assert!(of(Vec::new(), "", 256).is_err());
    }
}
