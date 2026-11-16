use std::sync::Arc;

use iv_core::panic::catch;
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Result, Source};

use crate::decoder::{Decoded, Decoder};
use crate::sniff::{SNIFF_LEN, sniff};

/// How a source's format was determined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identified {
    pub format: FormatId,
    /// `true` when the content signature decided the format, `false` when only the extension did.
    pub by_content: bool,
    /// The format the file's extension claims, if it names a known format.
    pub extension_format: Option<FormatId>,
}

impl Identified {
    /// The content says one format and the extension another (e.g. a WebP named `.jpg`).
    pub fn extension_mismatch(&self) -> bool {
        self.by_content && self.extension_format.is_some_and(|ext| ext != self.format)
    }
}

/// Holds the registered decoders and picks one per source.
///
/// Selection: identify the format (content first, extension as fallback), then try every
/// decoder claiming that format in descending priority (registration order breaks ties). A
/// decoder that errors falls through to the next; cancellation stops the chain at once. A
/// decoder that panics on a malformed file counts as one that errored ([`Error::Panicked`]), so
/// the thread calling the registry survives it.
#[derive(Default)]
pub struct Registry {
    decoders: Vec<Arc<dyn Decoder>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, decoder: Arc<dyn Decoder>) {
        assert!(
            !self.decoders.iter().any(|d| d.info().id == decoder.info().id),
            "decoder '{}' registered twice",
            decoder.info().id
        );
        // stable sort keeps registration order among equal priorities
        self.decoders.push(decoder);
        self.decoders.sort_by_key(|d| std::cmp::Reverse(d.info().priority));
    }

    /// Registered decoders in selection order.
    pub fn decoders(&self) -> impl Iterator<Item = &Arc<dyn Decoder>> {
        self.decoders.iter()
    }

    /// Decoders that claim `format`, in the order they will be tried.
    pub fn decoders_for(&self, format: FormatId) -> impl Iterator<Item = &Arc<dyn Decoder>> {
        self.decoders.iter().filter(move |d| d.info().formats.contains(&format))
    }

    /// Formats at least one registered decoder claims.
    pub fn supported_formats(&self) -> Vec<FormatId> {
        FormatId::ALL.into_iter().filter(|f| self.decoders_for(*f).next().is_some()).collect()
    }

    /// Determines the format of `src` from its content, falling back to its extension.
    pub fn identify(&self, src: &Source) -> Result<Identified> {
        let head = src.read_head(SNIFF_LEN)?;
        let extension_format = src.extension().as_deref().and_then(FormatId::from_extension);
        match sniff(&head) {
            // DNG, NEF, CR2, ARW … are TIFF containers; the RAW extension tells them apart
            Some(FormatId::Tiff) if extension_format == Some(FormatId::Raw) => {
                Ok(Identified { format: FormatId::Raw, by_content: true, extension_format })
            }
            Some(format) => Ok(Identified { format, by_content: true, extension_format }),
            None => match extension_format {
                Some(format) => Ok(Identified { format, by_content: false, extension_format }),
                None => Err(Error::UnsupportedFormat),
            },
        }
    }

    /// Probes with the first decoder that succeeds.
    pub fn probe(&self, src: &Source) -> Result<ImageInfo> {
        let id = self.identify(src)?;
        self.try_each(src, id.format, |d| d.probe(src, id.format))
    }

    /// Decodes with the first decoder that succeeds. Pixels with an embedded RGB ICC profile come
    /// back converted to sRGB (`Decoded::color_managed`); `info.icc_profile` still describes the
    /// file.
    /// Decodes without colour management: pixels stay in the file's colour space, described by
    /// `info.icc_profile`. Used when saving, so a wide-gamut photo keeps its gamut.
    pub fn decode_unmanaged(&self, src: &Source, cancel: &CancelToken) -> Result<Decoded> {
        let id = self.identify(src)?;
        self.try_each(src, id.format, |d| d.decode(src, id.format, cancel))
    }

    pub fn decode(&self, src: &Source, cancel: &CancelToken) -> Result<Decoded> {
        let id = self.identify(src)?;
        let mut decoded = self.try_each(src, id.format, |d| d.decode(src, id.format, cancel))?;
        cancel.check()?;
        decoded.color_managed = catch("colour management", || Ok(crate::color::to_srgb(&mut decoded).is_ok()))?;
        Ok(decoded)
    }

    /// The first preview any decoder for the format can produce cheaply.
    pub fn preview(&self, src: &Source, target_side: u32, cancel: &CancelToken) -> Result<Option<ImageBuf>> {
        let id = self.identify(src)?;
        for d in self.decoders_for(id.format) {
            cancel.check()?;
            let codec = d.info().id;
            if !catch(codec, || d.claims(src, id.format)).unwrap_or(false) {
                continue;
            }
            match catch(codec, || d.preview(src, id.format, target_side, cancel)) {
                Ok(Some(img)) => return Ok(Some(img)),
                Ok(None) => {}
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                // a broken embedded thumbnail must not block the real decode
                Err(_) => {}
            }
        }
        Ok(None)
    }

    fn try_each<T>(&self, src: &Source, format: FormatId, mut f: impl FnMut(&dyn Decoder) -> Result<T>) -> Result<T> {
        let mut failures = Vec::new();
        for d in self.decoders_for(format) {
            let codec = d.info().id;
            match catch(codec, || d.claims(src, format)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    failures.push(format!("{}: {e}", d.info().id));
                    continue;
                }
            }
            match catch(codec, || f(d.as_ref())) {
                Ok(v) => return Ok(v),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => failures.push(format!("{}: {e}", d.info().id)),
            }
        }
        let reason = if failures.is_empty() { "no decoder registered".to_string() } else { failures.join("; ") };
        Err(Error::NoDecoder { format, reason })
    }
}
