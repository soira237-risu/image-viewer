//! SVG / SVGZ rasterised with `resvg`.
//!
//! A vector image has no pixel size of its own, so it is rendered at its declared size but with
//! the long side at least [`MIN_LONG_SIDE`] (small icons stay sharp when fitted to the window)
//! and at most [`MAX_LONG_SIDE`].

use std::sync::{Arc, OnceLock};

use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Orientation, PixelFormat, Result, Source};
use resvg::{tiny_skia, usvg};

use crate::decoder::{Decoded, Decoder, DecoderInfo};

const ID: &str = "resvg";
pub const MIN_LONG_SIDE: f32 = 1024.0;
pub const MAX_LONG_SIDE: f32 = 8192.0;

pub struct SvgDecoder {
    info: DecoderInfo,
}

impl Default for SvgDecoder {
    fn default() -> Self {
        Self { info: DecoderInfo { id: ID, name: "resvg", formats: &[FormatId::Svg], priority: 100 } }
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::decode(ID, e)
}

/// System fonts, loaded once and only for documents that contain text (loading costs ~100 ms).
fn system_fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

fn parse(bytes: &[u8]) -> Result<usvg::Tree> {
    let mut opt = usvg::Options::default();
    // SVGZ is gzip; look for text in the decompressed form only when cheap to tell
    let has_text = bytes.windows(5).any(|w| w == b"<text") || bytes.starts_with(&[0x1F, 0x8B]);
    if has_text {
        opt.fontdb = system_fonts();
    }
    usvg::Tree::from_data(bytes, &opt).map_err(err)
}

/// Raster size for a document of `size` (declared CSS pixels).
pub fn raster_size(width: f32, height: f32) -> (u32, u32) {
    let long = width.max(height).max(1.0);
    let scale = long.clamp(MIN_LONG_SIDE, MAX_LONG_SIDE) / long;
    (((width * scale).round() as u32).max(1), ((height * scale).round() as u32).max(1))
}

fn info_from(tree: &usvg::Tree, format: FormatId) -> ImageInfo {
    let size = tree.size();
    let (width, height) = raster_size(size.width(), size.height());
    ImageInfo {
        format,
        codec: ID,
        width,
        height,
        frame_count: Some(1),
        has_alpha: true,
        bits_per_channel: 8,
        orientation: Orientation::Normal,
        icc_profile: None,
    }
}

impl Decoder for SvgDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo> {
        Ok(info_from(&parse(&src.read_all()?)?, format))
    }

    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let tree = parse(&src.read_all()?)?;
        let info = info_from(&tree, format);
        ImageBuf::byte_len(info.width, info.height, PixelFormat::Rgba8)?;
        let mut pixmap =
            tiny_skia::Pixmap::new(info.width, info.height).ok_or_else(|| err("cannot allocate canvas"))?;
        let size = tree.size();
        let transform =
            tiny_skia::Transform::from_scale(info.width as f32 / size.width(), info.height as f32 / size.height());
        cancel.check()?;
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        // tiny-skia stores premultiplied alpha; the pipeline uses straight alpha
        let rgba: Vec<u8> = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        let image = ImageBuf::new(info.width, info.height, PixelFormat::Rgba8, rgba)?;
        Ok(Decoded::still(info, image))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_documents_are_enlarged_and_huge_ones_capped() {
        assert_eq!(raster_size(24.0, 12.0), (1024, 512));
        assert_eq!(raster_size(2000.0, 1000.0), (2000, 1000));
        assert_eq!(raster_size(20000.0, 10000.0), (8192, 4096));
    }

    #[test]
    fn renders_shapes_with_straight_alpha() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
            <rect x="0" y="0" width="50" height="50" fill="#ff0000"/>
            <rect x="50" y="0" width="50" height="50" fill="#0000ff" fill-opacity="0.5"/>
        </svg>"##;
        let d = SvgDecoder::default()
            .decode(&Source::from_bytes("a.svg", svg.to_vec()), FormatId::Svg, &CancelToken::never())
            .unwrap();
        assert_eq!((d.image.width(), d.image.height()), (1024, 512));
        let px = |x: u32, y: u32| {
            let i = ((y * 1024 + x) * 4) as usize;
            d.image.data()[i..i + 4].to_vec()
        };
        assert_eq!(px(100, 256), vec![255, 0, 0, 255]);
        let half = px(900, 256);
        assert_eq!(&half[..3], &[0, 0, 255], "straight colour, not darkened by premultiplication");
        assert!((half[3] as i32 - 128).abs() <= 1, "{half:?}");
    }
}
