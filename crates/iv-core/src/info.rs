use std::sync::Arc;

use crate::FormatId;

/// EXIF orientation (TIFF tag 0x0112). The stored pixels must be transformed this way to be
/// shown upright.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Orientation {
    /// 1: no transform.
    #[default]
    Normal,
    /// 2: mirror horizontally.
    FlipH,
    /// 3: rotate 180°.
    Rotate180,
    /// 4: mirror vertically.
    FlipV,
    /// 5: mirror horizontally, then rotate 270° clockwise (transpose).
    Transpose,
    /// 6: rotate 90° clockwise.
    Rotate90,
    /// 7: mirror horizontally, then rotate 90° clockwise (transverse).
    Transverse,
    /// 8: rotate 270° clockwise.
    Rotate270,
}

impl Orientation {
    /// Maps the EXIF value 1..=8; anything else is treated as [`Orientation::Normal`].
    pub fn from_exif(value: u32) -> Self {
        match value {
            2 => Orientation::FlipH,
            3 => Orientation::Rotate180,
            4 => Orientation::FlipV,
            5 => Orientation::Transpose,
            6 => Orientation::Rotate90,
            7 => Orientation::Transverse,
            8 => Orientation::Rotate270,
            _ => Orientation::Normal,
        }
    }

    pub fn to_exif(self) -> u16 {
        match self {
            Orientation::Normal => 1,
            Orientation::FlipH => 2,
            Orientation::Rotate180 => 3,
            Orientation::FlipV => 4,
            Orientation::Transpose => 5,
            Orientation::Rotate90 => 6,
            Orientation::Transverse => 7,
            Orientation::Rotate270 => 8,
        }
    }

    pub const ALL: [Orientation; 8] = [
        Orientation::Normal,
        Orientation::FlipH,
        Orientation::Rotate180,
        Orientation::FlipV,
        Orientation::Transpose,
        Orientation::Rotate90,
        Orientation::Transverse,
        Orientation::Rotate270,
    ];

    /// The transform as a 2×2 integer matrix acting on centred pixel coordinates (x right,
    /// y down): `shown = M · stored`.
    fn matrix(self) -> [[i8; 2]; 2] {
        match self {
            Orientation::Normal => [[1, 0], [0, 1]],
            Orientation::FlipH => [[-1, 0], [0, 1]],
            Orientation::Rotate180 => [[-1, 0], [0, -1]],
            Orientation::FlipV => [[1, 0], [0, -1]],
            Orientation::Transpose => [[0, 1], [1, 0]],
            Orientation::Rotate90 => [[0, -1], [1, 0]],
            Orientation::Transverse => [[0, -1], [-1, 0]],
            Orientation::Rotate270 => [[0, 1], [-1, 0]],
        }
    }

    fn from_matrix(m: [[i8; 2]; 2]) -> Self {
        Orientation::ALL
            .into_iter()
            .find(|o| o.matrix() == m)
            .expect("the eight orientations are closed under composition")
    }

    /// `self` followed by `next`: e.g. a photo stored with EXIF `Rotate90` that the user then
    /// flips horizontally is shown as `Rotate90.then(FlipH)`.
    pub fn then(self, next: Orientation) -> Orientation {
        let (a, b) = (self.matrix(), next.matrix());
        let mut m = [[0i8; 2]; 2];
        for (i, row) in m.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = b[i][0] * a[0][j] + b[i][1] * a[1][j];
            }
        }
        Orientation::from_matrix(m)
    }

    /// Affine map from upright (displayed) UV, 0..1 over the shown image, to stored UV:
    /// `stored = [a b c; d e f] · [u v 1]`.
    pub fn display_to_stored_uv(self) -> [[f64; 3]; 2] {
        match self {
            Orientation::Normal => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            Orientation::FlipH => [[-1.0, 0.0, 1.0], [0.0, 1.0, 0.0]],
            Orientation::Rotate180 => [[-1.0, 0.0, 1.0], [0.0, -1.0, 1.0]],
            Orientation::FlipV => [[1.0, 0.0, 0.0], [0.0, -1.0, 1.0]],
            Orientation::Transpose => [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
            Orientation::Rotate90 => [[0.0, 1.0, 0.0], [-1.0, 0.0, 1.0]],
            Orientation::Transverse => [[0.0, -1.0, 1.0], [-1.0, 0.0, 1.0]],
            Orientation::Rotate270 => [[0.0, -1.0, 1.0], [1.0, 0.0, 0.0]],
        }
    }

    /// The transform that undoes `self`.
    pub fn inverse(self) -> Orientation {
        Orientation::ALL
            .into_iter()
            .find(|o| self.then(*o) == Orientation::Normal)
            .expect("every orientation is invertible")
    }

    /// Whether showing the image upright swaps its width and height.
    pub fn swaps_dimensions(self) -> bool {
        matches!(
            self,
            Orientation::Transpose | Orientation::Rotate90 | Orientation::Transverse | Orientation::Rotate270
        )
    }
}

/// What a codec learns about an image without decoding its pixels.
#[derive(Clone, Debug)]
pub struct ImageInfo {
    pub format: FormatId,
    /// Id of the codec that produced this info.
    pub codec: &'static str,
    /// Stored pixel width (before orientation is applied).
    pub width: u32,
    /// Stored pixel height (before orientation is applied).
    pub height: u32,
    /// Number of frames: `Some(1)` for still images, `None` when counting would require
    /// decoding the whole stream (e.g. GIF).
    pub frame_count: Option<u32>,
    pub has_alpha: bool,
    /// Bits per channel of the stored data (8, 16, 32 …).
    pub bits_per_channel: u8,
    pub orientation: Orientation,
    /// Embedded ICC profile, if any.
    pub icc_profile: Option<Arc<[u8]>>,
}

impl ImageInfo {
    /// Size as displayed, after applying the orientation.
    pub fn display_size(&self) -> (u32, u32) {
        if self.orientation.swaps_dimensions() { (self.height, self.width) } else { (self.width, self.height) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_round_trip() {
        for v in 1..=8 {
            assert_eq!(Orientation::from_exif(v).to_exif() as u32, v);
        }
        assert_eq!(Orientation::from_exif(0), Orientation::Normal);
        assert_eq!(Orientation::from_exif(9), Orientation::Normal);
    }

    #[test]
    fn composition_follows_the_group_laws() {
        use Orientation::*;
        assert_eq!(Rotate90.then(Rotate90), Rotate180);
        assert_eq!(Rotate90.then(Rotate90).then(Rotate90).then(Rotate90), Normal);
        assert_eq!(Rotate90.then(Rotate270), Normal);
        assert_eq!(FlipH.then(FlipH), Normal);
        assert_eq!(FlipH.then(Rotate180), FlipV);
        // EXIF 5 is "mirror horizontally, then rotate 270° clockwise"; 7 is mirror then 90°
        assert_eq!(FlipH.then(Rotate270), Transpose);
        assert_eq!(FlipH.then(Rotate90), Transverse);
        for a in Orientation::ALL {
            assert_eq!(a.then(a.inverse()), Normal, "{a:?}");
            assert_eq!(a.inverse().then(a), Normal, "{a:?}");
            assert_eq!(Normal.then(a), a);
            for b in Orientation::ALL {
                for c in Orientation::ALL {
                    assert_eq!(a.then(b).then(c), a.then(b.then(c)), "associative");
                }
                assert_eq!(a.then(b).swaps_dimensions(), a.swaps_dimensions() != b.swaps_dimensions());
            }
        }
    }

    #[test]
    fn rotated_images_swap_display_size() {
        let mut info = ImageInfo {
            format: FormatId::Jpeg,
            codec: "test",
            width: 400,
            height: 300,
            frame_count: Some(1),
            has_alpha: false,
            bits_per_channel: 8,
            orientation: Orientation::Rotate90,
            icc_profile: None,
        };
        assert_eq!(info.display_size(), (300, 400));
        info.orientation = Orientation::Rotate180;
        assert_eq!(info.display_size(), (400, 300));
    }
}
