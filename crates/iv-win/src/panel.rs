//! The edit panel's state and the actions it reports; the window itself lives in `win::panel`.
//! The panel only reports what the user did; the app applies it.

use iv_ops::{Adjust, Op};

pub struct PanelState {
    pub open: bool,
    /// Slider values; mirrors the newest adjustment edit.
    pub adjust: Adjust,
    /// Resize fields; (0, 0) until an image size is known.
    pub resize: (u32, u32),
    pub keep_aspect: bool,
    /// JPEG / WebP / AVIF quality used by Save and Save As.
    pub quality: u8,
}

impl Default for PanelState {
    fn default() -> Self {
        Self { open: false, adjust: Adjust::default(), resize: (0, 0), keep_aspect: true, quality: 90 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PanelAction {
    /// The sliders moved: replace the adjustment being dragged.
    Adjust(Adjust),
    Edit(Op),
    Undo,
    Redo,
    Save,
    SaveAs,
    /// The save quality slider moved.
    Quality(u8),
    /// The user typed a width or height into the resize fields.
    ResizeWidth(u32),
    ResizeHeight(u32),
    /// Resize fields to half the current size.
    ResizeHalf,
    /// Resize the image to the size in the fields.
    ResizeApply,
    KeepAspect(bool),
    /// The panel was closed with its close button or Esc.
    Closed,
}

/// What the panel needs to know about the current image.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelContext {
    /// Size of the edited image, if one is shown.
    pub size: Option<(u32, u32)>,
    pub can_undo: bool,
    pub can_redo: bool,
    pub edited: bool,
}

/// Keeps the resize fields consistent: changing one side with "keep aspect" follows with the
/// other, from the current image's aspect ratio.
pub fn follow_aspect(changed_width: bool, value: (u32, u32), image: (u32, u32)) -> (u32, u32) {
    let (iw, ih) = (image.0.max(1) as f64, image.1.max(1) as f64);
    if changed_width {
        (value.0.max(1), ((value.0 as f64 * ih / iw).round() as u32).max(1))
    } else {
        (((value.1 as f64 * iw / ih).round() as u32).max(1), value.1.max(1))
    }
}

/// The value of a resize field as typed: 1..=65535, or 0 for empty, zero or out of range text
/// (which disables Apply rather than leaving the last valid number in effect).
pub fn field_value(text: &str) -> u32 {
    text.trim().parse::<u32>().ok().filter(|v| (1..=65535).contains(v)).unwrap_or(0)
}

/// The resize fields after the user typed `value` into the width (or height) field. With "keep
/// aspect", a valid value moves the other side too; 0 (an empty or invalid field) leaves the other
/// side alone, so a field being retyped does not reshape the one beside it.
pub fn typed_resize(
    current: (u32, u32),
    keep_aspect: bool,
    image: Option<(u32, u32)>,
    width_changed: bool,
    value: u32,
) -> (u32, u32) {
    match (keep_aspect && value > 0, image, width_changed) {
        (true, Some(size), true) => follow_aspect(true, (value, current.1), size),
        (true, Some(size), false) => follow_aspect(false, (current.0, value), size),
        (_, _, true) => (value, current.1),
        (_, _, false) => (current.0, value),
    }
}

/// Whether Apply would change anything: both fields valid and different from the image.
pub fn can_apply(resize: (u32, u32), image: Option<(u32, u32)>) -> bool {
    image.is_some() && image != Some(resize) && resize.0 > 0 && resize.1 > 0
}

/// Trackbar position (−100..=100) ↔ gamma (0.2..=5, logarithmic so 0 is 1.0).
pub fn gamma_from_position(pos: i32) -> f32 {
    5f32.powf(pos.clamp(-100, 100) as f32 / 100.0)
}

pub fn gamma_to_position(gamma: f32) -> i32 {
    (gamma.clamp(0.2, 5.0).ln() / 5f32.ln() * 100.0).round() as i32
}

/// Trackbar position (−100..=100) ↔ a −1..=1 adjustment.
pub fn unit_from_position(pos: i32) -> f32 {
    pos.clamp(-100, 100) as f32 / 100.0
}

pub fn unit_to_position(v: f32) -> i32 {
    (v.clamp(-1.0, 1.0) * 100.0).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_follows_the_edited_side() {
        assert_eq!(follow_aspect(true, (1920, 999), (3840, 2400)), (1920, 1200));
        assert_eq!(follow_aspect(false, (1, 600), (3840, 2400)), (960, 600));
        assert_eq!(follow_aspect(true, (0, 0), (10, 10)), (1, 1));
    }

    #[test]
    fn clearing_a_resize_field_disables_apply_and_keeps_the_other_side() {
        let image = Some((3840, 2400));
        let mut r = (1920, 1200);
        assert!(can_apply(r, image));
        // the user empties the width field: width 0, height untouched, Apply off
        r = typed_resize(r, true, image, true, field_value(""));
        assert_eq!(r, (0, 1200));
        assert!(!can_apply(r, image));
        // "0" and "70000" count as invalid too
        assert_eq!(field_value(" 0 "), 0);
        assert_eq!(field_value("70000"), 0);
        assert_eq!(typed_resize((0, 1200), true, image, false, field_value("abc")), (0, 0));
        // typing a valid width again follows the aspect ratio
        r = typed_resize((0, 1200), true, image, true, field_value("960"));
        assert_eq!(r, (960, 600));
        assert!(can_apply(r, image));
        // without keep-aspect only the typed side changes
        assert_eq!(typed_resize((960, 600), false, image, false, 1000), (960, 1000));
        assert!(!can_apply((3840, 2400), image), "same size: nothing to do");
        assert!(!can_apply((10, 10), None), "no image");
    }

    #[test]
    fn slider_positions_round_trip() {
        assert_eq!(gamma_from_position(0), 1.0);
        assert!((gamma_from_position(100) - 5.0).abs() < 1e-5);
        assert!((gamma_from_position(-100) - 0.2).abs() < 1e-5);
        for pos in [-100, -37, 0, 12, 100] {
            assert_eq!(gamma_to_position(gamma_from_position(pos)), pos);
            assert_eq!(unit_to_position(unit_from_position(pos)), pos);
        }
        assert_eq!(unit_from_position(250), 1.0, "clamped");
    }
}
