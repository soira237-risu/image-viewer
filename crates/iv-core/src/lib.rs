//! Core types shared by every image-viewer crate.
//!
//! Nothing in here depends on a codec, a GPU or a window, so the whole pipeline can be driven
//! from the CLI and from tests.

mod cancel;
mod error;
mod format;
mod image;
mod info;
mod source;

pub use cancel::CancelToken;
pub use error::{Error, Result};
pub use format::FormatId;
pub use image::MAX_BUFFER_BYTES;
pub use image::{ImageBuf, PixelFormat};
pub use info::{ImageInfo, Orientation};
pub use source::{Source, SourceReader};
