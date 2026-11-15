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

/// A fully decoded image plus what was learned while decoding it.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub info: ImageInfo,
    /// Stored pixels of the first frame, *not* rotated by `info.orientation`; the viewer applies
    /// orientation as a display transform so edits and lossless saves can reason about the stored
    /// layout.
    pub image: ImageBuf,
    /// Every frame of an animated image (the first is `image` again, sharing its pixels).
    pub animation: Option<Animation>,
}

impl Decoded {
    pub fn still(info: ImageInfo, image: ImageBuf) -> Self {
        Self { info, image, animation: None }
    }

    /// Pixel memory held, counting every animation frame once.
    pub fn memory_size(&self) -> usize {
        match &self.animation {
            Some(a) => a.frames.iter().map(|f| f.image.memory_size()).sum(),
            None => self.image.memory_size(),
        }
    }
}

/// Fully composited animation frames (each one a complete canvas, ready to show), played in a loop.
#[derive(Clone, Debug)]
pub struct Animation {
    pub frames: Vec<AnimationFrame>,
}

#[derive(Clone, Debug)]
pub struct AnimationFrame {
    pub image: ImageBuf,
    pub delay: std::time::Duration,
}

/// A decoder for one or more formats.
///
/// Implementations must be cheap to share across threads; per-call state lives on the stack.
pub trait Decoder: Send + Sync {
    fn info(&self) -> &DecoderInfo;

    /// Whether this decoder wants this particular file, beyond its format. Lets a decoder take
    /// only the files it is better at (e.g. AVIF with alpha) and leave the rest to the next one.
    fn claims(&self, _src: &Source, _format: FormatId) -> Result<bool> {
        Ok(true)
    }

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
