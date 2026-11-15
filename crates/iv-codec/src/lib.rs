//! Codec layer: format sniffing, the [`Decoder`] trait, the [`Registry`] that picks a decoder per
//! source, and the built-in decoders.

mod builtin {
    pub(crate) mod exif;
    pub mod image_rs;
    pub mod jpeg;
}
mod decoder;
mod registry;
pub mod sniff;

use std::sync::Arc;

pub use builtin::image_rs::ImageRsDecoder;
pub use builtin::jpeg::ZuneJpegDecoder;
pub use decoder::{Decoded, Decoder, DecoderInfo};
pub use registry::{Identified, Registry};

/// A registry with every built-in decoder registered.
pub fn default_registry() -> Registry {
    let mut r = Registry::new();
    r.register(Arc::new(ZuneJpegDecoder::default()));
    r.register(Arc::new(ImageRsDecoder::default()));
    r
}
