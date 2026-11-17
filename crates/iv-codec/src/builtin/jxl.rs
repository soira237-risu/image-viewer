//! JPEG XL via `jxl-oxide` (pure Rust). Orientation is applied while rendering, so the reported
//! size is the upright size and `Orientation::Normal` is correct.

use std::io::Read;
use std::sync::Arc;

use image::{DynamicImage, ImageDecoder};
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, Orientation, PixelFormat, Result, Source};
use jxl_oxide::integration::JxlDecoder;
use jxl_oxide::{InitializeResult, JxlImage};

use super::image_rs::{color_traits, to_image_buf};
use crate::decoder::{Decoded, Decoder, DecoderInfo};

const ID: &str = "jxl-oxide";

pub struct JxlOxideDecoder {
    info: DecoderInfo,
}

impl Default for JxlOxideDecoder {
    fn default() -> Self {
        Self { info: DecoderInfo { id: ID, name: "jxl-oxide", formats: &[FormatId::Jxl], priority: 100 } }
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::decode(ID, e)
}

/// Whether the file's image header declares an animation. Reads only as far as the header (and
/// an embedded colour profile or preview frame that comes with it), like [`JxlDecoder::new`],
/// which does not tell.
fn is_animated(src: &Source) -> Result<bool> {
    let mut reader = src.reader()?;
    let mut uninit = JxlImage::builder().build_uninit();
    let mut buf = vec![0u8; 4096];
    // bytes in `buf` the container parser has not taken yet (part of a box header)
    let mut valid = 0;
    loop {
        let read = reader.read(&mut buf[valid..])?;
        if read == 0 {
            return Err(err("the file ends before its image header"));
        }
        valid += read;
        let consumed = uninit.feed_bytes(&buf[..valid]).map_err(err)?;
        buf.copy_within(consumed..valid, 0);
        valid -= consumed;
        match uninit.try_init().map_err(err)? {
            InitializeResult::NeedMoreData(more) => uninit = more,
            InitializeResult::Initialized(image) => return Ok(image.image_header().metadata.animation.is_some()),
        }
    }
}

fn info_from(dec: &mut JxlDecoder<iv_core::SourceReader>, format: FormatId, animated: bool) -> ImageInfo {
    let (width, height) = dec.dimensions();
    let (bits_per_channel, has_alpha) = color_traits(dec.color_type());
    ImageInfo {
        format,
        codec: ID,
        width,
        height,
        // An animated JXL is shown as its first frame, and its frame count is only known after
        // reading every frame: unknown, which saving treats as more than one (writing it back
        // would drop the other frames). A still one has exactly one.
        frame_count: if animated { None } else { Some(1) },
        has_alpha,
        bits_per_channel,
        orientation: Orientation::Normal,
        icc_profile: dec.icc_profile().ok().flatten().map(Arc::from),
        exif: None,
    }
}

impl Decoder for JxlOxideDecoder {
    fn info(&self) -> &DecoderInfo {
        &self.info
    }

    fn probe(&self, src: &Source, format: FormatId) -> Result<ImageInfo> {
        let mut dec = JxlDecoder::new(src.reader()?).map_err(err)?;
        Ok(info_from(&mut dec, format, is_animated(src)?))
    }

    fn decode(&self, src: &Source, format: FormatId, cancel: &CancelToken) -> Result<Decoded> {
        cancel.check()?;
        let mut dec = JxlDecoder::new(src.reader()?).map_err(err)?;
        let info = info_from(&mut dec, format, is_animated(src)?);
        let out = match info.bits_per_channel {
            16 => PixelFormat::Rgba16,
            32 => PixelFormat::RgbaF32,
            _ => PixelFormat::Rgba8,
        };
        ImageBuf::byte_len(info.width, info.height, out)?;
        let img = DynamicImage::from_decoder(dec).map_err(err)?;
        cancel.check()?;
        let image = to_image_buf(img)?;
        cancel.check()?;
        Ok(Decoded::still(info, image))
    }
}

/// The JPEG file a JPEG XL was made from by lossless recompression, rebuilt from the DCT
/// coefficients and the reconstruction data (`jbrd` box) it carries: the same bytes as the
/// original. With it comes the orientation the JPEG XL is shown in (its own header, which libjxl
/// took from the JPEG's EXIF). `Ok(None)` when the file carries no usable reconstruction data:
/// it was encoded from pixels, or the data is damaged.
pub fn rebuild_jpeg(src: &Source) -> Result<Option<(Vec<u8>, Orientation)>> {
    let image = JxlImage::builder().read(src.reader()?).map_err(err)?;
    if image.jpeg_reconstruction_status() != jxl_oxide::JpegReconstructionStatus::Available {
        return Ok(None);
    }
    let mut jpeg = Vec::new();
    // "Available" can still fail on inconsistent data: that is no reconstruction data either
    if image.reconstruct_jpeg(&mut jpeg).is_err() {
        return Ok(None);
    }
    Ok(Some((undouble_comment_markers(jpeg), Orientation::from_exif(image.image_header().metadata.orientation))))
}

/// Undoes a jxl-jbr 0.2.1 slip: it writes every comment (COM) segment as `FF FE FE <length>`,
/// because libjxl keeps the marker byte at the start of a stored comment and jxl-jbr writes it
/// once more before it. Any other part of the file is copied as it is. The extra byte is removed
/// only where the segment read without it ends exactly at the next marker, so a comment whose
/// length really starts with 0xFE (64 KiB long) is left alone.
fn undouble_comment_markers(jpeg: Vec<u8>) -> Vec<u8> {
    let be16 = |at: usize| jpeg.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize);
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return jpeg;
    }
    let mut out = Vec::with_capacity(jpeg.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut pos = 2;
    while pos + 1 < jpeg.len() && jpeg[pos] == 0xFF {
        let marker = jpeg[pos + 1];
        match marker {
            // fill byte before a marker
            0xFF => {
                out.push(0xFF);
                pos += 1;
                continue;
            }
            // end of image: the rest (and anything appended after it) as it is
            0xD9 => break,
            0x01 | 0xD0..=0xD7 => {
                out.extend_from_slice(&jpeg[pos..pos + 2]);
                pos += 2;
                continue;
            }
            _ => {}
        }
        if marker == 0xFE
            && jpeg.get(pos + 2) == Some(&0xFE)
            && let Some(len) = be16(pos + 3).filter(|l| *l >= 2)
            && jpeg.get(pos + 3 + len) == Some(&0xFF)
        {
            out.extend_from_slice(&jpeg[pos..pos + 2]);
            out.extend_from_slice(&jpeg[pos + 3..pos + 3 + len]);
            pos += 3 + len;
            continue;
        }
        let Some(end) = be16(pos + 2).map(|l| pos + 2 + l).filter(|e| *e <= jpeg.len()) else { break };
        out.extend_from_slice(&jpeg[pos..end]);
        pos = end;
        if marker == 0xDA {
            // entropy-coded data runs to the next marker that is not a stuffed zero or a restart
            let start = pos;
            while pos + 1 < jpeg.len() && !(jpeg[pos] == 0xFF && !matches!(jpeg[pos + 1], 0x00 | 0xD0..=0xD7)) {
                pos += 1;
            }
            out.extend_from_slice(&jpeg[start..pos]);
        }
    }
    out.extend_from_slice(&jpeg[pos.min(jpeg.len())..]);
    out
}

#[cfg(test)]
mod tests {
    use super::undouble_comment_markers;

    #[test]
    fn doubled_comment_markers_are_undone_and_nothing_else_changes() {
        let scan = [0xFF, 0xDA, 0, 2, 0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56];
        let good: Vec<u8> =
            [&[0xFF, 0xD8][..], &[0xFF, 0xFE, 0, 4, b'h', b'i'], &scan, &[0xFF, 0xFE, 0, 3, b'!'], &[0xFF, 0xD9, 7, 7]]
                .concat();
        assert_eq!(undouble_comment_markers(good.clone()), good, "a well-formed file is copied");
        let doubled: Vec<u8> = [
            &[0xFF, 0xD8][..],
            &[0xFF, 0xFE, 0xFE, 0, 4, b'h', b'i'],
            &scan,
            &[0xFF, 0xFE, 0xFE, 0, 3, b'!'],
            &[0xFF, 0xD9, 7, 7],
        ]
        .concat();
        assert_eq!(undouble_comment_markers(doubled), good);
        // a real 0xFExx-byte comment: read without the byte it would not end at a marker
        let mut long = vec![0xFF, 0xD8, 0xFF, 0xFE, 0xFE, 0x10];
        long.extend(std::iter::repeat_n(b'c', 0xFE10 - 2));
        long.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(undouble_comment_markers(long.clone()), long);
    }
}
