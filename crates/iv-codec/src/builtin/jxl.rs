//! JPEG XL via `jxl-oxide` (pure Rust). Orientation is applied while rendering, so the reported
//! size is the upright size and `Orientation::Normal` is correct.

use std::sync::Arc;

use image::{DynamicImage, ImageDecoder};
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Orientation, PixelFormat, Result, Source};
use jxl_oxide::integration::JxlDecoder;

use super::image_rs::{color_traits, to_image_buf};
use crate::decoder::{Decoded, Decoder, DecoderInfo};

const ID: &str = "jxl-oxide";

pub struct JxlOxideDecoder {
    info: DecoderInfo,
}

impl Default for JxlOxideDecoder {
    fn default() -> Self {
        Self { info: DecoderInfo { id: ID, name: "jxl-oxide", formats: &[FormatId::Jxl], priority: 100 } }
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::decode(ID, e)
}

fn info_from(dec: &mut JxlDecoder<iv_core::SourceReader>, format: FormatId) -> ImageInfo {
    let (width, height) = dec.dimensions();
    let (bits_per_channel, has_alpha) = color_traits(dec.color_type());
    ImageInfo {
        format,
        codec: ID,
        width,
        height,
        // animated JXL is shown as its first frame for now
        frame_count: None,
        has_alpha,
        bits_per_channel,
        orientation: Orientation::Normal,
        icc_profile: dec.icc_profile().ok().flatten().map(Arc::from),
    }
}

impl Decoder for JxlOxideDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo> {
        let mut dec = JxlDecoder::new(src.reader()?).map_err(err)?;
        Ok(info_from(&mut dec, format))
    }

    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let mut dec = JxlDecoder::new(src.reader()?).map_err(err)?;
        let info = info_from(&mut dec, format);
        let out = match info.bits_per_channel {
            16 => PixelFormat::Rgba16,
            32 => PixelFormat::RgbaF32,
            _ => PixelFormat::Rgba8,
        };
        ImageBuf::byte_len(info.width, info.height, out)?;
        let img = DynamicImage::from_decoder(dec).map_err(err)?;
        cancel.check()?;
        let image = to_image_buf(img)?;
        cancel.check()?;
        Ok(Decoded::still(info, image))
    }
}
