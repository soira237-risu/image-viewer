//! The `image` crate's pure-Rust decoders: PNG, GIF, WebP, BMP, ICO, TIFF, TGA, PNM, QOI, DDS,
//! OpenEXR, Radiance HDR — and JPEG as a fallback behind zune-jpeg.

use std::sync::Arc;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use iv_core::{
    CancelToken, Error, FormatId, ImageBuf, ImageInfo, MAX_BUFFER_BYTES, Orientation, PixelFormat, Result, Source,
};

use crate::decoder::{Animation, AnimationFrame, Decoded, Decoder, DecoderInfo};

const ID: &str = "image-rs";

const FORMATS: &[FormatId] = &[
    FormatId::Png,
    FormatId::Gif,
    FormatId::Webp,
    FormatId::Bmp,
    FormatId::Ico,
    FormatId::Tiff,
    FormatId::Tga,
    FormatId::Pnm,
    FormatId::Qoi,
    FormatId::Dds,
    FormatId::OpenExr,
    FormatId::Hdr,
    FormatId::Jpeg,
];

pub struct ImageRsDecoder {
    info: DecoderInfo,
}

impl Default for ImageRsDecoder {
    fn default() -> Self {
        // below zune-jpeg for JPEG; the only decoder for the rest so far
        Self { info: DecoderInfo { id: ID, name: "image (Rust)", formats: FORMATS, priority: 50 } }
    }
}

fn image_format(format: FormatId) -> Result<ImageFormat> {
    Ok(match format {
        FormatId::Png => ImageFormat::Png,
        FormatId::Gif => ImageFormat::Gif,
        FormatId::Webp => ImageFormat::WebP,
        FormatId::Bmp => ImageFormat::Bmp,
        FormatId::Ico => ImageFormat::Ico,
        FormatId::Tiff => ImageFormat::Tiff,
        FormatId::Tga => ImageFormat::Tga,
        FormatId::Pnm => ImageFormat::Pnm,
        FormatId::Qoi => ImageFormat::Qoi,
        FormatId::Dds => ImageFormat::Dds,
        FormatId::OpenExr => ImageFormat::OpenExr,
        FormatId::Hdr => ImageFormat::Hdr,
        FormatId::Jpeg => ImageFormat::Jpeg,
        other => return Err(Error::NoDecoder { format: other, reason: format!("{ID} does not handle {other}") }),
    })
}

fn err(e: image::ImageError) -> Error {
    match e {
        image::ImageError::IoError(io) => Error::Io(io),
        other => Error::decode(ID, other),
    }
}

pub(crate) fn limits() -> Limits {
    let mut l = Limits::default();
    // the default 512 MiB allocation cap would refuse ordinary 16-bit panoramas
    l.max_alloc = Some(MAX_BUFFER_BYTES);
    l
}

fn orientation(o: image::metadata::Orientation) -> Orientation {
    Orientation::from_exif(o.to_exif() as u32)
}

/// Bits per channel and alpha for the stored data.
pub(crate) fn color_traits(c: ColorType) -> (u8, bool) {
    let bits = (c.bytes_per_pixel() / c.channel_count()) * 8;
    (bits, c.has_alpha())
}

/// Frame count that can be known from headers alone.
fn frame_count(src: &Source, format: FormatId) -> Result<Option<u32>> {
    Ok(match format {
        FormatId::Gif => None,
        FormatId::Webp => {
            let dec = WebPDecoder::new(src.reader()?).map_err(err)?;
            if dec.has_animation() { None } else { Some(1) }
        }
        FormatId::Png => {
            let dec = PngDecoder::new(src.reader()?).map_err(err)?;
            if dec.is_apng().map_err(err)? { None } else { Some(1) }
        }
        _ => Some(1),
    })
}

fn info_from(dec: &mut impl ImageDecoder, src: &Source, format: FormatId) -> Result<ImageInfo> {
    let (width, height) = dec.dimensions();
    let (bits_per_channel, has_alpha) = color_traits(dec.color_type());
    Ok(ImageInfo {
        format,
        codec: ID,
        width,
        height,
        frame_count: frame_count(src, format)?,
        has_alpha,
        bits_per_channel,
        // a malformed orientation tag is not worth failing the image over
        orientation: dec.orientation().map(orientation).unwrap_or_default(),
        icc_profile: dec.icc_profile().ok().flatten().map(Arc::from),
    })
}

fn open(src: &Source, format: FormatId) -> Result<impl ImageDecoder> {
    let mut reader = ImageReader::with_format(src.reader()?, image_format(format)?);
    reader.limits(limits());
    reader.into_decoder().map_err(err)
}

/// Converts to the pipeline's RGBA layouts, keeping 16-bit and float precision.
pub(crate) fn to_image_buf(img: DynamicImage) -> Result<ImageBuf> {
    let (w, h) = (img.width(), img.height());
    match img.color() {
        ColorType::Rgb16 | ColorType::Rgba16 | ColorType::L16 | ColorType::La16 => {
            ImageBuf::byte_len(w, h, PixelFormat::Rgba16)?;
            let raw = img.into_rgba16().into_raw();
            ImageBuf::new(w, h, PixelFormat::Rgba16, bytemuck::cast_slice::<u16, u8>(&raw).to_vec())
        }
        ColorType::Rgb32F | ColorType::Rgba32F => {
            ImageBuf::byte_len(w, h, PixelFormat::RgbaF32)?;
            let raw = img.into_rgba32f().into_raw();
            ImageBuf::new(w, h, PixelFormat::RgbaF32, bytemuck::cast_slice::<f32, u8>(&raw).to_vec())
        }
        _ => {
            ImageBuf::byte_len(w, h, PixelFormat::Rgba8)?;
            ImageBuf::new(w, h, PixelFormat::Rgba8, img.into_rgba8().into_raw())
        }
    }
}

/// Upper bound for the pixels of all frames of one animation. Longer animations are shown as
/// their first frame rather than truncated (a cut animation would loop at the wrong point).
pub const MAX_ANIMATION_BYTES: u64 = 512 * 1024 * 1024;

/// Browsers treat GIF delays of 0–10 ms as 100 ms; authors rely on it.
pub fn effective_delay(numer: u32, denom: u32) -> Duration {
    let ms = if denom == 0 { 0.0 } else { numer as f64 / denom as f64 };
    if ms <= 10.0 { Duration::from_millis(100) } else { Duration::from_secs_f64(ms / 1000.0) }
}

/// Every composited frame of an animated GIF, WebP or APNG. `None` for formats or files that
/// are not animation containers, and for animations over [`MAX_ANIMATION_BYTES`].
fn decode_frames(src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Option<Vec<AnimationFrame>>> {
    let frames = match format {
        FormatId::Gif => GifDecoder::new(src.reader()?).map_err(err)?.into_frames(),
        FormatId::Webp => {
            let dec = WebPDecoder::new(src.reader()?).map_err(err)?;
            if !dec.has_animation() {
                return Ok(None);
            }
            dec.into_frames()
        }
        FormatId::Png => {
            let dec = PngDecoder::new(src.reader()?).map_err(err)?;
            if !dec.is_apng().map_err(err)? {
                return Ok(None);
            }
            dec.apng().map_err(err)?.into_frames()
        }
        _ => return Ok(None),
    };
    let mut out = Vec::new();
    let mut total = 0u64;
    for frame in frames {
        cancel.check()?;
        let frame = frame.map_err(err)?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        let buf = frame.into_buffer();
        let (w, h) = buf.dimensions();
        total += buf.len() as u64;
        if total > MAX_ANIMATION_BYTES {
            return Ok(None);
        }
        out.push(AnimationFrame {
            image: ImageBuf::new(w, h, PixelFormat::Rgba8, buf.into_raw())?,
            delay: effective_delay(numer, denom),
        });
    }
    Ok((!out.is_empty()).then_some(out))
}

impl Decoder for ImageRsDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo> {
        let mut dec = open(src, format)?;
        info_from(&mut dec, src, format)
    }

    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let mut dec = open(src, format)?;
        let mut info = info_from(&mut dec, src, format)?;

        if let Some(mut frames) = decode_frames(src, format, cancel)? {
            info.frame_count = Some(frames.len() as u32);
            // composited frames are 8-bit RGBA canvases
            info.bits_per_channel = 8;
            if frames.len() == 1 {
                return Ok(Decoded::still(info, frames.remove(0).image));
            }
            let image = frames[0].image.clone();
            return Ok(Decoded { info, image, animation: Some(Animation { frames }), color_managed: false });
        }

        // refuse before allocating, with the size the RGBA output will need
        let out_format = match info.bits_per_channel {
            16 => PixelFormat::Rgba16,
            32 => PixelFormat::RgbaF32,
            _ => PixelFormat::Rgba8,
        };
        ImageBuf::byte_len(info.width, info.height, out_format)?;
        let img = DynamicImage::from_decoder(dec).map_err(err)?;
        cancel.check()?;
        let image = to_image_buf(img)?;
        cancel.check()?;
        Ok(Decoded::still(info, image))
    }
}
