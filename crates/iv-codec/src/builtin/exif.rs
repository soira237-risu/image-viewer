//! Helpers for raw EXIF (TIFF-structured) blocks as handed out by decoders.

use exif::{In, Reader, Tag};
use iv_core::Orientation;

/// Parsed EXIF block, if it is well-formed.
pub(crate) fn parse(raw: &[u8]) -> Option<exif::Exif> {
    // some writers keep the APP1 "Exif\0\0" prefix
    let tiff = raw.strip_prefix(b"Exif\0\0").unwrap_or(raw);
    Reader::new().read_raw(tiff.to_vec()).ok()
}

pub(crate) fn orientation(exif: &exif::Exif) -> Orientation {
    exif.get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map_or(Orientation::Normal, Orientation::from_exif)
}

/// The embedded JPEG thumbnail (IFD1 `JPEGInterchangeFormat`), as encoded bytes.
pub(crate) fn jpeg_thumbnail(exif: &exif::Exif) -> Option<&[u8]> {
    let offset = exif.get_field(Tag::JPEGInterchangeFormat, In::THUMBNAIL)?.value.get_uint(0)? as usize;
    let len = exif.get_field(Tag::JPEGInterchangeFormatLength, In::THUMBNAIL)?.value.get_uint(0)? as usize;
    let buf = exif.buf();
    let thumb = buf.get(offset..offset.checked_add(len)?)?;
    thumb.starts_with(&[0xFF, 0xD8]).then_some(thumb)
}
