//! GPU presentation of decoded images.
//!
//! [`ViewState`] (zoom/pan math) and [`orient`] (EXIF orientation as UV transform) are pure and
//! tested without a GPU; [`ImageRenderer`] uploads an [`iv_core::ImageBuf`] with a mip chain and
//! draws it.

mod gpu;
pub mod orient;
mod renderer;
mod upload;
pub mod view;

pub use gpu::{Gpu, preferred_backends};
pub use renderer::{ImageRenderer, MAX_ADJUSTMENTS, Style, UploadInfo};
pub use view::{ScreenRect, ViewState, ZoomMode};
pub use wgpu;
