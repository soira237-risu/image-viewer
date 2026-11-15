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
    fn rejects_empty_and_oversized() {
        assert!(ImageBuf::new(0, 2, PixelFormat::Rgba8, Vec::<u8>::new()).is_err());
        assert!(matches!(ImageBuf::byte_len(70_000, 70_000, PixelFormat::RgbaF32), Err(Error::TooLarge { .. })));
    }
}
