//! Edit operations and the undoable stack holding them.

use iv_core::Orientation;

/// A rectangle in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Tone adjustments, applied to sRGB-encoded values in 0..1, in this order:
/// brightness (added), contrast (around mid-grey), gamma, saturation (around Rec.709 luma).
/// All zero (gamma one) is the identity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Adjust {
    /// −1..1, added to every channel.
    pub brightness: f32,
    /// −1..1; the slope around 0.5 is `1 + contrast`.
    pub contrast: f32,
    /// 0.1..10; values are raised to `1 / gamma`.
    pub gamma: f32,
    /// −1..1; −1 is greyscale, 1 doubles colourfulness.
    pub saturation: f32,
}

impl Default for Adjust {
    fn default() -> Self {
        Self { brightness: 0.0, contrast: 0.0, gamma: 1.0, saturation: 0.0 }
    }
}

impl Adjust {
    pub fn is_identity(&self) -> bool {
        *self == Adjust::default()
    }

    /// Maps one pixel's colour channels (0..1); alpha is not touched.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let tone = |c: f32| {
            let c = c + self.brightness;
            let c = (c - 0.5) * (1.0 + self.contrast) + 0.5;
            c.clamp(0.0, 1.0).powf(1.0 / self.gamma.clamp(0.1, 10.0))
        };
        let [r, g, b] = rgb.map(tone);
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let s = 1.0 + self.saturation;
        [r, g, b].map(|c| (luma + (c - luma) * s).clamp(0.0, 1.0))
    }
}

/// One edit. Geometry is in the coordinates of the image as it looks *at that point* in the
/// stack (after the edits before it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    /// A rotation or flip, expressed as the orientation transform it applies.
    Orient(Orientation),
    Crop(Rect),
    Resize {
        width: u32,
        height: u32,
    },
    Adjust(Adjust),
}

impl Op {
    pub fn rotate_cw() -> Op {
        Op::Orient(Orientation::Rotate90)
    }
    pub fn rotate_ccw() -> Op {
        Op::Orient(Orientation::Rotate270)
    }
    pub fn flip_h() -> Op {
        Op::Orient(Orientation::FlipH)
    }
    pub fn flip_v() -> Op {
        Op::Orient(Orientation::FlipV)
    }

    /// Only rotations and flips: such an edit can be saved losslessly by changing a JPEG's EXIF
    /// orientation instead of re-encoding.
    pub fn is_orientation(&self) -> bool {
        matches!(self, Op::Orient(_))
    }
}

/// The edits made to one image, with undo/redo.
#[derive(Clone, Debug, Default)]
pub struct EditStack {
    ops: Vec<Op>,
    /// Number of ops in effect; the rest are redoable.
    cursor: usize,
}

impl EditStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an edit, discarding anything that could have been redone.
    pub fn push(&mut self, op: Op) {
        self.ops.truncate(self.cursor);
        self.ops.push(op);
        self.cursor += 1;
    }

    /// Replaces the newest edit when `same_kind` says it is the same kind (a slider still being
    /// dragged), so one undo reverts the whole drag; otherwise pushes.
    pub fn push_or_replace(&mut self, op: Op, same_kind: impl Fn(&Op) -> bool) {
        if self.cursor > 0 && self.cursor == self.ops.len() && same_kind(&self.ops[self.cursor - 1]) {
            self.ops[self.cursor - 1] = op;
        } else {
            self.push(op);
        }
    }

    pub fn undo(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.cursor == self.ops.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// The edits in effect, oldest first.
    pub fn active(&self) -> &[Op] {
        &self.ops[..self.cursor]
    }

    pub fn is_empty(&self) -> bool {
        self.cursor == 0
    }

    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_redo(&self) -> bool {
        self.cursor < self.ops.len()
    }

    pub fn clear(&mut self) {
        self.ops.clear();
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo_and_branching() {
        let mut s = EditStack::new();
        assert!(!s.undo());
        s.push(Op::rotate_cw());
        s.push(Op::flip_h());
        assert_eq!(s.active().len(), 2);
        assert!(s.undo());
        assert_eq!(s.active(), &[Op::rotate_cw()]);
        assert!(s.can_redo());
        assert!(s.redo());
        assert_eq!(s.active().len(), 2);
        assert!(!s.redo());
        s.undo();
        // a new edit after undo drops the redo branch
        s.push(Op::flip_v());
        assert_eq!(s.active(), &[Op::rotate_cw(), Op::flip_v()]);
        assert!(!s.can_redo());
    }

    #[test]
    fn slider_drags_collapse_into_one_edit() {
        let is_adjust = |o: &Op| matches!(o, Op::Adjust(_));
        let mut s = EditStack::new();
        s.push(Op::rotate_cw());
        for b in [0.1, 0.2, 0.3] {
            s.push_or_replace(Op::Adjust(Adjust { brightness: b, ..Default::default() }), is_adjust);
        }
        assert_eq!(s.active().len(), 2);
        assert_eq!(s.active()[1], Op::Adjust(Adjust { brightness: 0.3, ..Default::default() }));
        s.undo();
        assert_eq!(s.active(), &[Op::rotate_cw()], "one undo reverts the whole drag");
        // after an undo the redo branch is not overwritten in place
        s.push_or_replace(Op::Adjust(Adjust::default()), is_adjust);
        assert_eq!(s.active().len(), 2);
        assert!(!s.can_redo());
    }

    #[test]
    fn adjust_identity_and_extremes() {
        let id = Adjust::default();
        assert!(id.is_identity());
        let c = [0.2, 0.5, 0.9];
        let out = id.apply(c);
        for i in 0..3 {
            assert!((out[i] - c[i]).abs() < 1e-6);
        }
        let grey = Adjust { saturation: -1.0, ..Default::default() }.apply([1.0, 0.0, 0.0]);
        assert!((grey[0] - grey[1]).abs() < 1e-6 && (grey[1] - grey[2]).abs() < 1e-6, "{grey:?}");
        let bright = Adjust { brightness: 1.0, ..Default::default() }.apply([0.3, 0.3, 0.3]);
        assert_eq!(bright, [1.0, 1.0, 1.0]);
        let flat = Adjust { contrast: -1.0, ..Default::default() }.apply([0.0, 0.25, 1.0]);
        assert!(flat.iter().all(|v| (v - 0.5).abs() < 1e-6), "{flat:?}");
    }
}
