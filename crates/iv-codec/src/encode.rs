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
// links the libjxl that gamut-jxl calls
use iv_libjxl as _;

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
    /// Writes [`Metadata::icc`] into the file. Where it cannot, a caller that keeps metadata
    /// converts the pixels to sRGB first, so the file looks as the original did.
    pub embeds_icc: bool,
}

pub trait Encoder: Send + Sync {
    fn info(&self) -> &EncoderInfo;
    fn options(&self) -> &'static [OptionSpec];
    /// Encodes `img` (already prepared by [`prepare`]) with metadata where supported.
    fn encode(&self, img: &DynamicImage, opts: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>>;
    /// Stores the JPEG file `jpeg` without decoding it, when the format can hold a JPEG's DCT
    /// coefficients as they are (JPEG XL, which can also give the original file back). Metadata
    /// is whatever segments `jpeg` has. `None` when this format cannot, or `opts` ask for a
    /// pixel encode; an error when the encoder refuses this particular JPEG (the caller then
    /// encodes the pixels instead).
    fn recompress_jpeg(&self, jpeg: &[u8], opts: &EncodeOptions) -> Option<Result<Vec<u8>>> {
        let _ = (jpeg, opts);
        None
    }
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
            let rgb: Vec<u16> = px.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
            return DynamicImage::ImageRgb16(image::ImageBuffer::from_raw(w, h, rgb).expect("buffer matches size"));
        }
        return DynamicImage::ImageRgba16(
            image::ImageBuffer::from_raw(w, h, px.to_vec()).expect("buffer matches size"),
        );
    }
    // 8-bit (16-bit and float rounded down to it), read without a converted copy when it already is
    let rgba = img.to_rgba8();
    if opaque {
        let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        return DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, rgb).expect("buffer matches size"));
    }
    if info.supports_alpha {
        return DynamicImage::ImageRgba8(RgbaImage::from_raw(w, h, rgba.into_owned()).expect("buffer matches size"));
    }
    let flat: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
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
    ($id:literal, $fmt:expr, alpha: $a:expr, deep: $d:expr, lossy: $l:expr, icc: $i:expr) => {
        EncoderInfo { id: $id, format: $fmt, supports_alpha: $a, supports_16bit: $d, lossy: $l, embeds_icc: $i }
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

/// JPEG XL through the reference libjxl (`gamut-jxl`, linked statically by `iv-libjxl`).
///
/// Defaults follow libjxl's `cjxl`: quality 90 (distance 1.0, "visually lossless"), effort 7,
/// and a JPEG source without pixel edits recompressed losslessly instead of decoded
/// ([`Encoder::recompress_jpeg`]). Quality 100 is mathematically lossless, as in `cjxl`.
struct Jxl(EncoderInfo);

const JXL_OPTIONS: &[OptionSpec] = &[
    OptionSpec { key: "quality", label: "Quality", kind: OptionKind::Int { min: 1, max: 100, default: 90 } },
    OptionSpec { key: "lossless", label: "Lossless", kind: OptionKind::Bool { default: false } },
    OptionSpec {
        key: "effort",
        label: "Effort (1 fast – 9 small)",
        kind: OptionKind::Int { min: 1, max: 9, default: 7 },
    },
    OptionSpec { key: "lossless_jpeg", label: "Recompress JPEG losslessly", kind: OptionKind::Bool { default: true } },
];

/// libjxl's `JxlEncoderDistanceFromQuality`: the Butteraugli distance for a JPEG-like quality.
/// 100 gives 0, which libjxl (and [`Jxl`]) treats as lossless; 90 gives 1.0, 30 gives 6.4, and
/// below 30 the curve rises to 25 (the largest distance) at 0.
pub fn jxl_distance_from_quality(quality: f32) -> f32 {
    if quality >= 100.0 {
        0.0
    } else if quality >= 30.0 {
        0.1 + (100.0 - quality) * 0.09
    } else {
        53.0 / 3000.0 * quality * quality - 23.0 / 20.0 * quality + 25.0
    }
}

impl Jxl {
    fn effort(opts: &EncodeOptions) -> gamut_jxl::Effort {
        let level = opts.get(JXL_OPTIONS, "effort").clamp(1, 9) as u8;
        gamut_jxl::Effort::from_level(level).expect("1..=9 is a libjxl effort")
    }
}

/// The ICC profile in `meta` when it is of colour space `space` (`b"RGB "`, `b"GRAY"`).
fn icc_of<'a>(meta: &'a Metadata, space: &[u8; 4]) -> Option<&'a [u8]> {
    meta.icc.as_deref().filter(|p| p.len() >= 128 && &p[16..20] == space)
}

/// Whether every pixel of `img` (as [`prepare`] makes it: RGB or RGBA, 8 or 16 bits) is gray.
fn all_gray(img: &DynamicImage) -> bool {
    fn equal<T: PartialEq>(px: &[T], channels: usize) -> bool {
        px.chunks_exact(channels).all(|p| p[0] == p[1] && p[1] == p[2])
    }
    match img {
        DynamicImage::ImageRgb8(i) => equal(i.as_raw(), 3),
        DynamicImage::ImageRgba8(i) => equal(i.as_raw(), 4),
        DynamicImage::ImageRgb16(i) => equal(i.as_raw(), 3),
        DynamicImage::ImageRgba16(i) => equal(i.as_raw(), 4),
        _ => false,
    }
}

/// The first channel and, with `alpha`, the last of each pixel of `px` (`channels` samples each).
fn gray_samples<T: Copy>(px: &[T], channels: usize, alpha: bool) -> Vec<T> {
    let mut out = Vec::with_capacity(px.len() / channels * (1 + alpha as usize));
    for p in px.chunks_exact(channels) {
        out.push(p[0]);
        if alpha {
            out.push(p[channels - 1]);
        }
    }
    out
}

impl Encoder for Jxl {
    fn info(&self) -> &EncoderInfo {
        &self.0
    }
    fn options(&self) -> &'static [OptionSpec] {
        JXL_OPTIONS
    }
    fn encode(&self, img: &DynamicImage, opts: &EncodeOptions, meta: &Metadata) -> Result<Vec<u8>> {
        use gamut_core::{
            Dimensions, EncodeImage, Gray8, Gray16, GrayAlpha8, GrayAlpha16, ImageRef, Rgb8, Rgb16, Rgba8, Rgba16,
        };
        use gamut_jxl::{ColorSpec, Container, Distance, JxlEncoder};
        let id = self.0.id;
        let quality = opts.get(JXL_OPTIONS, "quality");
        let distance = jxl_distance_from_quality(quality as f32);
        let mut enc = if opts.get(JXL_OPTIONS, "lossless") == 1 || distance <= 0.0 {
            JxlEncoder::lossless()
        } else {
            JxlEncoder::lossy(Distance::new(distance).map_err(|e| err(id, e))?)
        };
        enc = enc.with_effort(Self::effort(opts));
        // A gray profile (a gray source) describes gray samples: the image is written as gray
        // with it, so tones under a non-sRGB curve stay as they were. Pixels an edit coloured,
        // and a CMYK profile left over from the source, describe nothing that is written: the
        // RGB pixels go out as sRGB.
        let gray_icc = icc_of(meta, b"GRAY").filter(|_| all_gray(img));
        if let Some(icc) = icc_of(meta, b"RGB ").or(gray_icc) {
            enc = enc.with_color(ColorSpec::Icc(icc.to_vec()));
        }
        // EXIF lives in a box of the container; without it the bare codestream is smaller
        if let Some(exif) = meta.exif.as_deref().filter(|e| !e.is_empty()) {
            enc = enc.with_container(Container::IsoBmff).with_exif(exif);
        }
        let dims = Dimensions::new(img.width(), img.height()).map_err(|e| err(id, e))?;
        let encoded = match img {
            DynamicImage::ImageRgb8(i) if gray_icc.is_some() => {
                ImageRef::<Gray8>::new(&gray_samples(i.as_raw(), 3, false), dims).and_then(|r| enc.encode_to_vec(r))
            }
            DynamicImage::ImageRgba8(i) if gray_icc.is_some() => {
                ImageRef::<GrayAlpha8>::new(&gray_samples(i.as_raw(), 4, true), dims).and_then(|r| enc.encode_to_vec(r))
            }
            DynamicImage::ImageRgb16(i) if gray_icc.is_some() => {
                ImageRef::<Gray16>::new(&gray_samples(i.as_raw(), 3, false), dims).and_then(|r| enc.encode_to_vec(r))
            }
            DynamicImage::ImageRgba16(i) if gray_icc.is_some() => {
                ImageRef::<GrayAlpha16>::new(&gray_samples(i.as_raw(), 4, true), dims)
                    .and_then(|r| enc.encode_to_vec(r))
            }
            DynamicImage::ImageRgb8(i) => ImageRef::<Rgb8>::new(i.as_raw(), dims).and_then(|r| enc.encode_to_vec(r)),
            DynamicImage::ImageRgba8(i) => ImageRef::<Rgba8>::new(i.as_raw(), dims).and_then(|r| enc.encode_to_vec(r)),
            DynamicImage::ImageRgb16(i) => ImageRef::<Rgb16>::new(i.as_raw(), dims).and_then(|r| enc.encode_to_vec(r)),
            DynamicImage::ImageRgba16(i) => {
                ImageRef::<Rgba16>::new(i.as_raw(), dims).and_then(|r| enc.encode_to_vec(r))
            }
            other => {
                let rgba = other.to_rgba8();
                ImageRef::<Rgba8>::new(rgba.as_raw(), dims).and_then(|r| enc.encode_to_vec(r))
            }
        };
        encoded.map_err(|e| err(id, e))
    }
    fn recompress_jpeg(&self, jpeg: &[u8], opts: &EncodeOptions) -> Option<Result<Vec<u8>>> {
        if opts.get(JXL_OPTIONS, "lossless_jpeg") == 0 {
            return None;
        }
        let mut out = Vec::new();
        let result = gamut_jxl::JxlEncoder::lossless().with_effort(Self::effort(opts)).recompress_jpeg(jpeg, &mut out);
        Some(result.map(|_| out).map_err(|e| err(self.0.id, e)))
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
            Arc::new(Jpeg(info!("jpeg", FormatId::Jpeg, alpha: false, deep: false, lossy: true, icc: true))),
            Arc::new(Png(info!("png", FormatId::Png, alpha: true, deep: true, lossy: false, icc: true))),
            Arc::new(Webp(info!("webp", FormatId::Webp, alpha: true, deep: false, lossy: true, icc: true))),
            Arc::new(Avif(info!("avif", FormatId::Avif, alpha: true, deep: false, lossy: true, icc: false))),
            Arc::new(Jxl(info!("jxl", FormatId::Jxl, alpha: true, deep: true, lossy: true, icc: true))),
            Arc::new(Plain(info!("tiff", FormatId::Tiff, alpha: true, deep: true, lossy: false, icc: true))),
            Arc::new(Plain(info!("bmp", FormatId::Bmp, alpha: true, deep: false, lossy: false, icc: false))),
            Arc::new(Plain(info!("qoi", FormatId::Qoi, alpha: true, deep: false, lossy: false, icc: false))),
            Arc::new(Plain(info!("gif", FormatId::Gif, alpha: true, deep: false, lossy: true, icc: false))),
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
