//! AVIF through `rav1d` (a Rust port of dav1d), via `avif-decode`.
//!
//! On Windows this decoder only claims AVIF files with an alpha channel: the system AV1 codec
//! drops alpha, but unlike this path it applies the container's rotation and reports the ICC
//! profile, so opaque files stay with WIC. Elsewhere it decodes every AVIF.

use avif_decode::Image;
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Orientation, PixelFormat, Result, Source};

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
}

fn header(bytes: &[u8]) -> Result<Header> {
    let avif = avif_parse::read_avif(&mut &bytes[..]).map_err(err)?;
    let meta = avif.primary_item_metadata().map_err(err)?;
    Ok(Header {
        width: meta.max_frame_width.get(),
        height: meta.max_frame_height.get(),
        bit_depth: meta.bit_depth,
        has_alpha: avif.alpha_item.is_some(),
    })
}

/// `avif-decode` scales every 10/12-bit channel, colour and alpha, to the full 16-bit range
/// (measured: 10-bit red 220/255 comes out as 56503 ≈ 220 × 257), so values are taken as is.
fn to_image_buf(img: Image) -> Result<ImageBuf> {
    let rgba8 =
        |w: usize, h: usize, px: Vec<[u8; 4]>| ImageBuf::new(w as u32, h as u32, PixelFormat::Rgba8, px.concat());
    let rgba16 = |w: usize, h: usize, px: Vec<[u16; 4]>| {
        ImageBuf::new(w as u32, h as u32, PixelFormat::Rgba16, bytemuck::cast_slice::<[u16; 4], u8>(&px).to_vec())
    };
    match img {
        Image::Rgba8(i) => {
            let (w, h) = (i.width(), i.height());
            rgba8(w, h, i.pixels().map(|p| [p.r, p.g, p.b, p.a]).collect())
        }
        Image::Rgb8(i) => {
            let (w, h) = (i.width(), i.height());
            rgba8(w, h, i.pixels().map(|p| [p.r, p.g, p.b, 255]).collect())
        }
        Image::Gray8(i) => {
            let (w, h) = (i.width(), i.height());
            rgba8(w, h, i.pixels().map(|g| [g.value(), g.value(), g.value(), 255]).collect())
        }
        Image::Rgba16(i) => {
            let (w, h) = (i.width(), i.height());
            rgba16(w, h, i.pixels().map(|p| [p.r, p.g, p.b, p.a]).collect())
        }
        Image::Rgb16(i) => {
            let (w, h) = (i.width(), i.height());
            rgba16(w, h, i.pixels().map(|p| [p.r, p.g, p.b, u16::MAX]).collect())
        }
        Image::Gray16(i) => {
            let (w, h) = (i.width(), i.height());
            rgba16(w, h, i.pixels().map(|g| [g.value(), g.value(), g.value(), u16::MAX]).collect())
        }
    }
}

impl Decoder for Rav1dAvifDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn claims(&self, src: &Source, _format: FormatId) -> Result<bool> {
        if cfg!(windows) { Ok(header(&src.read_all()?)?.has_alpha) } else { Ok(true) }
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
        orientation: Orientation::Normal,
        icc_profile: None,
    }
}
