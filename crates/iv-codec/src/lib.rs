//! Codec layer: format sniffing, the [`Decoder`] trait, the [`Registry`] that picks a decoder per
//! source, and the built-in decoders.

mod builtin {
    pub mod avif;
    pub(crate) mod exif;
    pub mod image_rs;
    pub mod jpeg;
    pub mod jxl;
    pub mod svg;
    #[cfg(windows)]
    pub mod wic;
}
pub mod color;
mod decoder;
pub mod encode;
pub mod exif_edit;
mod registry;
pub mod sniff;
mod webp_container;

use std::sync::Arc;

pub use builtin::avif::Rav1dAvifDecoder;
pub use builtin::image_rs::ImageRsDecoder;
pub use builtin::jpeg::ZuneJpegDecoder;
pub use builtin::jxl::{JxlOxideDecoder, header_orientation as jxl_orientation, rebuild_jpeg as jpeg_from_jxl};
pub use builtin::svg::SvgDecoder;
#[cfg(windows)]
pub use builtin::wic::WicDecoder;
pub use decoder::{Animation, AnimationFrame, Decoded, Decoder, DecoderInfo};
pub use registry::{Identified, Registry};

/// A registry with every built-in decoder registered.
pub fn default_registry() -> Registry {
    let mut r = Registry::new();
    r.register(Arc::new(ZuneJpegDecoder::default()));
    r.register(Arc::new(ImageRsDecoder::default()));
    r.register(Arc::new(Rav1dAvifDecoder::default()));
    r.register(Arc::new(JxlOxideDecoder::default()));
    r.register(Arc::new(SvgDecoder::default()));
    #[cfg(windows)]
    {
        r.register(Arc::new(WicDecoder::primary()));
        r.register(Arc::new(WicDecoder::fallback()));
    }
    r
}
