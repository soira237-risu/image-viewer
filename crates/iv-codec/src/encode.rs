//! Encoders: each declares its options as data, so the CLI and the save dialog can validate and
//! present them without per-format code.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use image::codecs::avif::AvifEncoder;
use image::codecs::bmp::BmpEncoder;
use image::codecs::gif::GifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::qoi::QoiEncoder;
use image::codecs::tiff::TiffEncoder;
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ImageEncoder, RgbImage, RgbaImage};
use iv_core::{Error, FormatId, ImageBuf, PixelFormat, Result};

/// What a value of an option may be.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OptionKind {
    Int { min: i64, max: i64, default: i64 },
    Bool { default: bool },
    Choice { values: &'static [&'static str], default: usize },
}

#[derive(Clone, Copy, Debug)]
pub struct OptionSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: OptionKind,
}

/// Option values, validated against an encoder's [`OptionSpec`]s. Missing keys use defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncodeOptions {
    values: BTreeMap<&'static str, i64>,
}

impl EncodeOptions {
    /// Parses `key=value` pairs (`quality=80`, `lossless=true`, `compression=best`).
    pub fn parse(specs: &[OptionSpec], pairs: &[String]) -> std::result::Result<Self, String> {
        let mut values = BTreeMap::new();
        for pair in pairs {
            let (k, v) = pair.split_once('=').ok_or_else(|| format!("expected key=value, got '{pair}'"))?;
            let spec = specs.iter().find(|s| s.key == k.trim()).ok_or_else(|| {
                let known: Vec<_> = specs.iter().map(|s| s.key).collect();
                format!(
                    "unknown option '{k}' (this format takes: {})",
                    if known.is_empty() { "none".into() } else { known.join(", ") }
                )
            })?;
            let v = v.trim();
            let parsed = match spec.kind {
                OptionKind::Int { min, max, .. } => {
                    let n: i64 = v.parse().map_err(|_| format!("{k}: '{v}' is not a number"))?;
                    if !(min..=max).contains(&n) {
                        return Err(format!("{k}: {n} is outside {min}..={max}"));
                    }
                    n
                }
                OptionKind::Bool { .. } => match v {
                    "true" | "1" | "yes" | "on" => 1,
                    "false" | "0" | "no" | "off" => 0,
                    _ => return Err(format!("{k}: '{v}' is not true/false")),
                },
                OptionKind::Choice { values: choices, .. } => choices
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(v))
                    .ok_or_else(|| format!("{k}: '{v}' is not one of {}", choices.join(", ")))?
                    as i64,
            };
            values.insert(spec.key, parsed);
        }
        Ok(Self { values })
    }

    fn get(&self, specs: &[OptionSpec], key: &str) -> i64 {
        let spec = specs.iter().find(|s| s.key == key).expect("encoder reads only its own options");
        self.values.get(key).copied().unwrap_or(match spec.kind {
            OptionKind::Int { default, .. } => default,
            OptionKind::Bool { default } => default as i64,
            OptionKind::Choice { default, .. } => default as i64,
        })
    }
}

/// Metadata to embed, when the format can carry it.
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    /// ICC profile describing the pixels.
    pub icc: Option<Vec<u8>>,
    /// Raw EXIF (TIFF structure, without the `Exif\0\0` prefix).
    pub exif: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct EncoderInfo {
    pub id: &'static str,
    pub format: FormatId,
    pub supports_alpha: bool,
    pub supports_16bit: bool,
    pub lossy: bool,
}

pub trait Encoder: Send + Sync {
    fn info(&self) -> &EncoderInfo;
    fn options(&self) -> &'static [OptionSpec];
    /// Encodes `img` (already prepared by [`prepare`]) with metadata where supported.
    fn encode(&self, img: &DynamicImage, opts: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>>;
}

fn err(id: &'static str, e: impl std::fmt::Display) -> Error {
    Error::Decode { codec: id, message: format!("encode: {e}") }
}

/// Converts pixels to what the encoder accepts: 16-bit only where supported, alpha dropped when
/// the image is opaque (smaller files) and composited over `background` when the format has no
/// alpha.
///
/// Each result is made in one pass straight from `img`'s pixels: no intermediate full copy.
pub fn prepare(img: &ImageBuf, info: &EncoderInfo, background: [u8; 3]) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let opaque = img.is_opaque();
    if img.format() == PixelFormat::Rgba16 && info.supports_16bit {
        let px: &[u16] = bytemuck::cast_slice(img.data());
        if opaque || !info.supports_alpha {
            let rgb: Vec<u16> = px.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            return DynamicImage::ImageRgb16(image::ImageBuffer::from_raw(w, h, rgb).expect("buffer matches size"));
        }
        return DynamicImage::ImageRgba16(
            image::ImageBuffer::from_raw(w, h, px.to_vec()).expect("buffer matches size"),
        );
    }
    // 8-bit (16-bit and float rounded down to it), read without a converted copy when it already is
    let rgba = img.to_rgba8();
    if opaque {
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        return DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, rgb).expect("buffer matches size"));
    }
    if info.supports_alpha {
        return DynamicImage::ImageRgba8(RgbaImage::from_raw(w, h, rgba.into_owned()).expect("buffer matches size"));
    }
    let flat: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as u32;
            [0, 1, 2].map(|c| ((p[c] as u32 * a + background[c] as u32 * (255 - a) + 127) / 255) as u8)
        })
        .collect();
    DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, flat).expect("buffer matches size"))
}

fn write_with(enc: impl ImageEncoder, img: &DynamicImage, meta: &Metadata, id: &'static str) -> Result<()> {
    let mut enc = enc;
    if let Some(icc) = &meta.icc {
        // formats without an ICC slot simply skip it
        let _ = enc.set_icc_profile(icc.clone());
    }
    if let Some(exif) = &meta.exif {
        let _ = enc.set_exif_metadata(exif.clone());
    }
    enc.write_image(img.as_bytes(), img.width(), img.height(), img.color().into()).map_err(|e| err(id, e))
}

macro_rules! info {
    ($id:literal, $fmt:expr, alpha: $a:expr, deep: $d:expr, lossy: $l:expr) => {
        EncoderInfo { id: $id, format: $fmt, supports_alpha: $a, supports_16bit: $d, lossy: $l }
    };
}

struct Jpeg(EncoderInfo);
const QUALITY_90: OptionSpec =
    OptionSpec { key: "quality", label: "Quality", kind: OptionKind::Int { min: 1, max: 100, default: 90 } };
impl Encoder for Jpeg {
    fn info(&self) -> &EncoderInfo {
        &self.0
    }
    fn options(&self) -> &'static [OptionSpec] {
        &[QUALITY_90]
    }
    fn encode(&self, img: &DynamicImage, opts: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let q = opts.get(self.options(), "quality") as u8;
        write_with(JpegEncoder::new_with_quality(&mut out, q), img, meta, self.0.id)?;
        Ok(out)
    }
}

struct Png(EncoderInfo);
impl Encoder for Png {
    fn info(&self) -> &EncoderInfo {
        &self.0
    }
    fn options(&self) -> &'static [OptionSpec] {
        &[OptionSpec {
            key: "compression",
            label: "Compression",
            kind: OptionKind::Choice { values: &["fast", "default", "best"], default: 1 },
        }]
    }
    fn encode(&self, img: &DynamicImage, opts: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let c = match opts.get(self.options(), "compression") {
            0 => CompressionType::Fast,
            2 => CompressionType::Best,
            _ => CompressionType::Default,
        };
        write_with(PngEncoder::new_with_quality(&mut out, c, FilterType::Adaptive), img, meta, self.0.id)?;
        Ok(out)
    }
}

struct Webp(EncoderInfo);
impl Encoder for Webp {
    fn info(&self) -> &EncoderInfo {
        &self.0
    }
    fn options(&self) -> &'static [OptionSpec] {
        &[
            OptionSpec { key: "lossless", label: "Lossless", kind: OptionKind::Bool { default: false } },
            OptionSpec { key: "quality", label: "Quality", kind: OptionKind::Int { min: 1, max: 100, default: 85 } },
        ]
    }
    fn encode(&self, img: &DynamicImage, opts: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>> {
        if opts.get(self.options(), "lossless") == 1 {
            let mut out = Vec::new();
            write_with(WebPEncoder::new_lossless(&mut out), img, meta, self.0.id)?;
            return Ok(out);
        }
        let q = opts.get(self.options(), "quality") as f32;
        let rgba = img.to_rgba8();
        let has_alpha = img.color().has_alpha();
        let simple = if has_alpha {
            webp::Encoder::from_rgba(rgba.as_raw(), img.width(), img.height()).encode(q)
        } else {
            let rgb = img.to_rgb8();
            webp::Encoder::from_rgb(rgb.as_raw(), img.width(), img.height()).encode(q)
        };
        crate::webp_container::with_metadata(&simple, img.width(), img.height(), has_alpha, meta)
            .map_err(|e| err(self.0.id, e))
    }
}

struct Avif(EncoderInfo);
impl Encoder for Avif {
    fn info(&self) -> &EncoderInfo {
        &self.0
    }
    fn options(&self) -> &'static [OptionSpec] {
        &[
            OptionSpec { key: "quality", label: "Quality", kind: OptionKind::Int { min: 1, max: 100, default: 75 } },
            OptionSpec {
                key: "speed",
                label: "Speed (1 slow – 10 fast)",
                kind: OptionKind::Int { min: 1, max: 10, default: 6 },
            },
        ]
    }
    fn encode(&self, img: &DynamicImage, opts: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let q = opts.get(self.options(), "quality") as u8;
        let s = opts.get(self.options(), "speed") as u8;
        // the AVIF encoder takes 8-bit RGBA
        let rgba = DynamicImage::ImageRgba8(img.to_rgba8());
        write_with(AvifEncoder::new_with_speed_quality(&mut out, s, q), &rgba, meta, self.0.id)?;
        Ok(out)
    }
}

struct Jxl(EncoderInfo);
impl Encoder for Jxl {
    fn info(&self) -> &EncoderInfo {
        &self.0
    }
    fn options(&self) -> &'static [OptionSpec] {
        &[]
    }
    fn encode(&self, img: &DynamicImage, _: &EncodeOptions, _: &Metadata) -> Result<Vec<u8>> {
        use zune_core::bit_depth::BitDepth;
        use zune_core::colorspace::ColorSpace;
        use zune_core::options::EncoderOptions;
        let (w, h) = (img.width() as usize, img.height() as usize);
        let (bytes, colorspace, depth): (Vec<u8>, ColorSpace, BitDepth) = match img {
            DynamicImage::ImageRgb8(i) => (i.as_raw().clone(), ColorSpace::RGB, BitDepth::Eight),
            DynamicImage::ImageRgba8(i) => (i.as_raw().clone(), ColorSpace::RGBA, BitDepth::Eight),
            DynamicImage::ImageRgb16(i) => {
                (bytemuck::cast_slice(i.as_raw()).to_vec(), ColorSpace::RGB, BitDepth::Sixteen)
            }
            DynamicImage::ImageRgba16(i) => {
                (bytemuck::cast_slice(i.as_raw()).to_vec(), ColorSpace::RGBA, BitDepth::Sixteen)
            }
            other => (other.to_rgba8().into_raw(), ColorSpace::RGBA, BitDepth::Eight),
        };
        let mut out = Vec::new();
        zune_jpegxl::JxlSimpleEncoder::new(&bytes, EncoderOptions::new(w, h, colorspace, depth))
            .encode(&mut out)
            .map_err(|e| err(self.0.id, format!("{e:?}")))?;
        Ok(out)
    }
}

/// Formats encoded by `image` with no options.
struct Plain(EncoderInfo);
impl Encoder for Plain {
    fn info(&self) -> &EncoderInfo {
        &self.0
    }
    fn options(&self) -> &'static [OptionSpec] {
        &[]
    }
    fn encode(&self, img: &DynamicImage, _: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let id = self.0.id;
        match self.0.format {
            FormatId::Bmp => write_with(BmpEncoder::new(&mut out), img, meta, id)?,
            FormatId::Tiff => write_with(TiffEncoder::new(Cursor::new(&mut out)), img, meta, id)?,
            FormatId::Qoi => write_with(QoiEncoder::new(&mut out), img, meta, id)?,
            FormatId::Gif => {
                let mut enc = GifEncoder::new(&mut out);
                enc.encode_frame(image::Frame::new(img.to_rgba8())).map_err(|e| err(id, e))?;
            }
            other => return Err(err(id, format!("{other} is not a plain format"))),
        }
        Ok(out)
    }
}

/// The encoders available, one per output format.
#[derive(Clone)]
pub struct Encoders {
    list: Vec<Arc<dyn Encoder>>,
}

impl Default for Encoders {
    fn default() -> Self {
        let list: Vec<Arc<dyn Encoder>> = vec![
            Arc::new(Jpeg(info!("jpeg", FormatId::Jpeg, alpha: false, deep: false, lossy: true))),
            Arc::new(Png(info!("png", FormatId::Png, alpha: true, deep: true, lossy: false))),
            Arc::new(Webp(info!("webp", FormatId::Webp, alpha: true, deep: false, lossy: true))),
            Arc::new(Avif(info!("avif", FormatId::Avif, alpha: true, deep: false, lossy: true))),
            Arc::new(Jxl(info!("jxl", FormatId::Jxl, alpha: true, deep: true, lossy: false))),
            Arc::new(Plain(info!("tiff", FormatId::Tiff, alpha: true, deep: true, lossy: false))),
            Arc::new(Plain(info!("bmp", FormatId::Bmp, alpha: true, deep: false, lossy: false))),
            Arc::new(Plain(info!("qoi", FormatId::Qoi, alpha: true, deep: false, lossy: false))),
            Arc::new(Plain(info!("gif", FormatId::Gif, alpha: true, deep: false, lossy: true))),
        ];
        Self { list }
    }
}

impl Encoders {
    pub fn for_format(&self, format: FormatId) -> Option<&Arc<dyn Encoder>> {
        self.list.iter().find(|e| e.info().format == format)
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn Encoder>> {
        self.list.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_validate_and_default() {
        let enc = Encoders::default();
        let webp = enc.for_format(FormatId::Webp).unwrap();
        let o = EncodeOptions::parse(webp.options(), &["quality=40".into(), "lossless=yes".into()]).unwrap();
        assert_eq!(o.get(webp.options(), "quality"), 40);
        assert_eq!(o.get(webp.options(), "lossless"), 1);
        assert_eq!(EncodeOptions::default().get(webp.options(), "quality"), 85);
        assert!(EncodeOptions::parse(webp.options(), &["quality=0".into()]).unwrap_err().contains("outside"));
        assert!(EncodeOptions::parse(webp.options(), &["speed=3".into()]).unwrap_err().contains("unknown option"));
        let png = enc.for_format(FormatId::Png).unwrap();
        assert_eq!(
            EncodeOptions::parse(png.options(), &["compression=BEST".into()])
                .unwrap()
                .get(png.options(), "compression"),
            2
        );
    }

    #[test]
    fn prepare_flattens_alpha_only_where_needed() {
        let img = ImageBuf::new(1, 1, PixelFormat::Rgba8, vec![0, 0, 0, 0]).unwrap();
        let enc = Encoders::default();
        let jpeg = prepare(&img, enc.for_format(FormatId::Jpeg).unwrap().info(), [255, 255, 255]);
        assert_eq!(jpeg.as_bytes(), &[255, 255, 255], "transparent black over white");
        let png = prepare(&img, enc.for_format(FormatId::Png).unwrap().info(), [255, 255, 255]);
        assert_eq!(png.as_bytes(), &[0, 0, 0, 0], "alpha kept");
        let opaque = ImageBuf::new(1, 1, PixelFormat::Rgba8, vec![1, 2, 3, 255]).unwrap();
        assert_eq!(prepare(&opaque, enc.for_format(FormatId::Png).unwrap().info(), [0; 3]).as_bytes(), &[1, 2, 3]);
    }
}
