use iv_core::{CancelToken, FormatId, ImageBuf, ImageInfo, Result, Source};

/// Static description of a decoder.
#[derive(Clone, Debug)]
pub struct DecoderInfo {
    /// Stable identifier, e.g. `"zune-jpeg"`.
    pub id: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    /// Formats this decoder claims.
    pub formats: &'static [FormatId],
    /// Higher runs first. A failing decoder falls through to the next one for the same format.
    pub priority: i32,
}

/// A fully decoded still image plus what was learned while decoding it.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub info: ImageInfo,
    /// Stored pixels, *not* rotated by `info.orientation`; the viewer applies orientation as a
    /// display transform so edits and lossless saves can reason about the stored layout.
    pub image: ImageBuf,
}

/// A decoder for one or more formats.
///
/// Implementations must be cheap to share across threads; per-call state lives on the stack.
pub trait Decoder: Send + Sync {
    fn info(&self) -> &DecoderInfo;

    /// Reads dimensions, frame count, orientation and ICC profile without decoding pixels.
    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo>;

    /// A fast, low-resolution stand-in shown while [`Decoder::decode`] runs: an embedded
    /// thumbnail, a reduced-resolution decode, etc. `target_side` is the longest side the
    /// caller would like; implementations return whatever they can produce cheaply (possibly
    /// smaller) or `None` when they have no shortcut.
    fn preview(
        &self,
        _src: &Source,
        _format: FormatId,
        _target_side: u32,
        _cancel: &CancelToken,
    ) -> Result<Option<ImageBuf>> {
        Ok(None)
    }

    /// Decodes the full-resolution first frame.
    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded>;
}
