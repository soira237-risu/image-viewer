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
        exif: dec.exif_metadata().ok().flatten().map(Arc::from),
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

/// Widens `buf`, whose first `pixels * C * S` bytes hold pixels of `C` channels of `S`-byte
/// native-endian samples (grey, grey+alpha, RGB or RGBA), to RGBA in place. `opaque` is the
/// alpha value given to pixels without alpha. Works backwards: pixel `i` lands at or after its
/// source, and only after every later pixel has moved out of the way.
fn widen_to_rgba<const C: usize, const S: usize>(buf: &mut [u8], opaque: [u8; S]) {
    if C == 4 {
        return;
    }
    let pixels = buf.len() / (4 * S);
    for i in (0..pixels).rev() {
        let mut px = [[0u8; S]; 4];
        for (c, sample) in px.iter_mut().enumerate().take(C) {
            sample.copy_from_slice(&buf[(i * C + c) * S..(i * C + c + 1) * S]);
        }
        let out: [[u8; S]; 4] = match C {
            1 => [px[0], px[0], px[0], opaque],
            2 => [px[0], px[0], px[0], px[1]],
            _ => [px[0], px[1], px[2], opaque],
        };
        for (c, sample) in out.iter().enumerate() {
            buf[(i * 4 + c) * S..(i * 4 + c + 1) * S].copy_from_slice(sample);
        }
    }
}

/// Decodes a still image straight into the pipeline's RGBA layout: one buffer of the output's
/// size, reserved fallibly (a failed reservation is [`Error::TooLarge`] instead of an abort),
/// filled by the decoder and widened to RGBA in place. Converting through a `DynamicImage`
/// would hold the decoded and the converted image at once, plus a third copy for 16-bit and
/// float pixels.
fn decode_rgba(dec: impl ImageDecoder, width: u32, height: u32) -> Result<ImageBuf> {
    use PixelFormat::{Rgba8, Rgba16, RgbaF32};
    let color = dec.color_type();
    let (out_format, widen): (PixelFormat, fn(&mut [u8])) = match color {
        ColorType::L8 => (Rgba8, |b| widen_to_rgba::<1, 1>(b, [u8::MAX])),
        ColorType::La8 => (Rgba8, |b| widen_to_rgba::<2, 1>(b, [u8::MAX])),
        ColorType::Rgb8 => (Rgba8, |b| widen_to_rgba::<3, 1>(b, [u8::MAX])),
        ColorType::Rgba8 => (Rgba8, |_| {}),
        ColorType::L16 => (Rgba16, |b| widen_to_rgba::<1, 2>(b, u16::MAX.to_ne_bytes())),
        ColorType::La16 => (Rgba16, |b| widen_to_rgba::<2, 2>(b, u16::MAX.to_ne_bytes())),
        ColorType::Rgb16 => (Rgba16, |b| widen_to_rgba::<3, 2>(b, u16::MAX.to_ne_bytes())),
        ColorType::Rgba16 => (Rgba16, |_| {}),
        ColorType::Rgb32F => (RgbaF32, |b| widen_to_rgba::<3, 4>(b, 1f32.to_ne_bytes())),
        ColorType::Rgba32F => (RgbaF32, |_| {}),
        _ => return to_image_buf(DynamicImage::from_decoder(dec).map_err(err)?),
    };
    let out_len = ImageBuf::byte_len(width, height, out_format)?;
    let in_len = dec.total_bytes();
    let pixels = width as u64 * height as u64;
    if in_len != pixels * color.bytes_per_pixel() as u64 {
        return Err(Error::decode(ID, format!("decoder reports {in_len} bytes for {width}x{height} {color:?}")));
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(out_len as usize).map_err(|_| Error::TooLarge { width, height, bytes: out_len })?;
    buf.resize(out_len as usize, 0);
    dec.read_image(&mut buf[..in_len as usize]).map_err(err)?;
    widen(&mut buf);
    ImageBuf::new(width, height, out_format, buf)
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
        let image = decode_rgba(dec, info.width, info.height)?;
        cancel.check()?;
        Ok(Decoded::still(info, image))
    }
}
