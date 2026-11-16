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
    let mut opt = usvg::Options {
        // Only images embedded as `data:` URLs are drawn. By default usvg reads any other href
        // as a file path: an SVG from the internet could pull local files into the picture, or
        // make Windows connect to an SMB share (`\\host\x.png`) and send the user's credentials,
        // and prefetching would do that without the file even being opened.
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };
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
        exif: None,
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

    fn green_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 255, 0, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn base64(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in bytes.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        s
    }

    /// Alpha at the centre of an SVG whose only content is `<image href="{href}">`.
    fn centre_alpha(href: &str) -> u8 {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1024" height="1024">
                <image x="0" y="0" width="1024" height="1024" xlink:href="{href}"/>
            </svg>"#
        );
        let d = SvgDecoder::default()
            .decode(&Source::from_bytes("a.svg", svg.into_bytes()), FormatId::Svg, &CancelToken::never())
            .unwrap();
        d.image.data()[((512 * 1024 + 512) * 4 + 3) as usize]
    }

    #[test]
    fn external_image_files_are_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("secret.png");
        std::fs::write(&png, green_png()).unwrap();
        // absolute path, file URL and a relative name resolved from the working directory
        assert_eq!(centre_alpha(&png.display().to_string()), 0, "absolute path");
        assert_eq!(centre_alpha(&format!("file:///{}", png.display().to_string().replace('\\', "/"))), 0);
        // relative to the working directory (the temp folder can be on another drive, as on CI,
        // so this file goes in a short-lived folder under the working directory)
        let cwd = std::env::current_dir().unwrap();
        let local = tempfile::tempdir_in(&cwd).unwrap();
        std::fs::write(local.path().join("secret.png"), green_png()).unwrap();
        let rel = local.path().join("secret.png").strip_prefix(&cwd).unwrap().to_path_buf();
        assert!(rel.is_relative() && rel.is_file());
        assert_eq!(centre_alpha(&rel.display().to_string()), 0, "relative path");
        // an embedded image is part of the document and still drawn
        assert_eq!(centre_alpha(&format!("data:image/png;base64,{}", base64(&green_png()))), 255);
    }
}
