//! The viewer's Explorer thumbnail provider: an in-process COM server (`IThumbnailProvider`
//! initialised with `IInitializeWithStream`) that Explorer loads to draw thumbnails of the
//! formats the viewer reads and Windows does not. The settings window registers it for the
//! current user, only for extensions no other thumbnail handler claims (`iv_win::thumbs`).
//!
//! Stream-based providers run in a separate surrogate process (`dllhost.exe`) unless their
//! registration turns that off, which this one's never does: a decoder failing on a hostile
//! file takes down that process, not Explorer. Inside, panics become errors anyway, and the
//! decoding itself is `iv_pipeline::thumbnail`, tested without COM.

#![cfg(windows)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::sync::OnceLock;

use iv_codec::Registry;
use iv_core::{CancelToken, ImageBuf, PixelFormat};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED,
    ERROR_ALREADY_INITIALIZED, S_FALSE,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP,
};
use windows::Win32::System::Com::{
    CoTaskMemFree, IClassFactory, IClassFactory_Impl, IStream, STATFLAG_DEFAULT, STATSTG, STREAM_SEEK_CUR,
    STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithStream, IInitializeWithStream_Impl};
use windows::Win32::UI::Shell::{IThumbnailProvider, IThumbnailProvider_Impl, WTS_ALPHATYPE, WTSAT_ARGB, WTSAT_RGB};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement};

/// The provider's class id; [`iv_pipeline::thumbnail::PROVIDER_CLSID`] is what gets registered.
pub const CLSID: GUID = GUID::from_u128(iv_pipeline::thumbnail::PROVIDER_CLSID);

/// Largest side drawn, whatever Explorer asks (it asks up to 2560 for its largest icon view).
const MAX_SIDE: u32 = 2560;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(iv_codec::default_registry)
}

/// Hands out the class object for [`CLSID`].
///
/// # Safety
/// The pointers come from COM: `rclsid` and `riid` point at GUIDs, `ppv` at writable storage.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = std::ptr::null_mut() };
    if rclsid.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    if unsafe { *rclsid } != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    unsafe { factory.query(riid, ppv) }
}

/// Never unloaded once loaded: the decoders keep worker threads (rayon) and caches that must
/// not outlive the code they run.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppv = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let provider: IUnknown = Provider::default().into();
        unsafe { provider.query(riid, ppv) }.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

/// One thumbnail: Explorer creates it, hands it the file's stream once, and asks for the
/// picture. Apartment-threaded: only the thread that made it calls it.
#[implement(IThumbnailProvider, IInitializeWithStream)]
#[derive(Default)]
struct Provider {
    stream: RefCell<Option<IStream>>,
}

impl IInitializeWithStream_Impl for Provider_Impl {
    fn Initialize(&self, stream: Ref<IStream>, _mode: u32) -> Result<()> {
        let mut slot = self.stream.borrow_mut();
        if slot.is_some() {
            return Err(ERROR_ALREADY_INITIALIZED.to_hresult().into());
        }
        *slot = Some(stream.ok()?.clone());
        Ok(())
    }
}

impl IThumbnailProvider_Impl for Provider_Impl {
    fn GetThumbnail(&self, cx: u32, phbmp: *mut HBITMAP, pdwalpha: *mut WTS_ALPHATYPE) -> Result<()> {
        if phbmp.is_null() || pdwalpha.is_null() {
            return Err(E_POINTER.into());
        }
        let stream = self.stream.borrow().clone().ok_or(windows::core::Error::from(E_UNEXPECTED))?;
        let name = stream_name(&stream);
        let side = cx.clamp(1, MAX_SIDE);
        // decoders run on files from anywhere: a panic is a failed thumbnail, not a crash
        let image = iv_core::panic::catch("thumbnail", || {
            let reader = BufReader::with_capacity(64 * 1024, StreamReader(stream));
            iv_pipeline::thumbnail::thumbnail_of(registry(), reader, &name, side, &CancelToken::never())
        })
        .map_err(|_| windows::core::Error::from(E_FAIL))?;
        let bitmap = bitmap(&image)?;
        unsafe {
            *phbmp = bitmap;
            *pdwalpha = if image.is_opaque() { WTSAT_RGB } else { WTSAT_ARGB };
        }
        Ok(())
    }
}

/// The file name the stream reports (Explorer's file streams carry it), for formats told apart
/// only by their extension; empty when there is none.
fn stream_name(stream: &IStream) -> String {
    let mut stat = STATSTG::default();
    if unsafe { stream.Stat(&mut stat, STATFLAG_DEFAULT) }.is_err() || stat.pwcsName.is_null() {
        return String::new();
    }
    let name = unsafe { stat.pwcsName.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void)) };
    name
}

/// A COM stream as a Rust reader.
struct StreamReader(IStream);

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let mut read = 0u32;
        // S_FALSE with fewer bytes is the end of the stream
        let hr = unsafe { self.0.Read(buf.as_mut_ptr().cast(), len, Some(&mut read)) };
        if hr.is_err() {
            return Err(std::io::Error::other(windows::core::Error::from(hr)));
        }
        Ok(read as usize)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let (offset, origin) = match pos {
            SeekFrom::Start(o) => {
                (i64::try_from(o).map_err(|_| std::io::Error::other("seek offset too large"))?, STREAM_SEEK_SET)
            }
            SeekFrom::Current(o) => (o, STREAM_SEEK_CUR),
            SeekFrom::End(o) => (o, STREAM_SEEK_END),
        };
        let mut at = 0u64;
        unsafe { self.0.Seek(offset, origin, Some(&mut at)) }.map_err(std::io::Error::other)?;
        Ok(at)
    }
}

/// A top-down 32-bit DIB section of `image` (8-bit RGBA): BGRA with straight alpha, as
/// `WTSAT_ARGB` takes it.
fn bitmap(image: &ImageBuf) -> Result<HBITMAP> {
    if image.format() != PixelFormat::Rgba8 {
        return Err(E_INVALIDARG.into());
    }
    let (w, h) = (image.width(), image.height());
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        // negative: rows top to bottom
        biHeight: -(h as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let info = BITMAPINFO { bmiHeader: header, ..Default::default() };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(bitmap.into());
        }
        return Err(E_OUTOFMEMORY.into());
    }
    let len = w as usize * h as usize * 4;
    let out = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), len) };
    for (o, p) in out.as_chunks_mut::<4>().0.iter_mut().zip(image.data().as_chunks::<4>().0) {
        *o = [p[2], p[1], p[0], p[3]];
    }
    Ok(bitmap)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use windows::Win32::Graphics::Gdi::{BITMAP, GetObjectW};
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
    use windows::core::HSTRING;

    use super::*;

    /// Goes the way Explorer's surrogate does: class object by class id, an instance, the
    /// file's stream, then the picture.
    fn provider() -> IThumbnailProvider {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut factory: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { DllGetClassObject(&CLSID, &IClassFactory::IID, &mut factory) };
        assert_eq!(hr, windows::Win32::Foundation::S_OK);
        let factory = unsafe { IClassFactory::from_raw(factory) };
        unsafe { factory.CreateInstance::<Option<&IUnknown>, IThumbnailProvider>(None) }.unwrap()
    }

    fn stream(path: &Path) -> IStream {
        // STGM_READ | STGM_SHARE_DENY_NONE, as the shell opens files for thumbnails
        unsafe { SHCreateStreamOnFileEx(&HSTRING::from(path), 0x40, 0, false, None) }.unwrap()
    }

    /// Size and BGRA pixels of a DIB section.
    fn pixels(bitmap: HBITMAP) -> (i32, i32, Vec<u8>) {
        let mut bm = BITMAP::default();
        let n = unsafe { GetObjectW(bitmap.into(), size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast())) };
        assert!(n > 0);
        assert_eq!(bm.bmBitsPixel, 32);
        let bytes = unsafe {
            std::slice::from_raw_parts(bm.bmBits.cast::<u8>(), (bm.bmWidthBytes * bm.bmHeight) as usize).to_vec()
        };
        (bm.bmWidth, bm.bmHeight, bytes)
    }

    fn thumbnail_of(path: &Path, cx: u32) -> Result<(i32, i32, Vec<u8>, WTS_ALPHATYPE)> {
        let p = provider();
        let init: IInitializeWithStream = p.cast().unwrap();
        unsafe { init.Initialize(&stream(path), 0) }?;
        let (mut bitmap, mut alpha) = (HBITMAP::default(), WTS_ALPHATYPE::default());
        unsafe { p.GetThumbnail(cx, &mut bitmap, &mut alpha) }?;
        let (w, h, px) = pixels(bitmap);
        unsafe {
            let _ = DeleteObject(bitmap.into());
        }
        Ok((w, h, px, alpha))
    }

    #[test]
    fn a_file_stream_becomes_a_top_down_bgra_bitmap() {
        let dir = tempfile::tempdir().unwrap();
        // TGA has no signature: this also shows the stream's name reaches the decoders
        let tga = dir.path().join("halves.tga");
        image::RgbaImage::from_fn(400, 100, |x, y| match (x < 200, y < 50) {
            (true, true) => image::Rgba([255, 0, 0, 255]),
            (false, true) => image::Rgba([0, 0, 255, 255]),
            _ => image::Rgba([0, 255, 0, 255]),
        })
        .save(&tga)
        .unwrap();
        let (w, h, px, alpha) = thumbnail_of(&tga, 200).unwrap();
        assert_eq!((w, h), (200, 50));
        assert_eq!(alpha, WTSAT_RGB, "opaque");
        let at = |x: usize, y: usize| &px[(y * 200 + x) * 4..][..4];
        assert_eq!(at(5, 5), [0, 0, 255, 255], "red, as BGRA, in the top row");
        assert_eq!(at(194, 5), [255, 0, 0, 255], "blue on the right");
        assert_eq!(at(100, 45), [0, 255, 0, 255], "green at the bottom");
    }

    #[test]
    fn transparency_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("clear.png");
        image::RgbaImage::from_fn(10, 10, |x, _| image::Rgba([9, 99, 199, if x < 5 { 0 } else { 255 }]))
            .save(&png)
            .unwrap();
        let (w, h, px, alpha) = thumbnail_of(&png, 256).unwrap();
        assert_eq!((w, h, alpha), (10, 10, WTSAT_ARGB));
        assert_eq!(&px[..4], [199, 99, 9, 0], "straight alpha: colour kept under transparency");
        assert_eq!(&px[9 * 4..10 * 4], [199, 99, 9, 255]);
    }

    #[test]
    fn failures_are_errors_and_misuse_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.tga");
        std::fs::write(&junk, b"not an image at all").unwrap();
        assert!(thumbnail_of(&junk, 256).is_err());

        // asked before it has a stream, initialised twice, an unknown class
        let p = provider();
        let (mut bitmap, mut alpha) = (HBITMAP::default(), WTS_ALPHATYPE::default());
        assert_eq!(unsafe { p.GetThumbnail(256, &mut bitmap, &mut alpha) }.unwrap_err().code(), E_UNEXPECTED);
        let init: IInitializeWithStream = p.cast().unwrap();
        unsafe { init.Initialize(&stream(&junk), 0) }.unwrap();
        let again = unsafe { init.Initialize(&stream(&junk), 0) }.unwrap_err();
        assert_eq!(again.code(), ERROR_ALREADY_INITIALIZED.to_hresult());
        let mut out: *mut c_void = std::ptr::null_mut();
        let other = GUID::from_u128(0x1234);
        assert_eq!(unsafe { DllGetClassObject(&other, &IClassFactory::IID, &mut out) }, CLASS_E_CLASSNOTAVAILABLE);
        assert!(out.is_null());
        assert_eq!(DllCanUnloadNow(), S_FALSE);
    }
}
