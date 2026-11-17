//! The viewer's side of editing: the selection (kept in image pixels, so zooming, panning or
//! resizing the window leaves the same part of the image selected), and building save requests.
//! Kept free of window state so it can be tested directly.

use std::path::{Path, PathBuf};

use crate::i18n::{Strings, fill};
use crate::view::{ScreenRect, ViewState};
use iv_codec::Decoded;
use iv_codec::encode::{EncodeOptions, Encoders};
use iv_core::{FormatId, ImageInfo};
use iv_ops::{Op, Rect};
use iv_pipeline::export::ExportRequest;

/// The whole image pixels between two drag corners given in image space (any order), clamped to
/// an image of `image` size. This is what [`Op::Crop`] expects. `None` when the drag misses the
/// image or covers less than a pixel.
pub fn selection_rect(a: (f64, f64), b: (f64, f64), image: (f64, f64)) -> Option<Rect> {
    let (iw, ih) = image;
    let x0 = a.0.min(b.0).clamp(0.0, iw).round();
    let y0 = a.1.min(b.1).clamp(0.0, ih).round();
    let x1 = a.0.max(b.0).clamp(0.0, iw).round();
    let y1 = a.1.max(b.1).clamp(0.0, ih).round();
    (x1 - x0 >= 1.0 && y1 - y0 >= 1.0).then_some(Rect {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}

/// The selection from a drag between two screen points, through `view`.
pub fn selection_from_drag(view: &ViewState, a: (f64, f64), b: (f64, f64)) -> Option<Rect> {
    selection_rect(view.screen_to_image(a), view.screen_to_image(b), view.image_size())
}

/// Where a selection of image pixels lies on screen: its edges are the pixel edges, so the area
/// left undimmed is exactly what Crop keeps, at any zoom.
pub fn selection_on_screen(view: &ViewState, r: Rect) -> ScreenRect {
    let (x0, y0) = view.image_to_screen((r.x as f64, r.y as f64));
    let (x1, y1) = view.image_to_screen(((r.x + r.width) as f64, (r.y + r.height) as f64));
    ScreenRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
}

/// Why a save could not start. Worded for the user by [`SaveError::message`].
#[derive(Clone, Debug, PartialEq)]
pub enum SaveError {
    /// The name's extension is not an image format (the extension, without the dot).
    UnknownExtension(String),
    /// A known format with no encoder (HEIC, RAW, …).
    NotWritable(FormatId),
    /// The encoder rejected its options.
    Options(String),
}

impl SaveError {
    pub fn message(&self, t: &Strings) -> String {
        match self {
            SaveError::UnknownExtension(ext) => fill(t.unknown_file_type, &[("ext", ext)]),
            SaveError::NotWritable(format) => fill(t.cannot_write_format, &[("format", &format.to_string())]),
            SaveError::Options(e) => fill(t.save_failed, &[("error", e)]),
        }
    }
}

/// Format to use when saving over `format` in place, if it can be written.
pub fn in_place_format(encoders: &Encoders, format: FormatId) -> Option<FormatId> {
    encoders.for_format(format).map(|_| format)
}

/// Whether Save may write the edits over the file itself, and if not, why it asks for a new
/// name instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InPlace {
    /// Write over the file in this format: what its content is, whatever its extension says.
    Write(FormatId),
    /// A page inside a ZIP or 7z: there is no file of its own to write.
    ArchivePage,
    /// More than one frame: saving keeps only the first, which would destroy the animation.
    Animated,
    /// Only the header has been read and it does not give the frame count (GIF, animated WebP,
    /// APNG): saving now could overwrite an animation with its first frame, so nothing is
    /// saved until the full decode says how many frames there are.
    NotLoaded,
    /// Nothing can write the format (HEIC, RAW, …).
    NotWritable,
    /// Floating-point samples (a float JPEG XL or TIFF, HDR): no encoder writes them, so saving
    /// would round the file to 8 bits per channel and clip what lies outside 0–1.
    TooDeep,
}

impl InPlace {
    /// Why the save goes to Save As instead, in the UI language; `None` for [`InPlace::Write`].
    pub fn reason(self, t: &Strings) -> Option<&'static str> {
        match self {
            InPlace::Write(_) => None,
            InPlace::ArchivePage => Some(t.archive_page_save_as),
            InPlace::Animated => Some(t.animated_save_as),
            InPlace::NotLoaded => Some(t.save_not_loaded),
            InPlace::NotWritable => Some(t.format_not_writable),
            InPlace::TooDeep => Some(t.deep_save_as),
        }
    }

    /// `self`, unless it would write over an image with more than 16 bits per channel (what
    /// the decoders give for float and deeper integer samples), which no encoder keeps.
    pub fn keeping_depth(self, bits_per_channel: u8) -> InPlace {
        match self {
            InPlace::Write(_) if bits_per_channel > 16 => InPlace::TooDeep,
            other => other,
        }
    }
}

/// How many frames an image has, as far as is known so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frames {
    One,
    Many,
    /// Only the header has been read, and it does not say (GIF, animated WebP, APNG, …).
    Unknown,
}

/// What `info` (from the header probe, or from the decode once it is in) and the full decode,
/// if it has arrived, say about the frame count. Only a count of exactly one is a still image:
/// before the decode is in, a count the probe could not give may hide an animation; after it,
/// a count the decode did not give either means an animation too large to play was decoded as
/// its first frame (or a format that does not count frames), so it counts as more than one.
pub fn frames(info: &ImageInfo, full: Option<&Decoded>) -> Frames {
    let count = full.and_then(|d| d.info.frame_count).or(info.frame_count);
    match (count, full) {
        (_, Some(d)) if d.animation.is_some() => Frames::Many,
        (Some(n), _) if n > 1 => Frames::Many,
        (Some(1), _) => Frames::One,
        (_, Some(_)) => Frames::Many,
        (_, None) => Frames::Unknown,
    }
}

/// How the image at `path` can be saved in place. `format` is the format its content was
/// identified as (not the extension's), `frames` what is known of its frame count (see
/// [`frames`]).
pub fn in_place(encoders: &Encoders, path: &Path, format: FormatId, frames: Frames) -> InPlace {
    if iv_pipeline::archive::split(path).is_some() {
        return InPlace::ArchivePage;
    }
    // whatever the frame count is, nothing could write the format
    let Some(format) = in_place_format(encoders, format) else { return InPlace::NotWritable };
    match frames {
        Frames::Many => InPlace::Animated,
        Frames::Unknown => InPlace::NotLoaded,
        Frames::One => InPlace::Write(format),
    }
}

/// The file name's extension names another format than the content's (a WebP called `.jpg`);
/// an extension that names no format is not a mismatch.
pub fn extension_mismatch(path: &Path, content: FormatId) -> bool {
    path.extension().and_then(|e| e.to_str()).and_then(FormatId::from_extension).is_some_and(|f| f != content)
}

/// `a` and `b` name one file: the same path, or two spellings of an existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// Builds the request for saving `source` with `ops` to `dest`; the format follows `dest`'s
/// extension (Save As). `quality` applies to formats that have a quality option (JPEG, WebP,
/// AVIF, JPEG XL).
pub fn save_request(
    encoders: &Encoders,
    source: &Path,
    ops: &[Op],
    dest: PathBuf,
    quality: u8,
) -> Result<ExportRequest, SaveError> {
    let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or_default().to_string();
    let format = FormatId::from_extension(&ext).ok_or(SaveError::UnknownExtension(ext))?;
    save_request_as(encoders, source, ops, dest, format, quality)
}

/// Builds the request for saving `source` with `ops` to `dest` as `format`, whatever `dest`'s
/// extension says: Save keeps the file's own (content) format, so a WebP named `.jpg` stays
/// WebP and keeps its transparency and lossless pixels.
pub fn save_request_as(
    encoders: &Encoders,
    source: &Path,
    ops: &[Op],
    dest: PathBuf,
    format: FormatId,
    quality: u8,
) -> Result<ExportRequest, SaveError> {
    let Some(encoder) = encoders.for_format(format) else {
        return Err(SaveError::NotWritable(format));
    };
    let mut pairs = Vec::new();
    if encoder.options().iter().any(|o| o.key == "quality") {
        pairs.push(format!("quality={}", quality.clamp(1, 100)));
    }
    // A JPEG XL saved over itself is written losslessly: it may be a lossless original (or a
    // recompressed JPEG), and Save must never quietly degrade it. Elsewhere the quality applies
    // (a JPEG without pixel edits is still recompressed losslessly, see `export`).
    if format == FormatId::Jxl && same_file(source, &dest) {
        pairs.push("lossless=true".into());
    }
    let options = EncodeOptions::parse(encoder.options(), &pairs).map_err(SaveError::Options)?;
    Ok(ExportRequest {
        source: source.to_path_buf(),
        ops: ops.to_vec(),
        format,
        options,
        dest,
        background: [255, 255, 255],
        keep_metadata: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_drags_collapse_and_identity_adjustments_are_dropped() {
        use iv_core::Orientation;
        use iv_ops::{Adjust, EditStack, Geometry};
        let is_adjust = |o: &Op| matches!(o, Op::Adjust(_));
        let mut edits = EditStack::new();
        // a slider drag collapses to its last value
        for b in [0.1, 0.2, 0.4] {
            edits.push_or_replace(Op::Adjust(Adjust { brightness: b, ..Default::default() }), is_adjust);
        }
        edits.push(Op::rotate_cw());
        edits.push_or_replace(Op::Adjust(Adjust { saturation: -1.0, ..Default::default() }), is_adjust);
        edits.push(Op::Adjust(Adjust::default()));
        let g = Geometry::new((10, 10), Orientation::Normal, edits.active());
        assert_eq!(
            g.adjusts,
            vec![Adjust { brightness: 0.4, ..Default::default() }, Adjust { saturation: -1.0, ..Default::default() }]
        );
    }

    #[test]
    fn drag_corners_in_any_order() {
        let r = selection_rect((10.0, 20.0), (4.0, 2.0), (100.0, 100.0)).unwrap();
        assert_eq!(r, Rect { x: 4, y: 2, width: 6, height: 18 });
    }

    #[test]
    fn selection_maps_through_zoom_and_clamps_to_the_image() {
        // 200x100 image at 100 % in a 400x300 viewport sits at (100, 100)
        let mut v = ViewState::new((200, 100), (400, 300));
        v.actual_size_at((200.0, 150.0));
        let r = selection_from_drag(&v, (110.0, 120.0), (160.0, 150.0)).unwrap();
        assert_eq!(r, Rect { x: 10, y: 20, width: 50, height: 30 });
        // dragging past the image edge stops at the edge
        let edge = selection_from_drag(&v, (250.0, 50.0), (750.0, 550.0)).unwrap();
        assert_eq!(edge, Rect { x: 150, y: 0, width: 50, height: 100 });
        // entirely outside the image
        assert!(selection_from_drag(&v, (0.0, 0.0), (50.0, 50.0)).is_none());
    }

    #[test]
    fn selection_stays_on_the_same_pixels_through_zoom_pan_and_resize() {
        let mut v = ViewState::new((200, 100), (400, 300));
        v.actual_size_at((200.0, 150.0));
        let sel = selection_from_drag(&v, (110.0, 120.0), (160.0, 150.0)).unwrap();
        let on_screen_before = selection_on_screen(&v, sel);
        assert_eq!(on_screen_before, ScreenRect { x: 110.0, y: 120.0, width: 50.0, height: 30.0 });

        v.zoom_by_at(4.0, (130.0, 130.0));
        v.pan_by(-37.0, 12.0);
        v.set_viewport((640, 480));
        // the stored selection is what Crop uses, unchanged; on screen it moved with the image
        let s = selection_on_screen(&v, sel);
        assert_ne!(s, on_screen_before);
        assert!((s.width - 200.0).abs() < 1e-9 && (s.height - 120.0).abs() < 1e-9, "{s:?}");
        let back = v.screen_to_image((s.x, s.y));
        assert!((back.0 - 10.0).abs() < 1e-9 && (back.1 - 20.0).abs() < 1e-9, "{back:?}");
    }

    #[test]
    fn highlighted_area_is_whole_image_pixels() {
        // at 8x a drag that starts mid-pixel snaps to the nearest pixel edge, and the dimmed
        // window's edges are those pixel edges (not the raw mouse points)
        let mut v = ViewState::new((40, 40), (400, 400));
        v.set_zoom_at(8.0, (200.0, 200.0));
        let sel = selection_from_drag(&v, (43.0, 43.0), (90.0, 90.0)).unwrap();
        let s = selection_on_screen(&v, sel);
        let (a, b) = (v.screen_to_image((s.x, s.y)), v.screen_to_image((s.x + s.width, s.y + s.height)));
        for c in [a.0, a.1, b.0, b.1] {
            assert!((c - c.round()).abs() < 1e-9, "edge {c} is not on a pixel boundary");
        }
        assert_eq!((b.0 - a.0, b.1 - a.1), (sel.width as f64, sel.height as f64));
    }

    #[test]
    fn save_errors_are_worded_in_the_ui_language() {
        use crate::i18n::{EN, KO};
        let enc = Encoders::default();
        let unknown = save_request(&enc, Path::new("a.jpg"), &[], PathBuf::from("out.xyz"), 90).unwrap_err();
        assert_eq!(unknown, SaveError::UnknownExtension("xyz".into()));
        assert_eq!(unknown.message(&KO), "알 수 없는 파일 형식입니다: .xyz");
        let heic = save_request(&enc, Path::new("a.jpg"), &[], PathBuf::from("out.heic"), 90).unwrap_err();
        assert_eq!(heic, SaveError::NotWritable(FormatId::Heif));
        assert!(heic.message(&EN).ends_with("files cannot be written"), "{}", heic.message(&EN));
        assert!(heic.message(&KO).ends_with("형식으로는 저장할 수 없습니다"), "{}", heic.message(&KO));
    }

    #[test]
    fn save_requests_follow_the_extension() {
        let enc = Encoders::default();
        let r = save_request(&enc, Path::new("a.jpg"), &[Op::rotate_cw()], PathBuf::from("out.WEBP"), 70).unwrap();
        assert_eq!(r.format, FormatId::Webp);
        assert_eq!(r.ops, vec![Op::rotate_cw()]);
        let webp = enc.for_format(FormatId::Webp).unwrap();
        assert_eq!(
            r.options,
            EncodeOptions::parse(webp.options(), &["quality=70".into()]).unwrap(),
            "panel quality is used"
        );
        let png = save_request(&enc, Path::new("a.jpg"), &[], PathBuf::from("out.png"), 70).unwrap();
        assert_eq!(png.options, EncodeOptions::default(), "formats without quality ignore it");
        assert!(save_request(&enc, Path::new("a.jpg"), &[], PathBuf::from("out.xyz"), 90).is_err());
        assert_eq!(in_place_format(&enc, FormatId::Heif), None);
        assert_eq!(in_place_format(&enc, FormatId::Png), Some(FormatId::Png));
    }

    #[test]
    fn saving_in_place_keeps_the_content_format_not_the_extension() {
        let enc = Encoders::default();
        let dir = tempfile::tempdir().unwrap();
        // WebP bytes named .jpg: written back as WebP (alpha and lossless pixels survive)
        let webp_as_jpg = dir.path().join("x.jpg");
        let InPlace::Write(format) = in_place(&enc, &webp_as_jpg, FormatId::Webp, Frames::One) else { panic!() };
        let r = save_request_as(&enc, &webp_as_jpg, &[Op::rotate_cw()], webp_as_jpg.clone(), format, 80).unwrap();
        assert_eq!((r.format, r.dest.as_path()), (FormatId::Webp, webp_as_jpg.as_path()));
        // a PNG with an unknown extension still saves (the extension is never consulted)
        let odd = dir.path().join("x.dat");
        assert_eq!(in_place(&enc, &odd, FormatId::Png, Frames::One), InPlace::Write(FormatId::Png));
        assert_eq!(save_request_as(&enc, &odd, &[], odd.clone(), FormatId::Png, 80).unwrap().format, FormatId::Png);
        assert!(extension_mismatch(&webp_as_jpg, FormatId::Webp));
        assert!(!extension_mismatch(Path::new("a.JPEG"), FormatId::Jpeg));
        assert!(!extension_mismatch(&odd, FormatId::Png), "an unknown extension is not a mismatch");
        // a JPEG named .heic is writable: its content is JPEG
        assert_eq!(
            in_place(&enc, &dir.path().join("x.heic"), FormatId::Jpeg, Frames::One),
            InPlace::Write(FormatId::Jpeg)
        );
        assert_eq!(in_place(&enc, &dir.path().join("x.heic"), FormatId::Heif, Frames::One), InPlace::NotWritable);
    }

    #[test]
    fn animations_and_archive_pages_are_never_saved_in_place() {
        use crate::i18n::{EN, KO};
        let enc = Encoders::default();
        let dir = tempfile::tempdir().unwrap();
        // GIF, WebP and PNG can all be written, but only as one frame
        for f in [FormatId::Gif, FormatId::Webp, FormatId::Png] {
            assert_eq!(in_place(&enc, &dir.path().join("a.gif"), f, Frames::Many), InPlace::Animated);
        }
        // a page inside a real archive has no file of its own
        let cbz = dir.path().join("book.cbz");
        std::fs::write(&cbz, b"PK").unwrap();
        let page = iv_pipeline::archive::entry_path(&cbz, "p/001.png");
        assert_eq!(in_place(&enc, &page, FormatId::Png, Frames::One), InPlace::ArchivePage);
        assert_eq!(in_place(&enc, &page, FormatId::Gif, Frames::Many), InPlace::ArchivePage);
        // every refusal says why, in both languages
        for kind in
            [InPlace::ArchivePage, InPlace::Animated, InPlace::NotWritable, InPlace::NotLoaded, InPlace::TooDeep]
        {
            assert!(kind.reason(&KO).is_some_and(|s| !s.is_empty()) && kind.reason(&EN).is_some());
        }
        assert_eq!(InPlace::Write(FormatId::Png).reason(&EN), None);
    }

    /// A 1x1 GIF with `frames` identical frames, 100 ms each.
    fn gif(frames: usize) -> Vec<u8> {
        // header, 1x1 screen with a two-colour global palette
        let mut b = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\x00\x00\x00".to_vec();
        for _ in 0..frames {
            b.extend_from_slice(b"\x21\xf9\x04\x00\x0a\x00\x00\x00"); // graphic control: 100 ms
            b.extend_from_slice(b"\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00"); // image descriptor
            b.extend_from_slice(b"\x02\x02\x44\x01\x00"); // LZW: one pixel of colour 0
        }
        b.push(0x3b);
        b
    }

    #[test]
    fn a_frame_count_the_probe_cannot_give_is_never_saved_over() {
        use iv_core::{CancelToken, Orientation, Source};
        let enc = Encoders::default();
        let registry = iv_codec::default_registry();
        let path = tempfile::tempdir().unwrap().path().join("a.gif");
        for (count, after_decode) in [(1, InPlace::Write(FormatId::Gif)), (3, InPlace::Animated)] {
            let src = Source::from_bytes("a.gif", gif(count));
            // what the viewer has between the probe and the full decode
            let probed = registry.probe(&src).unwrap();
            assert_eq!(frames(&probed, None), Frames::Unknown, "{count} frames");
            assert_eq!(in_place(&enc, &path, probed.format, frames(&probed, None)), InPlace::NotLoaded);
            // once the decode is in, the viewer's info is the decoded one
            let decoded = registry.decode(&src, &CancelToken::never()).unwrap();
            let known = frames(&decoded.info, Some(&decoded));
            assert_eq!(in_place(&enc, &path, decoded.info.format, known), after_decode, "{count} frames");
        }
        // animated WebP and APNG probe without a count too; a probed count is trusted
        let info = |format, frame_count| ImageInfo {
            format,
            codec: "test",
            width: 1,
            height: 1,
            frame_count,
            has_alpha: false,
            bits_per_channel: 8,
            orientation: Orientation::Normal,
            icc_profile: None,
            exif: None,
        };
        for f in [FormatId::Gif, FormatId::Webp, FormatId::Png] {
            assert_eq!(in_place(&enc, &path, f, frames(&info(f, None), None)), InPlace::NotLoaded);
            assert_eq!(in_place(&enc, &path, f, frames(&info(f, Some(2)), None)), InPlace::Animated);
            assert_eq!(in_place(&enc, &path, f, frames(&info(f, Some(1)), None)), InPlace::Write(f));
        }
        // a format nothing can write says so, whatever its frame count
        for f in [Frames::Unknown, Frames::Many, Frames::One] {
            assert_eq!(in_place(&enc, &path, FormatId::Heif, f), InPlace::NotWritable);
        }
        // an animation over the size limit is decoded as its first frame without a count: the
        // file still holds every frame, so it is not written over
        let pixel = || iv_core::ImageBuf::new(1, 1, iv_core::PixelFormat::Rgba8, vec![0; 4]).unwrap();
        let first_frame_only = Decoded::still(info(FormatId::Gif, None), pixel());
        let known = frames(&first_frame_only.info, Some(&first_frame_only));
        assert_eq!(in_place(&enc, &path, FormatId::Gif, known), InPlace::Animated);
        // a decoded still with a count of one is written in place
        let still = Decoded::still(info(FormatId::Png, Some(1)), pixel());
        assert_eq!(frames(&still.info, Some(&still)), Frames::One);
    }

    #[test]
    fn a_still_jpeg_xl_is_saved_in_place() {
        use iv_codec::encode::{EncodeOptions, Metadata, prepare};
        use iv_core::{CancelToken, ImageBuf, PixelFormat, Source};
        let enc = Encoders::default();
        let jxl = enc.for_format(FormatId::Jxl).expect("a JPEG XL encoder");
        let image = ImageBuf::new(3, 2, PixelFormat::Rgba8, (0..24).collect()).unwrap();
        let bytes = jxl.encode(&prepare(&image, jxl.info(), [255; 3]), &EncodeOptions::default(), &Metadata::default());
        let src = Source::from_bytes("a.jxl", bytes.unwrap());
        let registry = iv_codec::default_registry();
        let path = tempfile::tempdir().unwrap().path().join("a.jxl");
        // the header says it is a still image: known as soon as it is probed
        let probed = registry.probe(&src).unwrap();
        assert_eq!(in_place(&enc, &path, probed.format, frames(&probed, None)), InPlace::Write(FormatId::Jxl));
        let decoded = registry.decode(&src, &CancelToken::never()).unwrap();
        let known = frames(&decoded.info, Some(&decoded));
        assert_eq!(in_place(&enc, &path, decoded.info.format, known), InPlace::Write(FormatId::Jxl));
    }

    #[test]
    fn a_float_jpeg_xl_is_never_saved_over_itself() {
        use iv_core::{CancelToken, Source};
        let enc = Encoders::default();
        let registry = iv_codec::default_registry();
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/jxl/float-hdr.jxl");
        let src = Source::from_path(fixture);
        // what the viewer has after the probe, and after the full decode: no encoder writes
        // float samples, so Save would round them to 8 bits and clip the HDR values
        let probed = registry.probe(&src).unwrap();
        let decoded = registry.decode(&src, &CancelToken::never()).unwrap();
        for (info, full) in [(&probed, None), (&decoded.info, Some(&decoded))] {
            let target = in_place(&enc, Path::new(fixture), info.format, frames(info, full));
            assert_eq!(target, InPlace::Write(FormatId::Jxl), "a still JPEG XL is writable");
            assert_eq!(target.keeping_depth(info.bits_per_channel), InPlace::TooDeep);
        }
        // 8- and 16-bit images are written in place as before
        for bits in [8, 16] {
            assert_eq!(InPlace::Write(FormatId::Jxl).keeping_depth(bits), InPlace::Write(FormatId::Jxl));
        }
        assert_eq!(InPlace::Animated.keeping_depth(32), InPlace::Animated, "the first reason stays");
    }

    #[test]
    fn a_jpeg_xl_saved_over_itself_stays_lossless() {
        use iv_codec::encode::{EncodeOptions, Metadata, prepare};
        use iv_core::{CancelToken, ImageBuf, PixelFormat, Source};
        use iv_pipeline::export::{ExportOutcome, export};
        let enc = Encoders::default();
        let jxl = enc.for_format(FormatId::Jxl).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art.jxl");
        // noisy pixels: any lossy encode changes them
        let (w, h) = (24u32, 16u32);
        let px: Vec<u8> = (0..w * h * 4)
            .map(|i| if i % 4 == 3 { 255 } else { (i.wrapping_mul(2_654_435_761) >> 13) as u8 })
            .collect();
        let img = prepare(&ImageBuf::new(w, h, PixelFormat::Rgba8, px.clone()).unwrap(), jxl.info(), [255; 3]);
        let lossless = EncodeOptions::parse(jxl.options(), &["lossless=true".into()]).unwrap();
        std::fs::write(&path, jxl.encode(&img, &lossless, &Metadata::default()).unwrap()).unwrap();

        // Save (in place), with the save quality at 50
        let req = save_request_as(&enc, &path, &[Op::flip_h()], path.clone(), FormatId::Jxl, 50).unwrap();
        let expected = EncodeOptions::parse(jxl.options(), &["quality=50".into(), "lossless=true".into()]).unwrap();
        assert_eq!(req.options, expected);
        assert!(matches!(export(&iv_codec::default_registry(), &enc, &req), Ok(ExportOutcome::Encoded { .. })));
        let d = iv_codec::default_registry().decode(&Source::from_path(&path), &CancelToken::never()).unwrap();
        let got = d.image.to_rgba8();
        for (i, p) in got.as_chunks::<4>().0.iter().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let s = ((y * w + (w - 1 - x)) * 4) as usize;
            assert_eq!(p[..], px[s..s + 4], "pixel ({x}, {y}) of the flipped image");
        }

        // Save As to the same file, spelled another way: the same rule
        let other_spelling = dir.path().join(".").join("art.jxl");
        let req = save_request(&enc, &path, &[], other_spelling, 50).unwrap();
        assert_eq!(req.options, expected);
        // Save As to another file: the save quality
        let req = save_request(&enc, &path, &[], dir.path().join("copy.jxl"), 50).unwrap();
        assert_eq!(req.options, EncodeOptions::parse(jxl.options(), &["quality=50".into()]).unwrap());
    }

    #[test]
    fn save_as_jpeg_xl_of_an_unedited_jpeg_recompresses_it() {
        use iv_codec::encode::{EncodeOptions, Metadata, prepare};
        use iv_core::{ImageBuf, PixelFormat};
        use iv_pipeline::export::{ExportOutcome, export};
        let enc = Encoders::default();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("photo.jpg");
        let px: Vec<u8> =
            (0..48 * 32u32).flat_map(|i| [(i % 48 * 5) as u8, (i / 48 * 7) as u8, (i % 251) as u8, 255]).collect();
        let jpeg = enc.for_format(FormatId::Jpeg).unwrap();
        let img = prepare(&ImageBuf::new(48, 32, PixelFormat::Rgba8, px).unwrap(), jpeg.info(), [255; 3]);
        std::fs::write(&src, jpeg.encode(&img, &EncodeOptions::default(), &Metadata::default()).unwrap()).unwrap();
        // the panel quality does not turn the lossless recompression into a pixel encode
        for ops in [vec![], vec![Op::rotate_cw()]] {
            let req = save_request(&enc, &src, &ops, dir.path().join("photo.jxl"), 60).unwrap();
            let outcome = export(&iv_codec::default_registry(), &enc, &req).unwrap();
            assert!(matches!(outcome, ExportOutcome::JpegRecompressed { .. }), "{ops:?}: {outcome:?}");
        }
        // Save (in place) of that JPEG XL turned once more: the JPEG inside is turned and
        // recompressed again, not decoded and written as a lossless pixel image many times larger
        let jxl = dir.path().join("photo.jxl");
        let before = std::fs::metadata(&jxl).unwrap().len();
        let req = save_request_as(&enc, &jxl, &[Op::rotate_cw()], jxl.clone(), FormatId::Jxl, 60).unwrap();
        let outcome = export(&iv_codec::default_registry(), &enc, &req).unwrap();
        assert!(matches!(outcome, ExportOutcome::JpegRecompressed { .. }), "{outcome:?}");
        let after = std::fs::metadata(&jxl).unwrap().len();
        assert!(after.abs_diff(before) < 64, "{before} -> {after} bytes");
        let shown = iv_codec::jxl_orientation(&std::fs::read(&jxl).unwrap()).unwrap();
        assert_eq!(shown, iv_core::Orientation::Rotate180, "turned twice");
    }
}
