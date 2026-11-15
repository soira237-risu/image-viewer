//! JPEG via `zune-jpeg`: SIMD-accelerated, pure Rust, decodes straight to RGBA.

use std::sync::Arc;

use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, PixelFormat, Result, Source};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use super::exif;
use crate::decoder::{Decoded, Decoder, DecoderInfo};

const ID: &str = "zune-jpeg";

/// JPEG's own limit is 65535 per side; zune defaults to 16384, which would refuse panoramas.
const MAX_SIDE: usize = u16::MAX as usize;

pub struct ZuneJpegDecoder {
    info: DecoderInfo,
}

impl Default for ZuneJpegDecoder {
    fn default() -> Self {
        Self { info: DecoderInfo { id: ID, name: "zune-jpeg", formats: &[FormatId::Jpeg], priority: 100 } }
    }
}

fn options() -> DecoderOptions {
    DecoderOptions::default().set_max_width(MAX_SIDE).set_max_height(MAX_SIDE).jpeg_set_out_colorspace(ColorSpace::RGBA)
}

fn err(e: impl std::fmt::Debug) -> Error {
    Error::decode(ID, format!("{e:?}"))
}

/// Builds the info from a decoder whose headers are decoded.
fn info_from<T: zune_core::bytestream::ZByteReaderTrait>(dec: &JpegDecoder<T>) -> Result<ImageInfo> {
    let (w, h) = dec.dimensions().ok_or_else(|| err("headers not decoded"))?;
    let parsed_exif = dec.exif().and_then(|raw| exif::parse(raw));
    Ok(ImageInfo {
        format: FormatId::Jpeg,
        codec: ID,
        width: w as u32,
        height: h as u32,
        frame_count: Some(1),
        has_alpha: false,
        bits_per_channel: 8,
        orientation: parsed_exif.as_ref().map(exif::orientation).unwrap_or_default(),
        icc_profile: dec.icc_profile().map(Arc::from),
        exif: dec.exif().map(|e| Arc::from(e.as_slice())),
    })
}

fn decode_bytes(bytes: &[u8]) -> Result<(ImageInfo, ImageBuf)> {
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(bytes), options());
    dec.decode_headers().map_err(err)?;
    let info = info_from(&dec)?;
    ImageBuf::byte_len(info.width, info.height, PixelFormat::Rgba8)?;
    let pixels = dec.decode().map_err(err)?;
    let image = ImageBuf::new(info.width, info.height, PixelFormat::Rgba8, pixels)?;
    Ok((info, image))
}

impl Decoder for ZuneJpegDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, _format: FormatId) -> Result<ImageInfo> {
        // headers only: the reader stops at the first scan, so huge files cost a few KB
        let mut dec = JpegDecoder::new_with_options(src.reader()?, options());
        dec.decode_headers().map_err(err)?;
        info_from(&dec)
    }

    fn preview(
        &self,
        src: &Source,
        _format: FormatId,
        _target_side: u32,
        cancel: &CancelToken,
    ) -> Result<Option<ImageBuf>> {
        let mut dec = JpegDecoder::new_with_options(src.reader()?, options());
        dec.decode_headers().map_err(err)?;
        let Some(parsed) = dec.exif().and_then(|raw| exif::parse(raw)) else { return Ok(None) };
        let Some(thumb) = exif::jpeg_thumbnail(&parsed) else { return Ok(None) };
        cancel.check()?;
        let (_, image) = decode_bytes(thumb)?;
        Ok(Some(image))
    }

    fn decode(&self, src: &Source, _format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let bytes = src.read_all()?;
        cancel.check()?;
        let (info, image) = decode_bytes(&bytes)?;
        cancel.check()?;
        Ok(Decoded::still(info, image))
    }
}
