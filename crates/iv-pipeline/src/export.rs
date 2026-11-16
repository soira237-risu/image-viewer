//! Saving edits and converting formats: one path for the viewer's Save / Save As, batch
//! conversion and `iv convert`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use iv_codec::encode::{EncodeOptions, Encoders, Metadata, prepare};
use iv_codec::{Registry, exif_edit};
use iv_core::{CancelToken, Error, FormatId, Result, Source};
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
    /// Copy ICC and EXIF into the output where the format allows.
    pub keep_metadata: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportOutcome {
    /// A JPEG that was only rotated or flipped: the EXIF orientation was rewritten and the
    /// compressed image data copied unchanged (no quality loss).
    LosslessOrientation,
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

/// Saves `req`. Rotations/flips of a JPEG saved as JPEG are done losslessly via EXIF; anything
/// else is decoded (in the file's own colour space), edited, encoded and written atomically.
pub fn export(registry: &Registry, encoders: &Encoders, req: &ExportRequest) -> Result<ExportOutcome> {
    let src = crate::archive::source_for(&req.source)?;
    let identified = registry.identify(&src)?;
    let info = registry.probe(&src)?;

    if identified.format == FormatId::Jpeg && req.format == FormatId::Jpeg {
        let g = Geometry::new((info.width, info.height), info.orientation, &req.ops);
        if g.is_orientation_only() {
            let bytes = src.read_all()?;
            if let Some(out) = exif_edit::jpeg_with_orientation(&bytes, g.orientation.to_exif()) {
                write_atomic(&req.dest, |f| f.write_all(&out))?;
                return Ok(ExportOutcome::LosslessOrientation);
            }
        }
    }

    let encoder = encoders
        .for_format(req.format)
        .ok_or_else(|| Error::NoDecoder { format: req.format, reason: "no encoder for this format".into() })?;
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
