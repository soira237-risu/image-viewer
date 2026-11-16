//! The viewer's side of editing: the selection (kept in image pixels, so zooming, panning or
//! resizing the window leaves the same part of the image selected), and building save requests.
//! Kept free of window state so it can be tested directly.

use std::path::{Path, PathBuf};

use crate::i18n::{Strings, fill};
use crate::view::{ScreenRect, ViewState};
use iv_codec::encode::{EncodeOptions, Encoders};
use iv_core::FormatId;
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
    /// Nothing can write the format (HEIC, RAW, …).
    NotWritable,
}

impl InPlace {
    /// Why the save goes to Save As instead, in the UI language; `None` for [`InPlace::Write`].
    pub fn reason(self, t: &Strings) -> Option<&'static str> {
        match self {
            InPlace::Write(_) => None,
            InPlace::ArchivePage => Some(t.archive_page_save_as),
            InPlace::Animated => Some(t.animated_save_as),
            InPlace::NotWritable => Some(t.format_not_writable),
        }
    }
}

/// How the image at `path` can be saved in place. `format` is the format its content was
/// identified as (not the extension's), `animated` whether it has more than one frame.
pub fn in_place(encoders: &Encoders, path: &Path, format: FormatId, animated: bool) -> InPlace {
    if iv_pipeline::archive::split(path).is_some() {
        InPlace::ArchivePage
    } else if animated {
        InPlace::Animated
    } else {
        in_place_format(encoders, format).map_or(InPlace::NotWritable, InPlace::Write)
    }
}

/// The file name's extension names another format than the content's (a WebP called `.jpg`);
/// an extension that names no format is not a mismatch.
pub fn extension_mismatch(path: &Path, content: FormatId) -> bool {
    path.extension().and_then(|e| e.to_str()).and_then(FormatId::from_extension).is_some_and(|f| f != content)
}

/// Builds the request for saving `source` with `ops` to `dest`; the format follows `dest`'s
/// extension (Save As). `quality` applies to formats that have a quality option (JPEG, WebP,
/// AVIF).
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
    let options = if encoder.options().iter().any(|o| o.key == "quality") {
        EncodeOptions::parse(encoder.options(), &[format!("quality={}", quality.clamp(1, 100))])
            .map_err(SaveError::Options)?
    } else {
        EncodeOptions::default()
    };
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
        let InPlace::Write(format) = in_place(&enc, &webp_as_jpg, FormatId::Webp, false) else { panic!() };
        let r = save_request_as(&enc, &webp_as_jpg, &[Op::rotate_cw()], webp_as_jpg.clone(), format, 80).unwrap();
        assert_eq!((r.format, r.dest.as_path()), (FormatId::Webp, webp_as_jpg.as_path()));
        // a PNG with an unknown extension still saves (the extension is never consulted)
        let odd = dir.path().join("x.dat");
        assert_eq!(in_place(&enc, &odd, FormatId::Png, false), InPlace::Write(FormatId::Png));
        assert_eq!(save_request_as(&enc, &odd, &[], odd.clone(), FormatId::Png, 80).unwrap().format, FormatId::Png);
        assert!(extension_mismatch(&webp_as_jpg, FormatId::Webp));
        assert!(!extension_mismatch(Path::new("a.JPEG"), FormatId::Jpeg));
        assert!(!extension_mismatch(&odd, FormatId::Png), "an unknown extension is not a mismatch");
        // a JPEG named .heic is writable: its content is JPEG
        assert_eq!(in_place(&enc, &dir.path().join("x.heic"), FormatId::Jpeg, false), InPlace::Write(FormatId::Jpeg));
        assert_eq!(in_place(&enc, &dir.path().join("x.heic"), FormatId::Heif, false), InPlace::NotWritable);
    }

    #[test]
    fn animations_and_archive_pages_are_never_saved_in_place() {
        use crate::i18n::{EN, KO};
        let enc = Encoders::default();
        let dir = tempfile::tempdir().unwrap();
        // GIF, WebP and PNG can all be written, but only as one frame
        for f in [FormatId::Gif, FormatId::Webp, FormatId::Png] {
            assert_eq!(in_place(&enc, &dir.path().join("a.gif"), f, true), InPlace::Animated);
        }
        // a page inside a real archive has no file of its own
        let cbz = dir.path().join("book.cbz");
        std::fs::write(&cbz, b"PK").unwrap();
        let page = iv_pipeline::archive::entry_path(&cbz, "p/001.png");
        assert_eq!(in_place(&enc, &page, FormatId::Png, false), InPlace::ArchivePage);
        assert_eq!(in_place(&enc, &page, FormatId::Gif, true), InPlace::ArchivePage);
        // every refusal says why, in both languages
        for kind in [InPlace::ArchivePage, InPlace::Animated, InPlace::NotWritable] {
            assert!(kind.reason(&KO).is_some_and(|s| !s.is_empty()) && kind.reason(&EN).is_some());
        }
        assert_eq!(InPlace::Write(FormatId::Png).reason(&EN), None);
    }
}
