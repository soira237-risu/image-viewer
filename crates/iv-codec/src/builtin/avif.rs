//! AVIF through `rav1d` (a Rust port of dav1d), via `avif-decode`.
//!
//! The primary AVIF decoder everywhere. The Windows AV1 codec was measured to be wrong twice:
//! it drops alpha, and it decodes BT.601 files (libavif's default for 8-bit sRGB) with BT.709
//! coefficients, turning pure red into (255, 25, 0); ffmpeg and this path give (254, 0, 0).
//!
//! The container's rotation and mirroring (`irot`, `imir`) become the image's
//! [`Orientation`](iv_core::Orientation), as a JPEG's EXIF orientation does: pixels and size are
//! reported as stored, and the renderer and the save path turn them upright. An ICC profile in
//! `colr` is passed on for colour management. What is not read: [`ItemProperties`].

use avif_decode::Image;
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, PixelFormat, Result, Source};

use super::heif::ItemProperties;
use crate::decoder::{Decoded, Decoder, DecoderInfo};

const ID: &str = "rav1d";

pub struct Rav1dAvifDecoder {
    info: DecoderInfo,
}

impl Default for Rav1dAvifDecoder {
    fn default() -> Self {
        Self { info: DecoderInfo { id: ID, name: "rav1d (AVIF)", formats: &[FormatId::Avif], priority: 90 } }
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::decode(ID, e)
}

struct Header {
    width: u32,
    height: u32,
    bit_depth: u8,
    has_alpha: bool,
    props: ItemProperties,
}

/// Probe and decode both read the file through here, so they agree on size, orientation and
/// profile.
fn header(bytes: &[u8]) -> Result<Header> {
    let avif = avif_parse::read_avif(&mut &bytes[..]).map_err(err)?;
    let meta = avif.primary_item_metadata().map_err(err)?;
    Ok(Header {
        width: meta.max_frame_width.get(),
        height: meta.max_frame_height.get(),
        bit_depth: meta.bit_depth,
        has_alpha: avif.alpha_item.is_some(),
        props: ItemProperties::of_primary_item(bytes),
    })
}

/// `avif-decode` scales every 10/12-bit channel, colour and alpha, to the full 16-bit range
/// (measured: 10-bit red 220/255 comes out as 56503 ≈ 220 × 257), so values are taken as is.
/// Pixels are written straight into the image's one buffer.
fn to_image_buf(img: Image) -> Result<ImageBuf> {
    fn fill<T: bytemuck::Pod>(
        (w, h): (usize, usize),
        format: PixelFormat,
        px: impl Iterator<Item = [T; 4]>,
    ) -> Result<ImageBuf> {
        ImageBuf::with_samples::<T>(w as u32, h as u32, format, |out| {
            for (o, p) in out.as_chunks_mut::<4>().0.iter_mut().zip(px) {
                o.copy_from_slice(&p);
            }
        })
    }
    use PixelFormat::{Rgba8, Rgba16};
    match img {
        Image::Rgba8(i) => fill((i.width(), i.height()), Rgba8, i.pixels().map(|p| [p.r, p.g, p.b, p.a])),
        Image::Rgb8(i) => fill((i.width(), i.height()), Rgba8, i.pixels().map(|p| [p.r, p.g, p.b, 255])),
        Image::Gray8(i) => {
            fill((i.width(), i.height()), Rgba8, i.pixels().map(|g| [g.value(), g.value(), g.value(), 255]))
        }
        Image::Rgba16(i) => fill((i.width(), i.height()), Rgba16, i.pixels().map(|p| [p.r, p.g, p.b, p.a])),
        Image::Rgb16(i) => fill((i.width(), i.height()), Rgba16, i.pixels().map(|p| [p.r, p.g, p.b, u16::MAX])),
        Image::Gray16(i) => {
            fill((i.width(), i.height()), Rgba16, i.pixels().map(|g| [g.value(), g.value(), g.value(), u16::MAX]))
        }
    }
}

impl Decoder for Rav1dAvifDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo> {
        Ok(info_from(&header(&src.read_all()?)?, format))
    }

    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let bytes = src.read_all()?;
        let h = header(&bytes)?;
        let info = info_from(&h, format);
        ImageBuf::byte_len(h.width, h.height, if h.bit_depth > 8 { PixelFormat::Rgba16 } else { PixelFormat::Rgba8 })?;
        cancel.check()?;
        let img = avif_decode::Decoder::from_avif(&bytes).and_then(|d| d.to_image()).map_err(err)?;
        let image = to_image_buf(img)?;
        cancel.check()?;
        Ok(Decoded::still(info, image))
    }
}

fn info_from(h: &Header, format: FormatId) -> ImageInfo {
    ImageInfo {
        format,
        codec: ID,
        width: h.width,
        height: h.height,
        frame_count: Some(1),
        has_alpha: h.has_alpha,
        bits_per_channel: if h.bit_depth > 8 { 16 } else { 8 },
        orientation: h.props.orientation,
        icc_profile: h.props.icc.clone(),
        exif: None,
    }
}
