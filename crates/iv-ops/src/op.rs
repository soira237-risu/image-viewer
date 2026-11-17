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
        self.mix(rgb.map(|c| self.tone(c)))
    }

    /// Brightness, contrast and gamma: the per-channel part of [`Self::apply`].
    fn tone(&self, c: f32) -> f32 {
        let c = c + self.brightness;
        let c = ((c - 0.5) * (1.0 + self.contrast) + 0.5).clamp(0.0, 1.0);
        let gamma = self.gamma.clamp(0.1, 10.0);
        // x^1 is x: skip the power at the default gamma
        if gamma == 1.0 { c } else { c.powf(1.0 / gamma) }
    }

    /// Saturation around Rec.709 luma: the part of [`Self::apply`] that mixes the channels of
    /// toned values (all within 0..1). At zero saturation it leaves them as they are.
    fn mix(&self, [r, g, b]: [f32; 3]) -> [f32; 3] {
        if self.saturation == 0.0 {
            return [r, g, b];
        }
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let s = 1.0 + self.saturation;
        [r, g, b].map(|c| (luma + (c - luma) * s).clamp(0.0, 1.0))
    }
}

/// A chain of [`Adjust`]s prepared for integer channels of `N` levels (256 for 8-bit, 65536 for
/// 16-bit): exactly what folding [`Adjust::apply`] over `c / (N - 1)` gives, with the first
/// adjustment's tone curve looked up instead of computed per pixel. When no adjustment in the
/// chain changes saturation, the channels never mix and the whole chain is one table.
///
/// The screen preview and saving both map pixels through this, so what is shown is what is
/// saved.
pub struct ToneTable {
    first: Adjust,
    /// The first adjustment's tone curve at every input level.
    first_tone: Vec<f32>,
    rest: Vec<Adjust>,
    /// The whole chain per channel, when it never mixes channels.
    direct: Option<Vec<u16>>,
    max: f32,
}

impl ToneTable {
    /// `None` for an empty chain (nothing to do). `levels` is 256 or 65536.
    pub fn new(adjusts: &[Adjust], levels: usize) -> Option<Self> {
        let (&first, rest) = adjusts.split_first()?;
        let max = (levels - 1) as f32;
        let first_tone: Vec<f32> = (0..levels).map(|i| first.tone(i as f32 / max)).collect();
        let direct = adjusts.iter().all(|a| a.saturation == 0.0).then(|| {
            first_tone.iter().map(|&c| (rest.iter().fold(c, |c, a| a.tone(c)) * max).round() as u16).collect()
        });
        Some(Self { first, first_tone, rest: rest.to_vec(), direct, max })
    }

    /// Maps the colour channels of one pixel (each below the table's level count).
    #[inline]
    pub fn map(&self, rgb: [u16; 3]) -> [u16; 3] {
        if let Some(direct) = &self.direct {
            return rgb.map(|c| direct[c as usize]);
        }
        let toned = self.first.mix(rgb.map(|c| self.first_tone[c as usize]));
        let out = self.rest.iter().fold(toned, |c, a| a.apply(c));
        out.map(|c| (c * self.max).round() as u16)
    }

    /// [`Self::map`] for 8-bit channels (a table made with 256 levels).
    #[inline]
    pub fn map8(&self, rgb: [u8; 3]) -> [u8; 3] {
        self.map(rgb.map(u16::from)).map(|c| c as u8)
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

    /// The plain per-pixel arithmetic the table stands for.
    fn reference(adjusts: &[Adjust], rgb: [u16; 3], max: f32) -> [u16; 3] {
        let c = adjusts.iter().fold(rgb.map(|c| c as f32 / max), |c, a| a.apply(c));
        c.map(|c| (c * max).round() as u16)
    }

    #[test]
    fn tone_tables_give_exactly_the_per_pixel_result() {
        let chains: Vec<Vec<Adjust>> = vec![
            vec![Adjust { brightness: 0.1, contrast: 0.2, gamma: 1.3, saturation: 0.2 }],
            vec![Adjust { brightness: -0.2, contrast: 0.5, gamma: 0.6, ..Default::default() }],
            vec![Adjust { gamma: 2.2, ..Default::default() }, Adjust { contrast: -0.3, ..Default::default() }],
            vec![
                Adjust { saturation: -1.0, ..Default::default() },
                Adjust { brightness: 0.3, gamma: 1.7, saturation: 0.8, ..Default::default() },
            ],
            vec![Adjust::default()],
        ];
        for chain in &chains {
            let t8 = ToneTable::new(chain, 256).unwrap();
            for v in 0..=255u16 {
                // every level on each channel, against two different other channels
                for rgb in [[v, 0, 255], [128, v, 37], [200, 90, v]] {
                    assert_eq!(t8.map(rgb), reference(chain, rgb, 255.0), "{chain:?} {rgb:?}");
                }
            }
            let t16 = ToneTable::new(chain, 65536).unwrap();
            for v in (0..=65535u16).step_by(97).chain([65535]) {
                let rgb = [v, 30000, 65535 - v];
                assert_eq!(t16.map(rgb), reference(chain, rgb, 65535.0), "{chain:?} {rgb:?}");
            }
        }
        assert!(ToneTable::new(&[], 256).is_none());
    }

    #[test]
    fn the_identity_keeps_every_level() {
        for levels in [256, 65536] {
            let t = ToneTable::new(&[Adjust::default()], levels).unwrap();
            for v in (0..levels as u32).step_by(7).chain([levels as u32 - 1]) {
                let v = v as u16;
                assert_eq!(t.map([v, v / 2, v / 3]), [v, v / 2, v / 3]);
            }
        }
    }
}
