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
