//! Windows Imaging Component: whatever codecs the OS has installed (HEIF, AV1/AVIF, camera RAW,
//! WebP extensions …). Primary decoder for formats without a built-in Rust decoder, and a last
//! resort for the common ones when their Rust decoder fails.

use std::cell::RefCell;
use std::sync::Arc;

use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Orientation, PixelFormat, Result, Source};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Variant::VT_UI2;
use windows::core::{GUID, HSTRING, Interface, w};

use crate::decoder::{Decoded, Decoder, DecoderInfo};

const ID: &str = "wic";
const FALLBACK_ID: &str = "wic-fallback";

/// Formats WIC handles first: nothing built in decodes them.
const PRIMARY: &[FormatId] = &[FormatId::Heif, FormatId::Avif, FormatId::Raw];
/// Formats WIC rescues when the Rust decoder fails on a feature it lacks (CMYK or arithmetic-coded
/// JPEG, exotic TIFF compressions, unusual BMP headers, BCn DDS). PNG, GIF, ICO and WebP are left
/// out on purpose: their Rust decoders are complete, so a failure means a damaged file, and WIC
/// would report a truncated file as a success with the missing part blank.
const FALLBACK: &[FormatId] = &[FormatId::Jpeg, FormatId::Bmp, FormatId::Tiff, FormatId::Jxl, FormatId::Dds];

pub struct WicDecoder {
    info: DecoderInfo,
}

impl WicDecoder {
    pub fn primary() -> Self {
        Self { info: DecoderInfo { id: ID, name: "Windows Imaging Component", formats: PRIMARY, priority: 80 } }
    }

    pub fn fallback() -> Self {
        Self {
            info: DecoderInfo { id: FALLBACK_ID, name: "Windows Imaging Component", formats: FALLBACK, priority: 10 },
        }
    }

    fn codec_id(&self) -> &'static str {
        self.info.id
    }
}

thread_local! {
    static FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

fn wic_err(e: windows::core::Error) -> Error {
    Error::decode(ID, format!("{} (0x{:08X})", e.message(), e.code().0))
}

/// One factory per thread; COM is initialised for the thread on first use.
fn factory() -> Result<IWICImagingFactory> {
    FACTORY.with(|cell| {
        if let Some(f) = cell.borrow().as_ref() {
            return Ok(f.clone());
        }
        // S_FALSE (already initialised) and RPC_E_CHANGED_MODE (an STA thread) both leave COM usable
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let f: IWICImagingFactory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER) }.map_err(wic_err)?;
        *cell.borrow_mut() = Some(f.clone());
        Ok(f)
    })
}

/// A decoder plus whatever must outlive it.
struct Opened {
    decoder: IWICBitmapDecoder,
    // a memory stream reads straight from these bytes
    _bytes: Option<Arc<[u8]>>,
}

fn open(src: &Source) -> Result<Opened> {
    let f = factory()?;
    unsafe {
        if let Some(path) = src.path() {
            let decoder = f
                .CreateDecoderFromFilename(
                    &HSTRING::from(path.as_os_str()),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnDemand,
                )
                .map_err(wic_err)?;
            Ok(Opened { decoder, _bytes: None })
        } else {
            let bytes = src.read_all()?;
            let stream = f.CreateStream().map_err(wic_err)?;
            stream.InitializeFromMemory(&bytes).map_err(wic_err)?;
            let decoder = f
                .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
                .map_err(wic_err)?;
            Ok(Opened { decoder, _bytes: Some(bytes) })
        }
    }
}

/// Bits per channel and whether the pixel format carries alpha.
fn pixel_traits(f: &IWICImagingFactory, format: &GUID) -> (u8, bool) {
    let info = unsafe { f.CreateComponentInfo(format) }.ok().and_then(|c| c.cast::<IWICPixelFormatInfo2>().ok());
    let Some(info) = info else { return (8, true) };
    unsafe {
        let bpp = info.GetBitsPerPixel().unwrap_or(32);
        let channels = info.GetChannelCount().unwrap_or(4).max(1);
        let alpha = info.SupportsTransparency().map(|b| b.as_bool()).unwrap_or(false);
        ((bpp / channels).clamp(1, 32) as u8, alpha)
    }
}

fn orientation(frame: &IWICBitmapFrameDecode) -> Orientation {
    unsafe {
        let Ok(reader) = frame.GetMetadataQueryReader() else { return Orientation::Normal };
        let mut value = PROPVARIANT::default();
        if reader.GetMetadataByName(w!("System.Photo.Orientation"), &mut value).is_err() {
            return Orientation::Normal;
        }
        let inner = &value.Anonymous.Anonymous;
        let o =
            if inner.vt == VT_UI2 { Orientation::from_exif(inner.Anonymous.uiVal as u32) } else { Orientation::Normal };
        let _ = PropVariantClear(&mut value);
        o
    }
}

fn icc_profile(f: &IWICImagingFactory, frame: &IWICBitmapFrameDecode) -> Option<Arc<[u8]>> {
    unsafe {
        let mut count = 0u32;
        frame.GetColorContexts(&mut [], &mut count).ok()?;
        if count == 0 {
            return None;
        }
        let mut contexts: Vec<Option<IWICColorContext>> = (0..count).map(|_| f.CreateColorContext().ok()).collect();
        frame.GetColorContexts(&mut contexts, &mut count).ok()?;
        for ctx in contexts.into_iter().flatten() {
            if ctx.GetType().ok() != Some(WICColorContextProfile) {
                continue;
            }
            let mut len = 0u32;
            ctx.GetProfileBytes(&mut [], &mut len).ok()?;
            let mut buf = vec![0u8; len as usize];
            ctx.GetProfileBytes(&mut buf, &mut len).ok()?;
            buf.truncate(len as usize);
            return Some(buf.into());
        }
        None
    }
}

fn read_info(codec: &'static str, opened: &Opened, format: FormatId) -> Result<(ImageInfo, IWICBitmapFrameDecode)> {
    let f = factory()?;
    unsafe {
        let frame = opened.decoder.GetFrame(0).map_err(wic_err)?;
        let (mut width, mut height) = (0u32, 0u32);
        frame.GetSize(&mut width, &mut height).map_err(wic_err)?;
        let pixel_format = frame.GetPixelFormat().map_err(wic_err)?;
        let (bits_per_channel, has_alpha) = pixel_traits(&f, &pixel_format);
        let frame_count = opened.decoder.GetFrameCount().ok();
        let info = ImageInfo {
            format,
            codec,
            width,
            height,
            frame_count,
            has_alpha,
            bits_per_channel,
            // HEIF/AVIF decoders apply the container's rotation themselves; EXIF orientation in
            // those files is informative only and applying it again would double-rotate
            orientation: if matches!(format, FormatId::Heif | FormatId::Avif) {
                Orientation::Normal
            } else {
                orientation(&frame)
            },
            icc_profile: icc_profile(&f, &frame),
        };
        Ok((info, frame))
    }
}

/// Converts any WIC source to straight-alpha RGBA, 8 or 16 bits per channel.
fn to_image(source: &IWICBitmapSource, deep: bool) -> Result<ImageBuf> {
    let f = factory()?;
    unsafe {
        let (mut w, mut h) = (0u32, 0u32);
        source.GetSize(&mut w, &mut h).map_err(wic_err)?;
        let (target, format) = if deep {
            (GUID_WICPixelFormat64bppRGBA, PixelFormat::Rgba16)
        } else {
            (GUID_WICPixelFormat32bppRGBA, PixelFormat::Rgba8)
        };
        let len = ImageBuf::byte_len(w, h, format)? as usize;
        let converter = f.CreateFormatConverter().map_err(wic_err)?;
        converter
            .Initialize(source, &target, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .map_err(wic_err)?;
        let stride = w * format.bytes_per_pixel() as u32;
        let mut buf = vec![0u8; len];
        converter.CopyPixels(std::ptr::null(), stride, &mut buf).map_err(wic_err)?;
        ImageBuf::new(w, h, format, buf)
    }
}

impl Decoder for WicDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo> {
        let opened = open(src)?;
        Ok(read_info(self.codec_id(), &opened, format)?.0)
    }

    fn preview(
        &self,
        src: &Source,
        format: FormatId,
        _target_side: u32,
        cancel: &CancelToken,
    ) -> Result<Option<ImageBuf>> {
        // AVIF carries no thumbnail: WIC would decode the whole image to make one (measured 46 ms
        // of a 76 ms decode at 9 MP), and without alpha
        if format == FormatId::Avif {
            return Ok(None);
        }
        let opened = open(src)?;
        cancel.check()?;
        let thumb = unsafe {
            // container thumbnail (HEIF, RAW previews), then the first frame's own (EXIF)
            opened.decoder.GetThumbnail().or_else(|_| opened.decoder.GetFrame(0).and_then(|fr| fr.GetThumbnail()))
        };
        match thumb {
            Ok(t) => Ok(Some(to_image(&t, false)?)),
            Err(_) => Ok(None),
        }
    }

    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let opened = open(src)?;
        let (info, frame) = read_info(self.codec_id(), &opened, format)?;
        cancel.check()?;
        let image = to_image(&frame.cast::<IWICBitmapSource>().map_err(wic_err)?, info.bits_per_channel > 8)?;
        cancel.check()?;
        Ok(Decoded { info, image })
    }
}
