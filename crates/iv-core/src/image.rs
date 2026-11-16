use std::sync::Arc;

use crate::{Error, Result};

/// Largest single pixel buffer the pipeline will allocate (4 GiB).
///
/// Beyond this a decode must reduce its output instead of allocating.
pub const MAX_BUFFER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Interleaved RGBA pixel layouts. Every buffer is RGBA so the renderer and the edit
/// operations deal with a single channel order; precision varies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8 bits per channel, straight (non-premultiplied) alpha, sRGB-encoded.
    Rgba8,
    /// 16 bits per channel, native-endian `u16`.
    Rgba16,
    /// 32-bit float per channel, native-endian `f32`, linear for HDR sources.
    RgbaF32,
}

impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgba8 => 4,
            PixelFormat::Rgba16 => 8,
            PixelFormat::RgbaF32 => 16,
        }
    }
}

/// A decoded, immutable image. Cloning is cheap: pixel data is shared through an `Arc`, so the
/// cache, the renderer and the editor can hold the same frame without copying it.
#[derive(Clone)]
pub struct ImageBuf {
    width: u32,
    height: u32,
    format: PixelFormat,
    // `Arc<Vec<u8>>` rather than `Arc<[u8]>`: converting a decoder's `Vec` into `Arc<[u8]>`
    // copies the whole buffer (tens of MB per photo), wrapping it in `Arc` does not.
    data: Arc<Vec<u8>>,
}

impl ImageBuf {
    /// Wraps tightly packed pixel data (`stride == width * bytes_per_pixel`).
    pub fn new(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Result<Self> {
        let data = Arc::new(data);
        let expected = Self::byte_len(width, height, format)?;
        if width == 0 || height == 0 {
            return Err(Error::decode("ImageBuf", format!("empty image {width}x{height}")));
        }
        if data.len() as u64 != expected {
            return Err(Error::decode(
                "ImageBuf",
                format!("buffer is {} bytes, {width}x{height} {format:?} needs {expected}", data.len()),
            ));
        }
        Ok(Self { width, height, format, data })
    }

    /// Bytes needed for a tightly packed buffer, or [`Error::TooLarge`] past [`MAX_BUFFER_BYTES`].
    pub fn byte_len(width: u32, height: u32, format: PixelFormat) -> Result<u64> {
        let bytes = width as u64 * height as u64 * format.bytes_per_pixel() as u64;
        if bytes > MAX_BUFFER_BYTES {
            return Err(Error::TooLarge { width, height, bytes });
        }
        Ok(bytes)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn stride(&self) -> usize {
        self.width as usize * self.format.bytes_per_pixel()
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// An image of `T` samples (`u16` for [`PixelFormat::Rgba16`], `f32` for
    /// [`PixelFormat::RgbaF32`], `u8` for [`PixelFormat::Rgba8`]) written by `fill` straight into
    /// the one buffer the image keeps: no second buffer of samples to copy from.
    pub fn with_samples<T: bytemuck::Pod>(
        width: u32,
        height: u32,
        format: PixelFormat,
        fill: impl FnOnce(&mut [T]),
    ) -> Result<Self> {
        let len = Self::byte_len(width, height, format)? as usize;
        if format.bytes_per_pixel() != 4 * size_of::<T>() {
            return Err(Error::decode("ImageBuf", format!("{format:?} does not hold {}", std::any::type_name::<T>())));
        }
        let mut bytes = vec![0u8; len];
        match bytemuck::try_cast_slice_mut::<u8, T>(&mut bytes) {
            Ok(samples) => fill(samples),
            // an allocation not aligned for `T` (allocators align far more in practice)
            Err(_) => {
                let mut samples = vec![T::zeroed(); len / size_of::<T>()];
                fill(&mut samples);
                bytes.copy_from_slice(bytemuck::cast_slice(&samples));
            }
        }
        Self::new(width, height, format, bytes)
    }

    /// The pixel data, without copying when nothing else shares it.
    pub fn try_into_vec(self) -> std::result::Result<Vec<u8>, Self> {
        let Self { width, height, format, data } = self;
        Arc::try_unwrap(data).map_err(|data| Self { width, height, format, data })
    }

    /// Writes pixels `first..first + out.len() / 4` as 8-bit RGBA (see [`Self::to_rgba8`]), so
    /// large images can be converted in parallel slices.
    pub fn rgba8_into(&self, first: usize, out: &mut [u8]) {
        let n = out.len();
        match self.format {
            PixelFormat::Rgba8 => out.copy_from_slice(&self.data[first * 4..first * 4 + n]),
            PixelFormat::Rgba16 => {
                let src: &[u16] = &bytemuck::cast_slice(&self.data)[first * 4..first * 4 + n];
                for (o, &v) in out.iter_mut().zip(src) {
                    *o = ((v as u32 + 128) / 257) as u8;
                }
            }
            PixelFormat::RgbaF32 => {
                let srgb = |c: f32| if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
                let enc = |c: f32| (srgb(c.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
                let src: &[f32] = &bytemuck::cast_slice(&self.data)[first * 4..first * 4 + n];
                for (o, p) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    o.copy_from_slice(&[enc(p[0]), enc(p[1]), enc(p[2]), (p[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8]);
                }
            }
        }
    }

    /// The pixels as 8-bit RGBA: borrowed when already 8-bit, rounded from 16-bit, and sRGB-encoded
    /// and clipped from (linear) float.
    pub fn to_rgba8(&self) -> std::borrow::Cow<'_, [u8]> {
        use std::borrow::Cow;
        match self.format {
            PixelFormat::Rgba8 => Cow::Borrowed(&self.data),
            _ => {
                let mut out = vec![0u8; self.width as usize * self.height as usize * 4];
                self.rgba8_into(0, &mut out);
                Cow::Owned(out)
            }
        }
    }

    /// Whether every pixel is fully opaque.
    pub fn is_opaque(&self) -> bool {
        match self.format {
            PixelFormat::Rgba8 => self.data.chunks_exact(4).all(|p| p[3] == u8::MAX),
            PixelFormat::Rgba16 => {
                bytemuck::cast_slice::<u8, u16>(&self.data).chunks_exact(4).all(|p| p[3] == u16::MAX)
            }
            PixelFormat::RgbaF32 => bytemuck::cast_slice::<u8, f32>(&self.data).chunks_exact(4).all(|p| p[3] >= 1.0),
        }
    }

    /// Size of the pixel data in bytes (what a cache charges for this image).
    pub fn memory_size(&self) -> usize {
        self.data.len()
    }
}

impl std::fmt::Debug for ImageBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageBuf")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mismatched_length() {
        assert!(ImageBuf::new(2, 2, PixelFormat::Rgba8, vec![0u8; 15]).is_err());
        let img = ImageBuf::new(2, 2, PixelFormat::Rgba8, vec![0u8; 16]).unwrap();
        assert_eq!(img.stride(), 8);
        assert_eq!(img.memory_size(), 16);
    }

    #[test]
    fn samples_are_written_in_place_and_converted_in_slices() {
        let img = ImageBuf::with_samples::<u16>(2, 1, PixelFormat::Rgba16, |s| {
            s.copy_from_slice(&[0, 257, 65535, 65535, 32896, 128, 127, 65535]);
        })
        .unwrap();
        assert_eq!(bytemuck::cast_slice::<u8, u16>(img.data())[1], 257);
        assert_eq!(&*img.to_rgba8(), &[0, 1, 255, 255, 128, 0, 0, 255]);
        let mut second = [0u8; 4];
        img.rgba8_into(1, &mut second);
        assert_eq!(second, [128, 0, 0, 255]);
        assert!(ImageBuf::with_samples::<u16>(2, 1, PixelFormat::RgbaF32, |_| {}).is_err(), "wrong sample type");

        let shared = img.clone();
        let img = img.try_into_vec().expect_err("shared with `shared`");
        drop(shared);
        assert_eq!(img.try_into_vec().unwrap().len(), 16);
    }

    #[test]
    fn rejects_empty_and_oversized() {
        assert!(ImageBuf::new(0, 2, PixelFormat::Rgba8, Vec::<u8>::new()).is_err());
        assert!(matches!(ImageBuf::byte_len(70_000, 70_000, PixelFormat::RgbaF32), Err(Error::TooLarge { .. })));
    }
}
