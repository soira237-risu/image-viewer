//! The viewer's side of editing: turning a screen selection into a crop, and building save
//! requests. Kept free of window state so it can be tested directly.

use std::path::{Path, PathBuf};

use crate::view::{ScreenRect, ViewState};
use iv_codec::encode::{EncodeOptions, Encoders};
use iv_core::FormatId;
use iv_ops::{Op, Rect};
use iv_pipeline::export::ExportRequest;

/// A screen rectangle from two drag corners, in any order.
pub fn drag_rect(a: (f64, f64), b: (f64, f64)) -> ScreenRect {
    ScreenRect { x: a.0.min(b.0), y: a.1.min(b.1), width: (a.0 - b.0).abs(), height: (a.1 - b.1).abs() }
}

/// The part of the shown image under `sel`, in the shown image's pixels (which is what
/// [`Op::Crop`] expects). `None` when the selection misses the image or is under a pixel.
pub fn selection_to_crop(view: &ViewState, sel: ScreenRect) -> Option<Rect> {
    let (iw, ih) = view.image_size();
    let a = view.screen_to_image((sel.x, sel.y));
    let b = view.screen_to_image((sel.x + sel.width, sel.y + sel.height));
    let x0 = a.0.clamp(0.0, iw).round();
    let y0 = a.1.clamp(0.0, ih).round();
    let x1 = b.0.clamp(0.0, iw).round();
    let y1 = b.1.clamp(0.0, ih).round();
    (x1 - x0 >= 1.0 && y1 - y0 >= 1.0).then_some(Rect {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}

/// Format to use when saving over `format` in place, if it can be written.
pub fn in_place_format(encoders: &Encoders, format: FormatId) -> Option<FormatId> {
    encoders.for_format(format).map(|_| format)
}

/// Builds the request for saving `source` with `ops` to `dest`; the format follows `dest`'s
/// extension. `quality` applies to formats that have a quality option (JPEG, WebP, AVIF).
pub fn save_request(
    encoders: &Encoders,
    source: &Path,
    ops: &[Op],
    dest: PathBuf,
    quality: u8,
) -> Result<ExportRequest, String> {
    let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or_default().to_string();
    let format = FormatId::from_extension(&ext).ok_or_else(|| format!("unknown file type '.{ext}'"))?;
    let Some(encoder) = encoders.for_format(format) else {
        return Err(format!("{format} files cannot be written"));
    };
    let options = if encoder.options().iter().any(|o| o.key == "quality") {
        EncodeOptions::parse(encoder.options(), &[format!("quality={}", quality.clamp(1, 100))])?
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
        assert_eq!(drag_rect((10.0, 20.0), (4.0, 2.0)), ScreenRect { x: 4.0, y: 2.0, width: 6.0, height: 18.0 });
    }

    #[test]
    fn selection_maps_through_zoom_and_clamps_to_the_image() {
        // 200x100 image at 100 % in a 400x300 viewport sits at (100, 100)
        let mut v = ViewState::new((200, 100), (400, 300));
        v.actual_size_at((200.0, 150.0));
        let r = selection_to_crop(&v, ScreenRect { x: 110.0, y: 120.0, width: 50.0, height: 30.0 }).unwrap();
        assert_eq!(r, Rect { x: 10, y: 20, width: 50, height: 30 });
        // dragging past the image edge stops at the edge
        let edge = selection_to_crop(&v, ScreenRect { x: 250.0, y: 50.0, width: 500.0, height: 500.0 }).unwrap();
        assert_eq!(edge, Rect { x: 150, y: 0, width: 50, height: 100 });
        // entirely outside the image
        assert!(selection_to_crop(&v, ScreenRect { x: 0.0, y: 0.0, width: 50.0, height: 50.0 }).is_none());
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
        assert!(
            save_request(&enc, Path::new("a.jpg"), &[], PathBuf::from("out.heic"), 90)
                .unwrap_err()
                .contains("cannot be written")
        );
        assert_eq!(in_place_format(&enc, FormatId::Heif), None);
        assert_eq!(in_place_format(&enc, FormatId::Png), Some(FormatId::Png));
    }
}
