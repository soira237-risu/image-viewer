//! JPEG XL via `jxl-oxide` (pure Rust). Orientation is applied while rendering, so the reported
//! size is the upright size and `Orientation::Normal` is correct.

use std::io::Read;
use std::sync::Arc;

use image::{DynamicImage, ImageDecoder};
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Orientation, PixelFormat, Result, Source};
use jxl_oxide::integration::JxlDecoder;
use jxl_oxide::{InitializeResult, JxlImage};

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

/// Whether the file's image header declares an animation. Reads only as far as the header (and
/// an embedded colour profile or preview frame that comes with it), like [`JxlDecoder::new`],
/// which does not tell.
fn is_animated(src: &Source) -> Result<bool> {
    let mut reader = src.reader()?;
    let mut uninit = JxlImage::builder().build_uninit();
    let mut buf = vec![0u8; 4096];
    // bytes in `buf` the container parser has not taken yet (part of a box header)
    let mut valid = 0;
    loop {
        let read = reader.read(&mut buf[valid..])?;
        if read == 0 {
            return Err(err("the file ends before its image header"));
        }
        valid += read;
        let consumed = uninit.feed_bytes(&buf[..valid]).map_err(err)?;
        buf.copy_within(consumed..valid, 0);
        valid -= consumed;
        match uninit.try_init().map_err(err)? {
            InitializeResult::NeedMoreData(more) => uninit = more,
            InitializeResult::Initialized(image) => return Ok(image.image_header().metadata.animation.is_some()),
        }
    }
}

fn info_from(dec: &mut JxlDecoder<iv_core::SourceReader>, format: FormatId, animated: bool) -> ImageInfo {
    let (width, height) = dec.dimensions();
    let (bits_per_channel, has_alpha) = color_traits(dec.color_type());
    ImageInfo {
        format,
        codec: ID,
        width,
        height,
        // An animated JXL is shown as its first frame, and its frame count is only known after
        // reading every frame: unknown, which saving treats as more than one (writing it back
        // would drop the other frames). A still one has exactly one.
        frame_count: if animated { None } else { Some(1) },
        has_alpha,
        bits_per_channel,
        orientation: Orientation::Normal,
        icc_profile: dec.icc_profile().ok().flatten().map(Arc::from),
        exif: None,
    }
}

impl Decoder for JxlOxideDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo> {
        let mut dec = JxlDecoder::new(src.reader()?).map_err(err)?;
        Ok(info_from(&mut dec, format, is_animated(src)?))
    }

    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let mut dec = JxlDecoder::new(src.reader()?).map_err(err)?;
        let info = info_from(&mut dec, format, is_animated(src)?);
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
