//! Saving edits and converting formats: one path for the viewer's Save / Save As, batch
//! conversion and `iv convert`.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use iv_codec::encode::{EncodeOptions, Encoder, Encoders, Metadata, prepare};
use iv_codec::{Registry, exif_edit};
use iv_core::{CancelToken, Error, FormatId, Orientation, Result, Source};
use iv_ops::{Geometry, Op, apply};

/// What to save.
#[derive(Clone, Debug)]
pub struct ExportRequest {
    pub source: PathBuf,
    /// Edits made in the viewer (may be empty: plain conversion).
    pub ops: Vec<Op>,
    pub format: FormatId,
    pub options: EncodeOptions,
    pub dest: PathBuf,
    /// Colour behind transparent pixels for formats without alpha.
    pub background: [u8; 3],
    /// Copy ICC and EXIF into the output where the format allows. When off, nothing of the
    /// source's metadata is written, on the lossless JPEG rotation too (only the orientation
    /// tag it needs).
    pub keep_metadata: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportOutcome {
    /// A JPEG that was only rotated or flipped: the EXIF orientation was rewritten and the
    /// compressed image data copied unchanged (no quality loss).
    LosslessOrientation,
    /// A JPEG (or the JPEG a JPEG XL was made from by this same recompression) stored as JPEG XL
    /// without decoding it: its DCT coefficients are kept as they are, with the data to rebuild
    /// the original file (no quality loss). Rotations and flips are in the orientation.
    JpegRecompressed { bytes: usize },
    /// The JPEG rebuilt from a JPEG XL's reconstruction data: the same bytes as the file it was
    /// made from, unless a rotation or flip (in the EXIF orientation) or dropping the metadata
    /// changed them; the compressed image data is untouched either way. The data carries no
    /// checksum, so a file damaged in a way that still parses rebuilds into another JPEG.
    JpegRebuilt { bytes: usize },
    /// Decoded, edited and encoded.
    Encoded {
        bytes: usize,
        /// Animation frames that were not written (only the first frame is saved).
        dropped_frames: usize,
    },
}

/// Numbers the temporary files of one process, so two writes never share one.
static TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

/// A new temporary file next to `dest` that no other writer uses: the name carries the process
/// id and a per-process serial number, and the file must not exist yet.
fn create_temp(dir: &Path, name: &str) -> std::io::Result<(PathBuf, File)> {
    loop {
        let serial = TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}-{serial}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            // left behind by a killed process that had the same id: take the next number
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Writes `dest` so that it is either fully the new content or untouched: the data goes to a
/// temporary file of its own in the same folder, is flushed to disk, then renamed over `dest`.
/// A failure in `write` leaves no temporary file and the old `dest` intact. Concurrent writes
/// to the same `dest` (two saves, a batch conversion with two inputs for one output) each use
/// their own temporary file, so `dest` ends up as one complete result, never a mix.
pub fn write_atomic(dest: &Path, write: impl FnOnce(&mut File) -> std::io::Result<()>) -> Result<()> {
    let dir = dest.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = dest.file_name().ok_or_else(|| Error::Io(std::io::Error::other("destination has no file name")))?;
    let (tmp, mut f) = create_temp(dir, &name.to_string_lossy())?;
    let result = (|| {
        write(&mut f)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(Error::Io)
}

/// `jpeg`, whose EXIF orientation is `stored`, made to show as `target` and stripped of its
/// metadata unless `keep_metadata`, with its compressed image data untouched. Without metadata
/// the only thing written besides the image is the orientation tag, when it is not the default.
/// Kept as it is when nothing changes. `None` when the file cannot be taken apart, or has EXIF
/// without an orientation tag to rewrite: the caller re-encodes.
fn jpeg_shown_as(jpeg: &[u8], stored: Orientation, target: Orientation, keep_metadata: bool) -> Option<Cow<'_, [u8]>> {
    if keep_metadata {
        if stored == target {
            return Some(Cow::Borrowed(jpeg));
        }
        return exif_edit::jpeg_with_orientation(jpeg, target.to_exif()).map(Cow::Owned);
    }
    let bare = exif_edit::jpeg_without_metadata(jpeg)?;
    if target == Orientation::Normal {
        return Some(Cow::Owned(bare));
    }
    exif_edit::jpeg_with_orientation(&bare, target.to_exif()).map(Cow::Owned)
}

/// `jpeg` recompressed losslessly as a JPEG XL shown as `target`. libjxl takes the orientation
/// from the JPEG's EXIF, but reads it more narrowly than the viewer does: only the first EXIF
/// segment, and only a SHORT tag. So the result's own header is checked, and when it says
/// otherwise (a LONG tag, two EXIF segments that disagree) every orientation tag of the JPEG is
/// rewritten in the form libjxl reads and it is recompressed once more. `None` when the encoder
/// does not recompress (a pixel encode was asked for), refuses this JPEG, or still disagrees:
/// the caller encodes the pixels.
fn recompress_as(encoder: &dyn Encoder, jpeg: &[u8], target: Orientation, opts: &EncodeOptions) -> Option<Vec<u8>> {
    let shown_right = |jxl: &[u8]| iv_codec::jxl_orientation(jxl).ok() == Some(target);
    let out = encoder.recompress_jpeg(jpeg, opts)?.ok()?;
    if shown_right(&out) {
        return Some(out);
    }
    let canonical = exif_edit::jpeg_with_orientation(jpeg, target.to_exif())?;
    let out = encoder.recompress_jpeg(&canonical, opts)?.ok()?;
    shown_right(&out).then_some(out)
}

/// The original JPEG rebuilt from a JPEG XL made by lossless recompression, made to show as the
/// JPEG XL does after `ops` (only rotations and flips) and stripped of its metadata unless
/// `keep_metadata`, with the orientation that is. `None` when the file carries no reconstruction
/// data, or the rebuilt file does not read: the caller decodes.
fn rebuilt_jpeg(
    registry: &Registry,
    src: &Source,
    size: (u32, u32),
    ops: &[Op],
    keep_metadata: bool,
) -> Option<(Vec<u8>, Orientation)> {
    // a file it cannot read is decoded by the caller, which reports the error
    let (jpeg, shown) = iv_codec::jpeg_from_jxl(src).ok()??;
    // the edits apply to the image as shown, which is the JPEG under the JPEG XL's orientation;
    // the rebuilt file's own EXIF says how it is stored
    let target = Geometry::new(size, shown, ops).orientation;
    let jpeg: std::sync::Arc<[u8]> = jpeg.into();
    let stored = registry.probe(&Source::from_bytes("rebuilt.jpg", jpeg.clone())).ok()?.orientation;
    let out = jpeg_shown_as(&jpeg, stored, target, keep_metadata)?.into_owned();
    Some((out, target))
}

/// Saves `req`, without decoding where that loses nothing:
/// - a JPEG only rotated or flipped and saved as JPEG: the EXIF orientation is rewritten;
/// - a JPEG only rotated or flipped and saved as a format that can hold its coefficients (JPEG
///   XL): recompressed losslessly, unless the encoder options ask for a pixel encode;
/// - a JPEG XL made that way, saved as JPEG: the original JPEG is rebuilt;
/// - such a JPEG XL only rotated or flipped and saved as JPEG XL again (the viewer's Save of it):
///   the original JPEG is rebuilt, turned in its EXIF and recompressed, so the file stays as small
///   and as able to give the JPEG back as it was.
///
/// Anything else, and a file one of those paths refuses (a JPEG libjxl cannot take, such as
/// arithmetic coding or CMYK), is decoded (in the file's own colour space), edited, encoded and
/// written atomically.
pub fn export(registry: &Registry, encoders: &Encoders, req: &ExportRequest) -> Result<ExportOutcome> {
    let src = crate::archive::source_for(&req.source)?;
    let identified = registry.identify(&src)?;
    let info = registry.probe(&src)?;
    let encoder = encoders
        .for_format(req.format)
        .ok_or_else(|| Error::NoDecoder { format: req.format, reason: "no encoder for this format".into() })?;
    let g = Geometry::new((info.width, info.height), info.orientation, &req.ops);

    // the formats that can take a JPEG without decoding it: JPEG itself and JPEG XL
    let lossless_target = matches!(req.format, FormatId::Jpeg | FormatId::Jxl);
    if identified.format == FormatId::Jpeg && lossless_target && g.is_orientation_only() {
        let bytes = src.read_all()?;
        // a file that cannot be taken apart is re-encoded below, which writes no metadata
        // unless asked either
        if let Some(jpeg) = jpeg_shown_as(&bytes, info.orientation, g.orientation, req.keep_metadata) {
            if req.format == FormatId::Jpeg {
                write_atomic(&req.dest, |f| f.write_all(&jpeg))?;
                return Ok(ExportOutcome::LosslessOrientation);
            }
            if let Some(out) = recompress_as(encoder.as_ref(), &jpeg, g.orientation, &req.options) {
                write_atomic(&req.dest, |f| f.write_all(&out))?;
                return Ok(ExportOutcome::JpegRecompressed { bytes: out.len() });
            }
        }
    }

    if identified.format == FormatId::Jxl && lossless_target && g.is_orientation_only() {
        let rebuilt = rebuilt_jpeg(registry, &src, (info.width, info.height), &req.ops, req.keep_metadata);
        if let Some((jpeg, target)) = rebuilt {
            if req.format == FormatId::Jpeg {
                write_atomic(&req.dest, |f| f.write_all(&jpeg))?;
                return Ok(ExportOutcome::JpegRebuilt { bytes: jpeg.len() });
            }
            if let Some(out) = recompress_as(encoder.as_ref(), &jpeg, target, &req.options) {
                write_atomic(&req.dest, |f| f.write_all(&out))?;
                return Ok(ExportOutcome::JpegRecompressed { bytes: out.len() });
            }
        }
    }

    let iv_codec::Decoded { info, image, animation, .. } = registry.decode_unmanaged(&src, &CancelToken::never())?;
    drop(src);
    let dropped_frames = animation.map_or(0, |a| a.frames.len().saturating_sub(1));
    let g = Geometry::new((image.width(), image.height()), info.orientation, &req.ops);
    let edited = apply::render(&image, &g)?;
    // only the metadata is needed from here on: the decoded pixels are freed before the
    // encoder's copy is made
    drop(image);

    let meta = if req.keep_metadata {
        Metadata {
            icc: info.icc_profile.as_ref().map(|p| p.to_vec()),
            exif: info.exif.as_ref().map(|e| {
                let mut e = e.to_vec();
                // the orientation is now in the pixels, and the old thumbnail shows the old image
                exif_edit::set_orientation(&mut e, 1);
                exif_edit::drop_thumbnail(&mut e);
                e
            }),
        }
    } else {
        Metadata::default()
    };
    let prepared = prepare(&edited, encoder.info(), req.background);
    drop(edited);
    let bytes = encoder.encode(&prepared, &req.options, &meta)?;
    drop(prepared);
    write_atomic(&req.dest, |f| f.write_all(&bytes))?;
    Ok(ExportOutcome::Encoded { bytes: bytes.len(), dropped_frames })
}

/// The file name with its extension replaced by the canonical one for `format`.
pub fn with_extension(path: &Path, format: FormatId) -> PathBuf {
    path.with_extension(format.canonical_extension())
}

/// Renames a file whose extension does not match its content (a WebP called `.jpg`) to the
/// right extension, without touching its bytes. Returns the new path, or `None` when the
/// extension already fits. Never overwrites an existing file.
pub fn fix_extension(registry: &Registry, path: &Path) -> Result<Option<PathBuf>> {
    if crate::archive::split(path).is_some() {
        return Err(Error::Io(std::io::Error::other("files inside an archive cannot be renamed")));
    }
    let id = registry.identify(&Source::from_path(path))?;
    if !id.extension_mismatch() {
        return Ok(None);
    }
    let target = with_extension(path, id.format);
    if target.exists() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", target.display()),
        )));
    }
    std::fs::rename(path, &target)?;
    Ok(Some(target))
}
